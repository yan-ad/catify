//! Shopify Identity device authorization and token exchange adapter.

use crate::{
    Secret, Session,
    flow::{DeviceAuthorization, IdentityProvider},
};
use async_trait::async_trait;
use cfy_core::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::time::sleep;
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityConfig {
    pub client_id: String,
    pub device_authorization_url: Url,
    pub token_url: Url,
    pub scopes: Vec<String>,
    pub max_poll_attempts: u32,
}

#[derive(Clone)]
pub struct HttpIdentityTransport {
    client: reqwest::Client,
}

impl HttpIdentityTransport {
    pub fn new() -> Result<Self> {
        static TLS: OnceLock<std::result::Result<(), String>> = OnceLock::new();
        TLS.get_or_init(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .map_err(|_| "a different Rustls provider is already installed".to_owned())
        })
        .clone()
        .map_err(|error| Error::new(ErrorKind::Config, error))?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("crabpify/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                Error::with_source(
                    ErrorKind::Api,
                    "could not create identity HTTP client",
                    error,
                )
            })?;
        Ok(Self { client })
    }

    async fn post_form<T: serde::de::DeserializeOwned>(
        &self,
        url: &Url,
        body: &[(String, String)],
    ) -> Result<T> {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(body)
            .finish();
        let response = self
            .client
            .post(url.clone())
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(encoded)
            .send()
            .await
            .map_err(|error| {
                Error::with_source(ErrorKind::Api, "identity request failed", error)
            })?;
        let status = response.status();
        if !status.is_success() && status.as_u16() != 400 {
            return Err(Error::new(
                ErrorKind::Api,
                format!("identity endpoint returned HTTP {status}"),
            ));
        }
        response
            .json()
            .await
            .map_err(|error| Error::with_source(ErrorKind::Api, "invalid identity response", error))
    }
}

#[async_trait]
impl IdentityTransport for HttpIdentityTransport {
    async fn device_authorization(&self, config: &IdentityConfig) -> Result<DeviceAuthorization> {
        #[derive(Deserialize)]
        struct Response {
            device_code: String,
            user_code: String,
            verification_uri: String,
            #[serde(default)]
            verification_uri_complete: Option<String>,
            #[serde(default = "default_interval")]
            interval: u64,
            #[serde(default)]
            expires_in: u64,
        }
        fn default_interval() -> u64 {
            5
        }
        let body = vec![
            ("client_id".into(), config.client_id.clone()),
            ("scope".into(), config.scopes.join(" ")),
        ];
        let response: Response = self
            .post_form(&config.device_authorization_url, &body)
            .await?;
        Ok(DeviceAuthorization {
            verification_uri: response
                .verification_uri_complete
                .unwrap_or(response.verification_uri),
            user_code: response.user_code,
            device_code: Secret::new(response.device_code),
            interval_seconds: response.interval,
            expires_in_seconds: response.expires_in,
        })
    }

    async fn token(
        &self,
        config: &IdentityConfig,
        body: Vec<(String, String)>,
    ) -> Result<std::result::Result<TokenResponse, TokenError>> {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&body)
            .finish();
        let response = self
            .client
            .post(config.token_url.clone())
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(encoded)
            .send()
            .await
            .map_err(|error| {
                Error::with_source(ErrorKind::Api, "identity token request failed", error)
            })?;
        if response.status().is_success() {
            return response
                .json::<TokenResponse>()
                .await
                .map(Ok)
                .map_err(|error| {
                    Error::with_source(ErrorKind::Api, "invalid identity token response", error)
                });
        }
        response
            .json::<TokenError>()
            .await
            .map(Err)
            .map_err(|error| {
                Error::with_source(
                    ErrorKind::Api,
                    "invalid identity token error response",
                    error,
                )
            })
    }
}

impl IdentityConfig {
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let client_id = env("CFY_IDENTITY_CLIENT_ID").ok_or_else(|| Error::new(ErrorKind::Config, "CFY_IDENTITY_CLIENT_ID is required; Crabpify cannot redistribute Shopify's private OAuth client ID"))?;
        let base =
            env("CFY_IDENTITY_BASE_URL").unwrap_or_else(|| "https://accounts.shopify.com".into());
        let base = Url::parse(&base).map_err(|error| {
            Error::new(
                ErrorKind::Config,
                format!("invalid CFY_IDENTITY_BASE_URL: {error}"),
            )
        })?;
        Ok(Self {
            client_id,
            device_authorization_url: base
                .join("oauth/device_authorization")
                .map_err(|error| Error::new(ErrorKind::Config, error.to_string()))?,
            token_url: base
                .join("oauth/token")
                .map_err(|error| Error::new(ErrorKind::Config, error.to_string()))?,
            scopes: vec!["openid".into(), "profile".into(), "email".into()],
            max_poll_attempts: 120,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "error")]
pub enum TokenError {
    #[serde(rename = "authorization_pending")]
    AuthorizationPending,
    #[serde(rename = "slow_down")]
    SlowDown,
    #[serde(rename = "access_denied")]
    AccessDenied,
    #[serde(rename = "expired_token")]
    ExpiredToken,
    #[serde(other)]
    Other,
}

#[async_trait]
pub trait IdentityTransport: Send + Sync {
    async fn device_authorization(&self, config: &IdentityConfig) -> Result<DeviceAuthorization>;
    async fn token(
        &self,
        config: &IdentityConfig,
        body: Vec<(String, String)>,
    ) -> Result<std::result::Result<TokenResponse, TokenError>>;
}

pub struct IdentityClient<T> {
    transport: T,
    config: IdentityConfig,
}

impl<T: IdentityTransport> IdentityClient<T> {
    pub fn new(transport: T, config: IdentityConfig) -> Self {
        Self { transport, config }
    }

    pub async fn login_and_save<S: crate::CredentialStore>(
        &self,
        store: &S,
        identity: &str,
    ) -> Result<Session> {
        self.login_and_save_with_notice(store, identity, |_| {})
            .await
    }

    pub async fn login_and_save_with_notice<
        S: crate::CredentialStore,
        F: FnOnce(&DeviceAuthorization),
    >(
        &self,
        store: &S,
        identity: &str,
        notice: F,
    ) -> Result<Session> {
        let authorization = self.transport.device_authorization(&self.config).await?;
        notice(&authorization);
        let mut session = self.device_login_with_authorization(&authorization).await?;
        session.identity = identity.to_owned();
        store.save(&session).await?;
        Ok(session)
    }

    pub async fn device_login(&self) -> Result<Session> {
        let authorization = self.transport.device_authorization(&self.config).await?;
        self.device_login_with_authorization(&authorization).await
    }

    async fn device_login_with_authorization(
        &self,
        authorization: &DeviceAuthorization,
    ) -> Result<Session> {
        let mut delay = authorization.interval_seconds.max(1);
        for _ in 0..self.config.max_poll_attempts {
            let response = self
                .transport
                .token(
                    &self.config,
                    vec![
                        (
                            "grant_type".into(),
                            "urn:ietf:params:oauth:grant-type:device_code".into(),
                        ),
                        (
                            "device_code".into(),
                            authorization.device_code.expose().into(),
                        ),
                        ("client_id".into(), self.config.client_id.clone()),
                    ],
                )
                .await?;
            match response {
                Ok(token) => {
                    return Ok(Session {
                        identity: "default".into(),
                        access_token: token.access_token,
                        refresh_token: token.refresh_token.unwrap_or_else(|| Secret::new("")),
                        expires_at_unix: token.expires_in,
                        scopes: token.scope.split_whitespace().map(str::to_owned).collect(),
                    });
                }
                Err(TokenError::AuthorizationPending) => sleep(Duration::from_secs(delay)).await,
                Err(TokenError::SlowDown) => {
                    delay = delay.saturating_add(5);
                    sleep(Duration::from_secs(delay)).await;
                }
                Err(error) => {
                    return Err(Error::new(
                        ErrorKind::Api,
                        format!("identity device login failed: {error:?}"),
                    ));
                }
            }
        }
        Err(Error::new(
            ErrorKind::Api,
            "identity device login timed out; run login again",
        ))
    }
}

#[async_trait]
impl<T: IdentityTransport> IdentityProvider for IdentityClient<T> {
    async fn request_device_authorization(&self) -> Result<DeviceAuthorization> {
        self.transport.device_authorization(&self.config).await
    }
    async fn poll_device_authorization(
        &self,
        authorization: &DeviceAuthorization,
    ) -> Result<Session> {
        let response = self
            .transport
            .token(
                &self.config,
                vec![(
                    "device_code".into(),
                    authorization.device_code.expose().into(),
                )],
            )
            .await?;
        match response {
            Ok(token) => Ok(Session {
                identity: "default".into(),
                access_token: token.access_token,
                refresh_token: token.refresh_token.unwrap_or_else(|| Secret::new("")),
                expires_at_unix: token.expires_in,
                scopes: token.scope.split_whitespace().map(str::to_owned).collect(),
            }),
            Err(error) => Err(Error::new(
                ErrorKind::Api,
                format!("identity device login failed: {error:?}"),
            )),
        }
    }
    async fn revoke(&self, _session: &Session) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        polls: Mutex<u32>,
    }
    #[async_trait]
    impl IdentityTransport for Fake {
        async fn device_authorization(&self, _: &IdentityConfig) -> Result<DeviceAuthorization> {
            Ok(DeviceAuthorization {
                verification_uri: "https://accounts.shopify.com/activate".into(),
                user_code: "ABCD".into(),
                device_code: Secret::new("device"),
                interval_seconds: 0,
                expires_in_seconds: 60,
            })
        }
        async fn token(
            &self,
            _: &IdentityConfig,
            _: Vec<(String, String)>,
        ) -> Result<std::result::Result<TokenResponse, TokenError>> {
            let mut polls = self.polls.lock().unwrap();
            *polls += 1;
            if *polls == 1 {
                Ok(Err(TokenError::AuthorizationPending))
            } else {
                Ok(Ok(TokenResponse {
                    access_token: Secret::new("secret-access-token"),
                    refresh_token: Some(Secret::new("secret-refresh-token")),
                    expires_in: 3600,
                    scope: "openid profile".into(),
                }))
            }
        }
    }

    fn config() -> IdentityConfig {
        IdentityConfig {
            client_id: "client".into(),
            device_authorization_url: Url::parse("https://example.test/device").unwrap(),
            token_url: Url::parse("https://example.test/token").unwrap(),
            scopes: vec![],
            max_poll_attempts: 2,
        }
    }

    #[tokio::test]
    async fn device_login_polls_pending_then_stores_session_shape() {
        let client = IdentityClient::new(
            Fake {
                polls: Mutex::new(0),
            },
            config(),
        );
        let session = client.device_login().await.unwrap();
        assert_eq!(session.scopes, vec!["openid", "profile"]);
        let debug = format!("{session:?}");
        assert!(!debug.contains("secret-access-token"));
        assert!(!debug.contains("secret-refresh-token"));
    }

    #[test]
    fn client_id_is_required_from_environment() {
        assert!(IdentityConfig::from_env(|_| None).is_err());
    }
}
