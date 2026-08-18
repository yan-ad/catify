//! Shopify Identity device authorization and token exchange adapter.

use crate::{
    Secret, Session,
    flow::{DeviceAuthorization, IdentityProvider},
};
use async_trait::async_trait;
use cfy_core::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
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

    pub async fn device_login(&self) -> Result<Session> {
        let authorization = self.transport.device_authorization(&self.config).await?;
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
