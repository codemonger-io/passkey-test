//! Registration.
//!
//! You have to configure the following environment variable:
//! - `BASE_PATH`: base path to provide the service; e.g., `/auth/cedentials/`
//! - `SESSION_TABLE_NAME`: name of the DynamoDB table to store sessions
//!
//! ## Endpoints
//!
//! Provides the following endpoints under the base path.
//!
//! ### `POST ${BASE_PATH}start`
//!
//! Starts registration of a new user.
//! The request body must be [`NewUserInfo`] as `application/json`.
//! The response body is [`StartRegistrationSession`] as `application/json`.
//!
//! ### `POST ${BASE_PATH}finish`
//!
//! Verifies the new user and finishes registration.
//! The request body must be [`FinishRegistrationSession`] as `application/json`.
//! The response body is an empty text.

use aws_sdk_dynamodb::{
    primitives::DateTime,
    types::{AttributeValue, ReturnValue},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as base64url};
use lambda_http::{
    Body,
    Error,
    Request,
    RequestExt,
    RequestPayloadExt,
    Response,
    http::StatusCode,
    run,
    service_fn,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::time::SystemTime;
use tracing::{error, info};
use webauthn_rs::{
    WebauthnBuilder,
    prelude::{
        CreationChallengeResponse,
        CredentialID,
        PasskeyRegistration,
        Url,
        Uuid,
    },
};
use webauthn_rs_proto::RegisterPublicKeyCredential;

/// Information on a new user.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewUserInfo {
    /// Username.
    pub username: String,

    /// Display name.
    pub display_name: String,
}

/// Beginning of a session to register a new user.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRegistrationSession {
    /// Session ID.
    pub session_id: String,

    /// Credential creation options.
    pub credential_creation_options: CreationChallengeResponse,
}

/// End of a session to register a new user.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishRegistrationSession {
    /// Session ID.
    pub session_id: String,

    /// Public key credential.
    pub public_key_credential: RegisterPublicKeyCredential,
}

async fn function_handler(event: Request) -> Result<Response<Body>, Error> {
    let base_path = env::var("BASE_PATH")
        .or(Err("BASE_PATH env must be configured"))?;
    let base_path = base_path.trim_end_matches('/');
    let job_path = event.raw_http_path().strip_prefix(base_path)
        .ok_or(format!("path must start with \"{}\"", base_path))?;
    match job_path {
        "/start" => {
            let user_info: NewUserInfo = event
                .payload()?
                .ok_or("missing new user info")?;
            start_registration(user_info).await
        }
        "/finish" => {
            let session: FinishRegistrationSession = event
                .payload()?
                .ok_or("missing registration session")?;
            finish_registration(session).await
        }
        _ => Err(format!("unsupported job path: {}", job_path).into()),
    }
}

async fn start_registration(user_info: NewUserInfo) -> Result<Response<Body>, Error> {
    info!("start_registration: {:?}", user_info);
    // TODO: reuse Webauthn
    let rp_id = "localhost";
    let rp_origin = Url::parse("http://localhost:5173")?;
    let builder = WebauthnBuilder::new(rp_id, &rp_origin)?;
    let builder = builder.rp_name("Passkey Test");
    let webauthn = builder.build()?;

    // TODO: resolve the existing user

    // associates this ID with the new Cognito user later
    let user_unique_id = Uuid::new_v4();

    // TODO: list existing credentials to exclude
    let exclude_credentials: Option<Vec<CredentialID>> = None;

    let res = match webauthn.start_passkey_registration(
        user_unique_id,
        &user_info.username,
        &user_info.display_name,
        exclude_credentials,
    ) {
        Ok((ccr, reg_state)) => {
            // caches `reg_state`
            // TODO: reuse DynamoDB client
            let table_name = env::var("SESSION_TABLE_NAME")?;
            let config = aws_config::load_from_env().await;
            let client = aws_sdk_dynamodb::Client::new(&config);
            let user_unique_id = base64url.encode(user_unique_id.into_bytes());
            let session_id = base64url.encode(Uuid::new_v4().as_ref());
            let ttl = DateTime::from(SystemTime::now()).secs() + 60;
            info!("putting registration session: {}", session_id);
            client.put_item()
                .table_name(table_name)
                .item("pk", AttributeValue::S(format!("registration#{}", session_id)))
                .item("ttl", AttributeValue::N(format!("{}", ttl)))
                .item("userId", AttributeValue::S(user_unique_id))
                .item("userInfo", AttributeValue::M(HashMap::from([
                    ("username".into(), AttributeValue::S(user_info.username.into())),
                    ("displayName".into(), AttributeValue::S(user_info.display_name.into())),
                ])))
                .item("state", AttributeValue::S(serde_json::to_string(&reg_state)?))
                .send()
                .await?;
            serde_json::to_string(&StartRegistrationSession {
                session_id,
                credential_creation_options: ccr,
            })?
        }
        Err(e) => {
            error!("failed to start registration: {}", e);
            return Err("failed to start registration".into());
        }
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(res.into())?)
}

async fn finish_registration(session: FinishRegistrationSession) -> Result<Response<Body>, Error> {
    info!("finish_registration: {}", session.session_id);
    // TODO: reuse Webauthn
    let rp_id = "localhost";
    let rp_origin = Url::parse("http://localhost:5173")?;
    let builder = WebauthnBuilder::new(rp_id, &rp_origin)?;
    let builder = builder.rp_name("Passkey Test");
    let webauthn = builder.build()?;

    // pops the session
    let table_name = env::var("SESSION_TABLE_NAME")?;
    let config = aws_config::load_from_env().await;
    let client = aws_sdk_dynamodb::Client::new(&config);
    let item = client.delete_item()
        .table_name(table_name)
        .key("pk", AttributeValue::S(format!("registration#{}", session.session_id)))
        .return_values(ReturnValue::AllOld)
        .send()
        .await?
        .attributes
        .ok_or("expired or wrong registration session")?;

    // the session may have expired
    let ttl: i64 = item.get("ttl")
        .ok_or("missing ttl")?
        .as_n()
        .or(Err("invalid ttl"))?
        .parse()?;
    if ttl < DateTime::from(SystemTime::now()).secs() {
        return Err("registration session expired".into());
    }

    // extracts the registration state
    let reg_state: PasskeyRegistration = serde_json::from_str(
        item.get("state")
            .ok_or("missing registration state")?
            .as_s()
            .or(Err("invalid state"))?,
    )?;

    // verifies the request
    match webauthn.finish_passkey_registration(
        &session.public_key_credential,
        &reg_state,
    ) {
        Ok(key) => {
            info!("verified key: {:?}", key);
            // extracts the user information
            let user_unique_id = item.get("userId")
                .ok_or("missing userId")?
                .as_s()
                .or(Err("invalid userId"))?;
            // TODO: create Cognito user if necessary
            // TODO: remembers `key` in the database
        }
        Err(e) => {
            error!("failed to finish registration: {}", e);
            return Err("failed to finish registration".into());
        }
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain")
        .body(().into())?)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        // disable printing the name of the module in every log line.
        .with_target(false)
        // disabling time is handy because CloudWatch will add the ingestion time.
        .without_time()
        .init();
    run(service_fn(function_handler)).await
}