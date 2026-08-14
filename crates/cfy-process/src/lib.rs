//! Cross-platform subprocess supervision for adapters, tunnels, and build tools.

use cfy_core::{Error, ErrorKind, Result};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{Notify, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, timeout},
};

const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub current_dir: Option<PathBuf>,
    pub output: OutputMode,
}

impl ProcessSpec {
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            environment: Vec::new(),
            current_dir: None,
            output: OutputMode::Capture,
        }
    }

    #[must_use]
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.push((key.into(), value.into()));
        self
    }

    #[must_use]
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    #[must_use]
    pub const fn output(mut self, output: OutputMode) -> Self {
        self.output = output;
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    Capture,
    Stream,
    CaptureAndStream,
    Inherit,
}

impl OutputMode {
    const fn captures(self) -> bool {
        matches!(self, Self::Capture | Self::CaptureAndStream)
    }

    const fn streams(self) -> bool {
        matches!(self, Self::Stream | Self::CaptureAndStream)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cancelled: bool,
}

impl ProcessOutput {
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        self.status.code()
    }
}

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
    grace_period: Duration,
    next_id: AtomicU64,
    children: Mutex<HashMap<u64, watch::Sender<Option<ShutdownSignal>>>>,
    child_exited: Notify,
}

#[derive(Debug, Clone, Copy)]
enum ShutdownSignal {
    Interrupt,
    Terminate,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new(DEFAULT_GRACE_PERIOD)
    }
}

impl Supervisor {
    #[must_use]
    pub fn new(grace_period: Duration) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                grace_period,
                next_id: AtomicU64::new(1),
                children: Mutex::new(HashMap::new()),
                child_exited: Notify::new(),
            }),
        }
    }

    pub fn spawn(&self, spec: ProcessSpec) -> Result<RunningProcess> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .envs(spec.environment.iter().cloned());
        if let Some(current_dir) = &spec.current_dir {
            command.current_dir(current_dir);
        }

        if spec.output == OutputMode::Inherit {
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        } else {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }

        platform::configure(&mut command);
        let mut child = command.spawn().map_err(|error| {
            Error::with_source(
                ErrorKind::Process,
                format!("could not start {}", spec.program),
                error,
            )
        })?;
        let process_tree = platform::ProcessTree::attach(&mut child).map_err(|error| {
            let _ = child.start_kill();
            Error::with_source(
                ErrorKind::Process,
                format!("could not supervise {}", spec.program),
                error,
            )
        })?;

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (cancel_tx, cancel_rx) = watch::channel(None);
        let (complete_tx, complete_rx) = oneshot::channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        self.inner
            .children
            .lock()
            .expect("process registry lock poisoned")
            .insert(id, cancel_tx.clone());

        let inner = Arc::downgrade(&self.inner);
        let grace_period = self.inner.grace_period;
        tokio::spawn(async move {
            let result = supervise_child(
                child,
                process_tree,
                spec.output,
                cancel_rx,
                events_tx,
                grace_period,
            )
            .await;
            remove_child(&inner, id);
            let _ = complete_tx.send(result);
        });

        Ok(RunningProcess {
            id,
            cancel: cancel_tx,
            completion: Some(complete_rx),
            events: events_rx,
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        let senders = self
            .inner
            .children
            .lock()
            .expect("process registry lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for sender in senders {
            let _ = sender.send(Some(ShutdownSignal::Terminate));
        }

        let deadline = Instant::now() + self.inner.grace_period + Duration::from_secs(5);
        loop {
            let child_exited = self.inner.child_exited.notified();
            if self
                .inner
                .children
                .lock()
                .expect("process registry lock poisoned")
                .is_empty()
            {
                return Ok(());
            }
            if timeout(
                deadline.saturating_duration_since(Instant::now()),
                child_exited,
            )
            .await
            .is_err()
            {
                return Err(Error::process("timed out while stopping child processes"));
            }
        }
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        self.inner
            .children
            .lock()
            .expect("process registry lock poisoned")
            .len()
    }
}

pub struct RunningProcess {
    id: u64,
    cancel: watch::Sender<Option<ShutdownSignal>>,
    completion: Option<oneshot::Receiver<Result<ProcessOutput>>>,
    events: mpsc::UnboundedReceiver<OutputChunk>,
}

impl RunningProcess {
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn cancel(&self) {
        let _ = self.cancel.send(Some(ShutdownSignal::Terminate));
    }

    pub async fn next_output(&mut self) -> Option<OutputChunk> {
        self.events.recv().await
    }

    pub async fn wait(mut self) -> Result<ProcessOutput> {
        self.wait_inner().await
    }

    pub async fn wait_with_signal_forwarding(mut self) -> Result<ProcessOutput> {
        let completion = self
            .completion
            .as_mut()
            .expect("process completion receiver missing");
        tokio::select! {
            result = completion => flatten_completion(result),
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| Error::with_source(
                    ErrorKind::Process,
                    "could not listen for an interrupt signal",
                    error,
                ))?;
                let _ = self.cancel.send(Some(ShutdownSignal::Interrupt));
                self.wait_inner().await
            }
        }
    }

    async fn wait_inner(&mut self) -> Result<ProcessOutput> {
        let completion = self
            .completion
            .take()
            .expect("process can only be awaited once");
        flatten_completion(completion.await)
    }
}

impl Drop for RunningProcess {
    fn drop(&mut self) {
        if self.completion.is_some() {
            let _ = self.cancel.send(Some(ShutdownSignal::Terminate));
        }
    }
}

fn flatten_completion(
    result: std::result::Result<Result<ProcessOutput>, oneshot::error::RecvError>,
) -> Result<ProcessOutput> {
    result.map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "process supervisor stopped unexpectedly",
            error,
        )
    })?
}

async fn supervise_child(
    mut child: Child,
    process_tree: platform::ProcessTree,
    output_mode: OutputMode,
    mut cancel: watch::Receiver<Option<ShutdownSignal>>,
    events: mpsc::UnboundedSender<OutputChunk>,
    grace_period: Duration,
) -> Result<ProcessOutput> {
    let stdout_task = child
        .stdout
        .take()
        .map(|stdout| pump_output(stdout, OutputStream::Stdout, output_mode, events.clone()));
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| pump_output(stderr, OutputStream::Stderr, output_mode, events));

    let mut cancelled = false;
    let status = tokio::select! {
        status = child.wait() => status,
        changed = cancel.changed() => {
            let signal = cancel.borrow().to_owned();
            if changed.is_ok() && let Some(signal) = signal {
                cancelled = true;
                process_tree.signal(signal);
                match timeout(grace_period, child.wait()).await {
                    Ok(status) => status,
                    Err(_) => {
                        process_tree.kill();
                        child.wait().await
                    }
                }
            } else {
                child.wait().await
            }
        }
    }
    .map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not wait for child process",
            error,
        )
    })?;

    drop(process_tree);
    let stdout = join_output(stdout_task).await?;
    let stderr = join_output(stderr_task).await?;
    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
        cancelled,
    })
}

fn pump_output(
    mut reader: impl AsyncRead + Unpin + Send + 'static,
    stream: OutputStream,
    mode: OutputMode,
    events: mpsc::UnboundedSender<OutputChunk>,
) -> JoinHandle<std::io::Result<Vec<u8>>> {
    tokio::spawn(async move {
        let mut captured = Vec::new();
        let mut buffer = vec![0; 8 * 1024];
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                return Ok(captured);
            }
            let bytes = buffer[..count].to_vec();
            if mode.captures() {
                captured.extend_from_slice(&bytes);
            }
            if mode.streams() {
                let _ = events.send(OutputChunk { stream, bytes });
            }
        }
    })
}

async fn join_output(task: Option<JoinHandle<std::io::Result<Vec<u8>>>>) -> Result<Vec<u8>> {
    let Some(task) = task else {
        return Ok(Vec::new());
    };
    task.await
        .map_err(|error| {
            Error::with_source(ErrorKind::Process, "output reader task failed", error)
        })?
        .map_err(|error| {
            Error::with_source(ErrorKind::Process, "could not read process output", error)
        })
}

fn remove_child(inner: &Weak<SupervisorInner>, id: u64) {
    let Some(inner) = inner.upgrade() else {
        return;
    };
    inner
        .children
        .lock()
        .expect("process registry lock poisoned")
        .remove(&id);
    inner.child_exited.notify_waiters();
}

/// Runs one process to completion while preserving its exact exit status.
pub async fn run(spec: &ProcessSpec) -> Result<ExitStatus> {
    Supervisor::default()
        .spawn(spec.clone())?
        .wait_with_signal_forwarding()
        .await
        .map(|output| output.status)
}

#[cfg(unix)]
mod platform {
    use std::io;
    use tokio::process::{Child, Command};

    pub fn configure(command: &mut Command) {
        command.process_group(0);
    }

    pub struct ProcessTree {
        group_id: i32,
    }

    impl ProcessTree {
        pub fn attach(child: &mut Child) -> io::Result<Self> {
            let group_id = child
                .id()
                .ok_or_else(|| io::Error::other("child process has no process ID"))?;
            Ok(Self {
                group_id: group_id.cast_signed(),
            })
        }

        pub fn signal(&self, signal: super::ShutdownSignal) {
            let signal = match signal {
                super::ShutdownSignal::Interrupt => libc::SIGINT,
                super::ShutdownSignal::Terminate => libc::SIGTERM,
            };
            signal_group(self.group_id, signal);
        }

        pub fn kill(&self) {
            signal_group(self.group_id, libc::SIGKILL);
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            self.kill();
        }
    }

    fn signal_group(group_id: i32, signal: i32) {
        // SAFETY: A negative PID asks kill(2) to signal the isolated process group.
        unsafe {
            libc::kill(-group_id, signal);
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::{io, mem::size_of, ptr};
    use tokio::process::{Child, Command};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::CREATE_NEW_PROCESS_GROUP,
        },
    };

    pub fn configure(command: &mut Command) {
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    pub struct ProcessTree {
        job: HANDLE,
    }

    unsafe impl Send for ProcessTree {}

    impl ProcessTree {
        pub fn attach(child: &mut Child) -> io::Result<Self> {
            // SAFETY: Win32 handles are checked for failure and owned by ProcessTree.
            unsafe {
                let job = CreateJobObjectW(ptr::null(), ptr::null());
                if job.is_null() {
                    return Err(io::Error::last_os_error());
                }

                let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    ptr::addr_of!(information).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    let error = io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error);
                }

                let process = child.raw_handle().ok_or_else(|| {
                    CloseHandle(job);
                    io::Error::other("child process has no process handle")
                })? as HANDLE;
                if AssignProcessToJobObject(job, process) == 0 {
                    let error = io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error);
                }
                Ok(Self { job })
            }
        }

        pub fn signal(&self, _signal: super::ShutdownSignal) {
            self.terminate_job();
        }

        pub fn kill(&self) {
            self.terminate_job();
        }

        fn terminate_job(&self) {
            // SAFETY: self.job remains valid for the lifetime of ProcessTree.
            unsafe {
                TerminateJobObject(self.job, 1);
            }
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE also prevents descendants escaping if the supervisor crashes.
            unsafe {
                CloseHandle(self.job);
            }
        }
    }
}
