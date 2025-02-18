use std::{collections::HashMap, fmt};

use anyhow::{anyhow, format_err};
use constant_time_eq::constant_time_eq;
use libwally::{package_id::PackageId, package_index::PackageIndex};
use reqwest::{Client, StatusCode};
use rocket::{
    http::Status,
    request::{FromRequest, Outcome},
    Request, State,
};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::{config::Config, error::ApiErrorStatus};

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum AuthMode {
    ApiKey(Vec<String>),
    DoubleApiKey {
        read: Option<Vec<String>>,
        write: Vec<String>,
    },
    GithubOAuth {
        #[serde(rename = "client-id")]
        client_id: String,
        #[serde(rename = "client-secret")]
        client_secret: String,
    },
    Unauthenticated,
}

#[derive(Deserialize)]
pub struct GithubInfo {
    login: String,
    id: u64,
}

impl GithubInfo {
    pub fn login(&self) -> &str {
        &self.login
    }

    pub fn id(&self) -> &u64 {
        &self.id
    }
}

#[derive(Deserialize)]
#[allow(unused)] // Variables are (currently) not accessed but ensure they are present during json parsing
struct ValidatedGithubApp {
    client_id: String,
}

#[derive(Deserialize)]
#[allow(unused)] // Variables are (currently) not accessed but ensure they are present during json parsing
struct ValidatedGithubInfo {
    id: u64,
    app: ValidatedGithubApp,
}

impl fmt::Debug for AuthMode {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AuthMode::ApiKey(_) => write!(formatter, "API key"),
            AuthMode::DoubleApiKey { .. } => write!(formatter, "double API key"),
            AuthMode::GithubOAuth { .. } => write!(formatter, "Github OAuth"),
            AuthMode::Unauthenticated => write!(formatter, "no authentication"),
        }
    }
}

fn match_api_key<T>(request: &Request<'_>, keys: &[String], result: T) -> Outcome<T, Error> {
    let input_api_key: String = match request.headers().get_one("authorization") {
        Some(key) if key.starts_with("Bearer ") => (key[6..].trim()).to_owned(),
        _ => {
            return format_err!("API key required")
                .status(Status::Unauthorized)
                .into();
        }
    };

    if keys.iter().any(|key| constant_time_eq(key.as_bytes(), input_api_key.as_bytes())) {
        Outcome::Success(result)
    } else {
        format_err!("Invalid API key")
            .status(Status::Unauthorized)
            .into()
    }
}

async fn verify_github_token(
    request: &Request<'_>,
    client_id: &str,
    client_secret: &str,
) -> Outcome<WriteAccess, Error> {
    let token: String = match request.headers().get_one("authorization") {
        Some(key) if key.starts_with("Bearer ") => (key[6..].trim()).to_owned(),
        _ => {
            return format_err!("Github auth required")
                .status(Status::Unauthorized)
                .into();
        }
    };

    let client = Client::new();
    let response = client
        .get("https://api.github.com/user")
        .header("accept", "application/json")
        .header("user-agent", "wally")
        .bearer_auth(&token)
        .send()
        .await;

    let github_info = match response {
        Err(err) => {
            return format_err!(err).status(Status::InternalServerError).into();
        }
        Ok(response) => match response.json::<GithubInfo>().await {
            Err(err) => {
                return format_err!("Github auth failed: {}", err)
                    .status(Status::Unauthorized)
                    .into();
            }
            Ok(github_info) => github_info,
        },
    };

    let mut body = HashMap::new();
    body.insert("access_token", &token);

    let response = client
        .post(format!(
            "https://api.github.com/applications/{}/token",
            client_id
        ))
        .header("accept", "application/json")
        .header("user-agent", "wally")
        .basic_auth(client_id, Some(client_secret))
        .json(&body)
        .send()
        .await;

    let validated_github_info = match response {
        Err(err) => {
            return format_err!(err).status(Status::InternalServerError).into();
        }
        Ok(response) => {
            // If a code 422 (unprocessable entity) is returned, it's a sign of
            // auth failure. Otherwise, we don't know what happened!
            // https://docs.github.com/en/rest/apps/oauth-applications#check-a-token--status-codes
            match response.status() {
                StatusCode::OK => response.json::<ValidatedGithubInfo>().await,
                StatusCode::UNPROCESSABLE_ENTITY => {
                    return anyhow!("GitHub auth was invalid")
                        .status(Status::Unauthorized)
                        .into();
                }
                status => {
                    return format_err!("Github auth failed because: {}", status)
                        .status(Status::UnprocessableEntity)
                        .into()
                }
            }
        }
    };

    match validated_github_info {
        Err(err) => format_err!("Github auth failed: {}", err)
            .status(Status::Unauthorized)
            .into(),
        Ok(_) => Outcome::Success(WriteAccess::Github(github_info)),
    }
}

pub enum ReadAccess {
    Public,
    ApiKey,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ReadAccess {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Error> {
        let config = request
            .guard::<&State<Config>>()
            .await
            .expect("AuthMode was not configured");

        match &config.auth {
            AuthMode::Unauthenticated => Outcome::Success(ReadAccess::Public),
            AuthMode::GithubOAuth { .. } => Outcome::Success(ReadAccess::Public),
            AuthMode::ApiKey(key) => match_api_key(request, key, ReadAccess::ApiKey),
            AuthMode::DoubleApiKey { read, .. } => match read {
                None => Outcome::Success(ReadAccess::Public),
                Some(key) => match_api_key(request, key, ReadAccess::ApiKey),
            },
        }
    }
}

pub enum WriteAccess {
    ApiKey,
    Github(GithubInfo),
}

impl WriteAccess {
    pub fn can_write_package(
        &self,
        package_id: &PackageId,
        index: &PackageIndex,
    ) -> anyhow::Result<bool> {
        let scope = package_id.name().scope();

        let has_permission = match self {
            WriteAccess::ApiKey => true,
            WriteAccess::Github(github_info) => {
                match index.is_scope_owner(scope, github_info.id())? {
                    true => true,
                    // Only grant write access if the username matches the scope AND the scope has no existing owners
                    false => {
                        github_info.login().to_lowercase() == scope
                            && index.get_scope_owners(scope)?.is_empty()
                    }
                }
            }
        };

        Ok(has_permission)
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WriteAccess {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Error> {
        let config = request
            .guard::<&State<Config>>()
            .await
            .expect("AuthMode was not configured");

        match &config.auth {
            AuthMode::Unauthenticated => format_err!("Invalid API key for write access")
                .status(Status::Unauthorized)
                .into(),
            AuthMode::ApiKey(key) => match_api_key(request, key, WriteAccess::ApiKey),
            AuthMode::DoubleApiKey { write, .. } => {
                match_api_key(request, write, WriteAccess::ApiKey)
            }
            AuthMode::GithubOAuth {
                client_id,
                client_secret,
            } => verify_github_token(request, client_id, client_secret).await,
        }
    }
}
