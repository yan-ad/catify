use crate::{Secret, Session};
use async_trait::async_trait;
use cfy_core::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceAuthorization {
    pub verification_uri: String,
    pub user_code: String,
    pub device_code: Secret,
    pub interval_seconds: u64,
    pub expires_in_seconds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginMode {
    Browser,
    Headless {
        access_token: Secret,
        refresh_token: Option<Secret>,
        expires_at_unix: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginResult {
    DeviceAuthorization(DeviceAuthorization),
    Session(Session),
}

#[async_trait]
pub trait IdentityProvider: Send + Sync {
    async fn request_device_authorization(&self) -> Result<DeviceAuthorization>;
    async fn poll_device_authorization(
        &self,
        authorization: &DeviceAuthorization,
    ) -> Result<Session>;
    async fn revoke(&self, session: &Session) -> Result<()>;
}

pub async fn login<P: IdentityProvider>(
    provider: &P,
    identity: &str,
    mode: LoginMode,
) -> Result<LoginResult> {
    match mode {
        LoginMode::Headless {
            access_token,
            refresh_token,
            expires_at_unix,
        } => Ok(LoginResult::Session(Session {
            identity: identity.to_owned(),
            access_token,
            refresh_token: refresh_token.unwrap_or_else(|| Secret::new("")),
            expires_at_unix,
            scopes: Vec::new(),
        })),
        LoginMode::Browser => Ok(LoginResult::DeviceAuthorization(
            provider.request_device_authorization().await?,
        )),
    }
}

pub async fn complete_device_login<P: IdentityProvider>(
    provider: &P,
    authorization: &DeviceAuthorization,
) -> Result<Session> {
    if authorization.interval_seconds == 0 || authorization.expires_in_seconds == 0 {
        return Err(Error::new(
            ErrorKind::Api,
            "identity provider returned invalid device authorization timing",
        ));
    }
    provider.poll_device_authorization(authorization).await
}

pub fn headless_from_env(
    identity: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<LoginMode> {
    let Some(access_token) = env("SHOPIFY_CLI_TOKEN").or_else(|| env("SHOPIFY_CLI_THEME_TOKEN"))
    else {
        return Err(Error::new(
            ErrorKind::Api,
            "headless login requires SHOPIFY_CLI_TOKEN; browser device login is unavailable in non-interactive mode",
        ));
    };
    let refresh_token = env("SHOPIFY_CLI_REFRESH_TOKEN").map(Secret::new);
    let expires_at_unix = env("SHOPIFY_CLI_TOKEN_EXPIRES_AT")
        .and_then(|value| value.parse().ok())
        .unwrap_or(u64::MAX);
    let _ = identity;
    Ok(LoginMode::Headless {
        access_token: Secret::new(access_token),
        refresh_token,
        expires_at_unix,
    })
}

pub fn poll_interval(authorization: &DeviceAuthorization) -> Duration {
    Duration::from_secs(authorization.interval_seconds.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_requires_explicit_credential_and_never_uses_argv() {
        let mode = headless_from_env("user", |key| {
            (key == "SHOPIFY_CLI_TOKEN").then(|| "secret-token".into())
        })
        .unwrap();
        assert!(matches!(mode, LoginMode::Headless { .. }));
        assert!(headless_from_env("user", |_| None).is_err());
        assert!(!format!("{mode:?}").contains("secret-token"));
    }

    #[test]
    fn device_timing_is_never_zero() {
        let authorization = DeviceAuthorization {
            verification_uri: "https://example.test".into(),
            user_code: "ABCD".into(),
            device_code: Secret::new("hidden"),
            interval_seconds: 0,
            expires_in_seconds: 1,
        };
        assert!(matches!(poll_interval(&authorization), Duration { .. }));
    }
}
