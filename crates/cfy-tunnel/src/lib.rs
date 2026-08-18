//! Pluggable tunnel and local proxy adapters.

use async_trait::async_trait;
use cfy_core::{Cancellation, Error, ErrorKind};
use cfy_process::{OutputMode, ProcessSpec, Supervisor};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error as ThisError;
use tokio::time::{sleep, timeout};
use url::Url;

type Result<T> = std::result::Result<T, TunnelError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TunnelConfig {
    pub local_host: String,
    pub local_port: u16,
    pub public_url: Option<Url>,
    pub provider: TunnelProvider,
    pub max_reconnects: u32,
    pub readiness_timeout_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TunnelProvider {
    Cloudflared {
        executable: String,
    },
    Custom {
        executable: String,
        args: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TunnelState {
    Starting,
    Ready { url: Url },
    Disconnected,
    Reconnecting { attempt: u32 },
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TunnelEvent {
    pub state: TunnelState,
    pub detail: Option<String>,
}

#[derive(Debug, ThisError)]
pub enum TunnelError {
    #[error("local tunnel port must be between 1 and 65535")]
    InvalidPort,
    #[error("tunnel provider executable is empty")]
    EmptyExecutable,
    #[error("tunnel readiness timed out after {0}ms")]
    ReadinessTimeout(u64),
    #[error("tunnel provider did not emit a valid public URL")]
    MissingUrl,
    #[error("tunnel provider exited: {0}")]
    Provider(String),
    #[error("tunnel reconnect budget exhausted")]
    ReconnectExhausted,
    #[error("tunnel session cancelled")]
    Cancelled,
    #[error("process supervisor error: {0}")]
    Process(#[from] Error),
}

impl From<TunnelError> for Error {
    fn from(error: TunnelError) -> Self {
        let kind = match error {
            TunnelError::InvalidPort | TunnelError::EmptyExecutable => ErrorKind::Config,
            TunnelError::Cancelled => ErrorKind::Process,
            TunnelError::Process(error) => return error,
            _ => ErrorKind::Process,
        };
        Error::new(kind, error.to_string())
    }
}

#[async_trait]
pub trait TunnelProviderAdapter: Send + Sync {
    fn process(&self, config: &TunnelConfig) -> Result<ProcessSpec>;
    fn parse_public_url(&self, output: &[u8]) -> Option<Url>;
}

pub struct CloudflaredAdapter;

#[async_trait]
impl TunnelProviderAdapter for CloudflaredAdapter {
    fn process(&self, config: &TunnelConfig) -> Result<ProcessSpec> {
        let TunnelProvider::Cloudflared { executable } = &config.provider else {
            return Err(TunnelError::EmptyExecutable);
        };
        if executable.trim().is_empty() {
            return Err(TunnelError::EmptyExecutable);
        }
        Ok(ProcessSpec::new(executable)
            .args([
                "tunnel",
                "--url",
                &format!("http://{}:{}", config.local_host, config.local_port),
            ])
            .output(OutputMode::CaptureAndStream))
    }

    fn parse_public_url(&self, output: &[u8]) -> Option<Url> {
        parse_https_url(output, |url| {
            url.host_str()
                .is_some_and(|host| host.ends_with("trycloudflare.com"))
        })
    }
}

pub struct CustomProviderAdapter;

#[async_trait]
impl TunnelProviderAdapter for CustomProviderAdapter {
    fn process(&self, config: &TunnelConfig) -> Result<ProcessSpec> {
        let TunnelProvider::Custom { executable, args } = &config.provider else {
            return Err(TunnelError::EmptyExecutable);
        };
        if executable.trim().is_empty() {
            return Err(TunnelError::EmptyExecutable);
        }
        Ok(ProcessSpec::new(executable)
            .args(args.clone())
            .output(OutputMode::CaptureAndStream))
    }

    fn parse_public_url(&self, output: &[u8]) -> Option<Url> {
        parse_https_url(output, |_| true)
    }
}

pub struct TunnelSession<A> {
    supervisor: Supervisor,
    adapter: A,
    config: TunnelConfig,
    events: Vec<TunnelEvent>,
    running: Option<cfy_process::RunningProcess>,
}

impl<A: TunnelProviderAdapter> TunnelSession<A> {
    pub fn new(supervisor: Supervisor, adapter: A, config: TunnelConfig) -> Result<Self> {
        if config.local_port == 0 {
            return Err(TunnelError::InvalidPort);
        }
        Ok(Self {
            supervisor,
            adapter,
            config,
            events: Vec::new(),
            running: None,
        })
    }

    pub fn events(&self) -> &[TunnelEvent] {
        &self.events
    }

    pub async fn start(&mut self, cancellation: &Cancellation) -> Result<Url> {
        self.emit(TunnelState::Starting, None);
        let mut attempt = 0;
        loop {
            if cancellation.is_cancelled() {
                self.stop().await?;
                return Err(TunnelError::Cancelled);
            }
            let spec = self.adapter.process(&self.config)?;
            let mut process = self.supervisor.spawn(spec)?;
            if let Some(url) = self.config.public_url.clone() {
                self.running = Some(process);
                self.emit(TunnelState::Ready { url: url.clone() }, None);
                return Ok(url);
            }
            let readiness = timeout(
                Duration::from_millis(self.config.readiness_timeout_ms.max(1)),
                async {
                    loop {
                        match process.next_output().await {
                            Some(chunk) => {
                                if let Some(url) = self.adapter.parse_public_url(&chunk.bytes) {
                                    return Some(url);
                                }
                            }
                            None => return None,
                        }
                    }
                },
            )
            .await;
            match readiness {
                Ok(Some(url)) => {
                    self.running = Some(process);
                    self.emit(TunnelState::Ready { url: url.clone() }, None);
                    return Ok(url);
                }
                Ok(None) => self.emit(
                    TunnelState::Disconnected,
                    Some("provider exited before readiness".into()),
                ),
                Err(_) => self.emit(TunnelState::Disconnected, Some("readiness timeout".into())),
            }
            process.cancel();
            let _ = process.wait().await;
            if attempt >= self.config.max_reconnects {
                self.emit(
                    TunnelState::Failed,
                    Some("reconnect budget exhausted".into()),
                );
                return Err(TunnelError::ReconnectExhausted);
            }
            attempt += 1;
            self.emit(TunnelState::Reconnecting { attempt }, None);
            sleep(Duration::from_millis(10 * u64::from(attempt))).await;
        }
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(process) = self.running.take() {
            process.cancel();
        }
        self.supervisor.shutdown().await?;
        self.emit(TunnelState::Stopped, None);
        Ok(())
    }

    fn emit(&mut self, state: TunnelState, detail: Option<String>) {
        self.events.push(TunnelEvent { state, detail });
    }
}

fn parse_https_url(bytes: &[u8], predicate: impl Fn(&Url) -> bool) -> Option<Url> {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .filter_map(|token| {
            token
                .trim_matches(|c: char| "()[]{}<>\"',.\n".contains(c))
                .parse::<Url>()
                .ok()
        })
        .find(|url| url.scheme() == "https" && predicate(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: TunnelProvider) -> TunnelConfig {
        TunnelConfig {
            local_host: "127.0.0.1".into(),
            local_port: 3000,
            public_url: None,
            provider,
            max_reconnects: 1,
            readiness_timeout_ms: 50,
        }
    }

    #[test]
    fn cloudflared_builds_local_url_and_parses_readiness() {
        let adapter = CloudflaredAdapter;
        let spec = adapter
            .process(&config(TunnelProvider::Cloudflared {
                executable: "cloudflared".into(),
            }))
            .unwrap();
        assert_eq!(spec.args, vec!["tunnel", "--url", "http://127.0.0.1:3000"]);
        let url = adapter
            .parse_public_url(b"INF https://abc.trycloudflare.com -> http://localhost:3000")
            .unwrap();
        assert_eq!(url.host_str(), Some("abc.trycloudflare.com"));
    }

    #[test]
    fn custom_provider_accepts_documented_url() {
        let adapter = CustomProviderAdapter;
        assert!(
            adapter
                .parse_public_url(b"ready https://dev.example.test/callback")
                .is_some()
        );
    }

    #[tokio::test]
    async fn cancellation_stops_session_without_leaking_children() {
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let mut session = TunnelSession::new(
            Supervisor::default(),
            CustomProviderAdapter,
            config(TunnelProvider::Custom {
                executable: "sleep".into(),
                args: vec!["10".into()],
            }),
        )
        .unwrap();
        assert!(matches!(
            session.start(&cancellation).await,
            Err(TunnelError::Cancelled)
        ));
        assert!(
            session
                .events()
                .iter()
                .any(|event| event.state == TunnelState::Stopped)
        );
    }
}
