import * as path from 'node:path';
import { CorsHttpMethod, HttpApi, HttpMethod } from '@aws-cdk/aws-apigatewayv2-alpha';
import { HttpLambdaIntegration } from '@aws-cdk/aws-apigatewayv2-integrations-alpha';
import { Duration, aws_lambda as lambda } from 'aws-cdk-lib';
import { RustFunction } from 'cargo-lambda-cdk';
import { Construct } from 'constructs';

import type { Parameters } from './parameters';
import type { SessionStore } from './session-store';
import type { UserPool } from './user-pool';

/** Props for `CredentialsApi`. */
export interface CredentialsApiProps {
    /** Base path where tht API is to be served. */
    readonly basePath: string;

    /** Parameters in Parameter Store on AWS Systems Manager. */
    readonly parameters: Parameters;

    /** Session store. */
    readonly sessionStore: SessionStore;

    /** User pool. */
    readonly userPool: UserPool;

    /** Origins allowed to access the API. */
    readonly allowOrigins: string[];
}

/** CDK construct that provisions the Credentials API. */
export class CredentialsApi extends Construct {
    /** Lambda function for registration. */
    readonly registrationLambda: lambda.IFunction;

    /** Lambda function for discoverable credentials. */
    readonly discoverableLambda: lambda.IFunction;

    /** Credentials API. */
    readonly credentialsApi: HttpApi;

    constructor(scope: Construct, id: string, readonly props: CredentialsApiProps) {
        super(scope, id);

        const {
          allowOrigins,
          basePath,
          parameters,
          sessionStore,
          userPool,
        } = props;
        const manifestPath = path.join('lambda', 'authentication', 'Cargo.toml');
        const registrationBasePath = `${basePath.replace(/\/$/, '')}/registration/`;
        const discoverableBasePath = `${basePath.replace(/\/$/, '')}/discoverable/`;

        this.registrationLambda = new RustFunction(this, 'RegistrationLambda', {
            manifestPath,
            binaryName: 'registration',
            architecture: lambda.Architecture.ARM_64,
            environment: {
                BASE_PATH: registrationBasePath,
                SESSION_TABLE_NAME: sessionStore.sessionTable.tableName,
                USER_POOL_ID: userPool.userPool.userPoolId,
                CREDENTIAL_TABLE_NAME: userPool.credentialTable.tableName,
                RP_ORIGIN_PARAMETER_PATH: parameters.rpOriginParameter.parameterName,
            },
            memorySize: 128,
            timeout: Duration.seconds(5),
        });
        parameters.rpOriginParameter.grantRead(this.registrationLambda);
        sessionStore.sessionTable.grantReadWriteData(this.registrationLambda);
        userPool.credentialTable.grantReadWriteData(this.registrationLambda);
        userPool.userPool.grant(
            this.registrationLambda,
            'cognito-idp:AdminCreateUser',
            'cognito-idp:AdminSetUserPassword',
            'cognito-idp:ListUsers',
        );

        this.discoverableLambda = new RustFunction(this, 'DiscoverableLambda', {
            manifestPath,
            binaryName: 'discoverable',
            architecture: lambda.Architecture.ARM_64,
            environment: {
                BASE_PATH: discoverableBasePath,
                SESSION_TABLE_NAME: sessionStore.sessionTable.tableName,
                RP_ORIGIN_PARAMETER_PATH: parameters.rpOriginParameter.parameterName,
            },
            memorySize: 128,
            timeout: Duration.seconds(5),
        });
        parameters.rpOriginParameter.grantRead(this.discoverableLambda);
        sessionStore.sessionTable.grantReadWriteData(this.discoverableLambda);

        this.credentialsApi = new HttpApi(this, 'CredentialsApi', {
            description: 'API to manage credentials',
            createDefaultStage: true,
            corsPreflight: {
                allowHeaders: ['Content-Type'],
                allowMethods: [CorsHttpMethod.POST],
                allowOrigins,
                maxAge: Duration.days(1),
            },
        });
        this.credentialsApi.addRoutes({
            path: `${registrationBasePath}{proxy+}`,
            methods: [HttpMethod.POST],
            integration: new HttpLambdaIntegration('Registration', this.registrationLambda),
        });
        this.credentialsApi.addRoutes({
            path: `${discoverableBasePath}{proxy+}`,
            methods: [HttpMethod.POST],
            integration: new HttpLambdaIntegration('Discoverable', this.discoverableLambda),
        });
    }

    /** Base path of the Credentials API not including the trailing slash. */
    get basePath(): string {
      return this.props.basePath.replace(/\/$/, '');
    }

    /** Internal URL of the Credentials API. */
    get internalUrl(): string {
        return `${this.credentialsApi.defaultStage!.url}${this.props.basePath.replace(/^\//, '')}`;
    }
}
