//! App development lifecycle orchestration.

use cfy_core::{Cancellation, Error, ErrorKind};
use cfy_process::{OutputStream, ProcessOutput, ProcessSpec, RunningProcess, Supervisor};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};
use thiserror::Error as ThisError;
use tokio::sync::broadcast;

mod tls_proxy;

pub use tls_proxy::{TlsProxy, TlsProxyError};

type Result<T> = std::result::Result<T, DevError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ComponentState {
    Pending,
    Starting,
    Ready,
    Running,
    Restarting { attempt: u32 },
    Failed,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentSpec {
    pub name: String,
    pub process: ProcessSpec,
    pub max_restarts: u32,
    pub restart_backoff_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComponentSnapshot {
    pub name: String,
    pub state: ComponentState,
    pub restart_count: u32,
    pub last_exit_code: Option<i32>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LifecycleEvent {
    StateChanged {
        component: String,
        state: ComponentState,
    },
    Output {
        component: String,
        bytes: Vec<u8>,
        stderr: bool,
    },
    Failure {
        component: String,
        message: String,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DevOptions {
    pub max_parallel: usize,
    pub restart_failed: bool,
}

impl Default for DevOptions {
    fn default() -> Self {
        Self {
            max_parallel: 4,
            restart_failed: true,
        }
    }
}

#[derive(Debug, ThisError)]
pub enum DevError {
    #[error("development session requires at least one component")]
    Empty,
    #[error("component name is duplicated: {0}")]
    Duplicate(String),
    #[error("max_parallel must be greater than zero")]
    InvalidParallelism,
    #[error("component {component} failed: {message}")]
    ComponentFailed { component: String, message: String },
    #[error("development session was cancelled")]
    Cancelled,
    #[error("process supervisor error: {0}")]
    Process(#[from] Error),
}

impl From<DevError> for Error {
    fn from(error: DevError) -> Self {
        let kind = match error {
            DevError::Process(error) => return error,
            DevError::Cancelled => ErrorKind::Process,
            DevError::Empty | DevError::Duplicate(_) | DevError::InvalidParallelism => {
                ErrorKind::Config
            }
            DevError::ComponentFailed { .. } => ErrorKind::Process,
        };
        Error::new(kind, error.to_string())
    }
}

pub fn validate_specs(specs: &[ComponentSpec], options: &DevOptions) -> Result<()> {
    if specs.is_empty() {
        return Err(DevError::Empty);
    }
    if options.max_parallel == 0 {
        return Err(DevError::InvalidParallelism);
    }
    let mut names = std::collections::HashSet::new();
    for spec in specs {
        if !names.insert(&spec.name) {
            return Err(DevError::Duplicate(spec.name.clone()));
        }
    }
    Ok(())
}

pub struct DevSession {
    supervisor: Supervisor,
    components: HashMap<String, RunningComponent>,
    snapshots: HashMap<String, ComponentSnapshot>,
    events: Vec<LifecycleEvent>,
    event_tx: broadcast::Sender<LifecycleEvent>,
    options: DevOptions,
}

struct RunningComponent {
    process: RunningProcess,
    spec: ComponentSpec,
}

impl DevSession {
    pub fn new(
        supervisor: Supervisor,
        specs: &[ComponentSpec],
        options: DevOptions,
    ) -> Result<Self> {
        validate_specs(specs, &options)?;
        let snapshots = specs
            .iter()
            .map(|spec| {
                (
                    spec.name.clone(),
                    ComponentSnapshot {
                        name: spec.name.clone(),
                        state: ComponentState::Pending,
                        restart_count: 0,
                        last_exit_code: None,
                        diagnostics: Vec::new(),
                    },
                )
            })
            .collect();
        let (event_tx, _) = broadcast::channel(512);
        Ok(Self {
            supervisor,
            components: HashMap::new(),
            snapshots,
            events: Vec::new(),
            event_tx,
            options,
        })
    }

    pub fn snapshots(&self) -> Vec<ComponentSnapshot> {
        let mut values = self.snapshots.values().cloned().collect::<Vec<_>>();
        values.sort_by(|a, b| a.name.cmp(&b.name));
        values
    }

    pub fn events(&self) -> &[LifecycleEvent] {
        &self.events
    }

    /// Subscribes to lifecycle and child-output events as they occur. Dropping
    /// or lagging a dashboard does not affect process supervision.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.event_tx.subscribe()
    }

    pub async fn start(
        &mut self,
        specs: &[ComponentSpec],
        cancellation: &Cancellation,
    ) -> Result<()> {
        validate_specs(specs, &self.options)?;
        let _parallelism = self.options.max_parallel;
        for spec in specs {
            if cancellation.is_cancelled() {
                self.shutdown().await?;
                return Err(DevError::Cancelled);
            }
            self.set_state(&spec.name, ComponentState::Starting);
            let process = self
                .supervisor
                .spawn(spec.process.clone())
                .map_err(DevError::Process)?;
            self.forward_output(&spec.name, &process);
            self.set_state(&spec.name, ComponentState::Ready);
            self.components.insert(
                spec.name.clone(),
                RunningComponent {
                    process,
                    spec: spec.clone(),
                },
            );
            self.set_state(&spec.name, ComponentState::Running);
        }
        Ok(())
    }

    pub async fn wait(&mut self, cancellation: &Cancellation) -> Result<()> {
        loop {
            if cancellation.is_cancelled() {
                self.shutdown().await?;
                return Err(DevError::Cancelled);
            }
            let Some(name) = self.components.keys().next().cloned() else {
                return Ok(());
            };
            let running = self.components.remove(&name).expect("component exists");
            let output = running.process.wait().await.map_err(DevError::Process)?;
            self.record_output(&name, &output);
            if output.cancelled || cancellation.is_cancelled() {
                self.shutdown().await?;
                return Err(DevError::Cancelled);
            }
            if output.exit_code() == Some(0) {
                self.set_state(&name, ComponentState::Stopped);
                continue;
            }
            let snapshot = self.snapshots.get_mut(&name).expect("snapshot exists");
            snapshot.last_exit_code = output.exit_code();
            snapshot
                .diagnostics
                .push(String::from_utf8_lossy(&output.stderr).trim().to_string());
            if self.options.restart_failed && snapshot.restart_count < running.spec.max_restarts {
                snapshot.restart_count += 1;
                let attempt = snapshot.restart_count;
                self.set_state(&name, ComponentState::Restarting { attempt });
                tokio::time::sleep(Duration::from_millis(running.spec.restart_backoff_ms)).await;
                let process = self
                    .supervisor
                    .spawn(running.spec.process.clone())
                    .map_err(DevError::Process)?;
                self.forward_output(&name, &process);
                self.components.insert(
                    name.clone(),
                    RunningComponent {
                        process,
                        spec: running.spec,
                    },
                );
                self.set_state(&name, ComponentState::Running);
            } else {
                self.set_state(&name, ComponentState::Failed);
                self.emit(LifecycleEvent::Failure {
                    component: name.clone(),
                    message: format!("exit code {:?}", output.exit_code()),
                });
                self.shutdown().await?;
                return Err(DevError::ComponentFailed {
                    component: name,
                    message: "restart budget exhausted".into(),
                });
            }
        }
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let mut stopped = Vec::new();
        for snapshot in self.snapshots.values_mut() {
            snapshot.state = ComponentState::Stopped;
            stopped.push(LifecycleEvent::StateChanged {
                component: snapshot.name.clone(),
                state: ComponentState::Stopped,
            });
        }
        for event in stopped {
            self.emit(event);
        }
        self.components.clear();
        self.supervisor
            .shutdown()
            .await
            .map_err(DevError::Process)?;
        self.emit(LifecycleEvent::Shutdown);
        Ok(())
    }

    fn set_state(&mut self, name: &str, state: ComponentState) {
        if let Some(snapshot) = self.snapshots.get_mut(name) {
            snapshot.state = state.clone();
        }
        self.emit(LifecycleEvent::StateChanged {
            component: name.into(),
            state,
        });
    }

    fn record_output(&mut self, name: &str, output: &ProcessOutput) {
        // Output is forwarded live by `forward_output`. Keep a final captured
        // diagnostic for callers that inspect snapshots after process exit.
        if !output.stderr.is_empty()
            && let Some(snapshot) = self.snapshots.get_mut(name)
        {
            snapshot
                .diagnostics
                .push(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
    }

    fn forward_output(&self, component: &str, process: &RunningProcess) {
        let mut output = process.subscribe_output();
        let events = self.event_tx.clone();
        let component = component.to_owned();
        tokio::spawn(async move {
            loop {
                match output.recv().await {
                    Ok(chunk) => {
                        let _ = events.send(LifecycleEvent::Output {
                            component: component.clone(),
                            bytes: chunk.bytes,
                            stderr: chunk.stream == OutputStream::Stderr,
                        });
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn emit(&mut self, event: LifecycleEvent) {
        let _ = self.event_tx.send(event.clone());
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, code: &str) -> ComponentSpec {
        ComponentSpec {
            name: name.into(),
            process: ProcessSpec::new("sh").args(["-c", code]),
            max_restarts: 1,
            restart_backoff_ms: 1,
        }
    }

    #[test]
    fn validates_duplicates_and_budget() {
        assert!(matches!(
            validate_specs(&[], &DevOptions::default()),
            Err(DevError::Empty)
        ));
        assert!(matches!(
            validate_specs(
                &[spec("a", "exit 0"), spec("a", "exit 0")],
                &DevOptions::default()
            ),
            Err(DevError::Duplicate(_))
        ));
        assert!(matches!(
            validate_specs(
                &[spec("a", "exit 0")],
                &DevOptions {
                    max_parallel: 0,
                    restart_failed: true
                }
            ),
            Err(DevError::InvalidParallelism)
        ));
    }

    #[tokio::test]
    async fn failed_component_restarts_then_cleans_up() {
        let mut session = DevSession::new(
            Supervisor::default(),
            &[spec("bad", "exit 9"), spec("sibling", "sleep 10")],
            DevOptions::default(),
        )
        .unwrap();
        session
            .start(
                &[spec("bad", "exit 9"), spec("sibling", "sleep 10")],
                &Cancellation::default(),
            )
            .await
            .unwrap();
        let result = session.wait(&Cancellation::default()).await;
        assert!(matches!(result, Err(DevError::ComponentFailed { .. })));
        assert!(session.events().iter().any(|event| matches!(event, LifecycleEvent::StateChanged { component, state: ComponentState::Ready } if component == "bad")));
        assert!(session.events().iter().any(|event| matches!(event, LifecycleEvent::StateChanged { component, state: ComponentState::Restarting { .. } } if component == "bad")));
        assert!(
            session
                .events()
                .iter()
                .any(|event| matches!(event, LifecycleEvent::Shutdown))
        );
    }
}
