use async_trait::async_trait;
use cfy_core::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::Mutex;
use zeroize::{Zeroize, ZeroizeOnDrop};

const SERVICE_NAME: &str = "dev.catify.cfy";
const LEGACY_SERVICE_NAME: &str = "dev.crabpify.cfy";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Secret text that is zeroized on drop and never exposed through `Debug`.
#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Clone for Secret {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for Secret {}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// Developer identity session stored as one credential payload.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub identity: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub access_token: Secret,
    pub refresh_token: Secret,
    pub expires_at_unix: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("identity", &self.identity)
            .field("display_name", &self.display_name)
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at_unix", &self.expires_at_unix)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl Session {
    #[must_use]
    pub const fn is_valid_at(&self, now_unix: u64, refresh_skew_seconds: u64) -> bool {
        self.expires_at_unix.saturating_sub(refresh_skew_seconds) > now_unix
    }
}

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn load(&self, identity: &str) -> Result<Option<Session>>;
    async fn save(&self, session: &Session) -> Result<()>;
    async fn delete(&self, identity: &str) -> Result<()>;
}

/// Native OS credential storage: Keychain, Secret Service, or Credential Manager.
#[derive(Debug, Clone)]
pub struct NativeCredentialStore {
    service: String,
    legacy_service: Option<String>,
}

impl Default for NativeCredentialStore {
    fn default() -> Self {
        Self {
            service: SERVICE_NAME.to_owned(),
            legacy_service: Some(LEGACY_SERVICE_NAME.to_owned()),
        }
    }
}

impl NativeCredentialStore {
    #[must_use]
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            legacy_service: None,
        }
    }

    fn entry(service: &str, identity: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(service, identity).map_err(|source| {
            Error::with_source(
                ErrorKind::Config,
                "could not access the operating system credential store",
                source,
            )
        })
    }
}

#[async_trait]
impl CredentialStore for NativeCredentialStore {
    async fn load(&self, identity: &str) -> Result<Option<Session>> {
        let service = self.service.clone();
        let legacy_service = self.legacy_service.clone();
        let identity = identity.to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = Self::entry(&service, &identity)?;
            match entry.get_password() {
                Ok(payload) => decode_session(&payload).map(Some),
                Err(keyring::Error::NoEntry) => {
                    let Some(legacy_service) = legacy_service else {
                        return Ok(None);
                    };
                    let legacy_entry = Self::entry(&legacy_service, &identity)?;
                    let payload = match legacy_entry.get_password() {
                        Ok(payload) => payload,
                        Err(keyring::Error::NoEntry) => return Ok(None),
                        Err(source) => {
                            return Err(Error::with_source(
                                ErrorKind::Config,
                                "could not read legacy credentials from the operating system store",
                                source,
                            ));
                        }
                    };
                    let session = decode_session(&payload)?;
                    entry.set_password(&payload).map_err(|source| {
                        Error::with_source(
                            ErrorKind::Config,
                            "could not migrate credentials to the Catify credential store",
                            source,
                        )
                    })?;
                    match legacy_entry.delete_credential() {
                        Ok(()) | Err(keyring::Error::NoEntry) => {}
                        Err(source) => {
                            return Err(Error::with_source(
                                ErrorKind::Config,
                                "credentials were migrated but the legacy entry could not be removed",
                                source,
                            ));
                        }
                    }
                    Ok(Some(session))
                }
                Err(source) => Err(Error::with_source(
                    ErrorKind::Config,
                    "could not read credentials from the operating system store",
                    source,
                )),
            }
        })
        .await
        .map_err(|source| Error::with_source(ErrorKind::Config, "credential task failed", source))?
    }

    async fn save(&self, session: &Session) -> Result<()> {
        let service = self.service.clone();
        let identity = session.identity.clone();
        let payload = encode_session(session)?;
        tokio::task::spawn_blocking(move || {
            Self::entry(&service, &identity)?
                .set_password(&payload)
                .map_err(|source| {
                    Error::with_source(
                        ErrorKind::Config,
                        "could not save credentials in the operating system store",
                        source,
                    )
                })
        })
        .await
        .map_err(|source| Error::with_source(ErrorKind::Config, "credential task failed", source))?
    }

    async fn delete(&self, identity: &str) -> Result<()> {
        let service = self.service.clone();
        let identity = identity.to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = Self::entry(&service, &identity)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(source) => Err(Error::with_source(
                    ErrorKind::Config,
                    "could not delete credentials from the operating system store",
                    source,
                )),
            }
        })
        .await
        .map_err(|source| Error::with_source(ErrorKind::Config, "credential task failed", source))?
    }
}

/// Deliberate opt-in required before credentials may be written as plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaintextConsent {
    Explicit,
}

/// Caller policy when native credential storage is unavailable.
#[derive(Debug, Clone)]
pub enum FallbackPolicy {
    Deny,
    AllowPlaintext {
        path: PathBuf,
        consent: PlaintextConsent,
    },
}

/// Runtime-selected credential backend with no implicit insecure fallback.
#[derive(Debug, Clone)]
pub enum CredentialBackend {
    Native(NativeCredentialStore),
    Plaintext(PlaintextCredentialStore),
}

impl CredentialBackend {
    pub fn after_native_failure(policy: FallbackPolicy) -> Result<Self> {
        match policy {
            FallbackPolicy::Deny => Err(Error::config(
                "native credential storage is unavailable; plaintext fallback was not authorized",
            )),
            FallbackPolicy::AllowPlaintext { path, consent } => Ok(Self::Plaintext(
                PlaintextCredentialStore::new(path, consent),
            )),
        }
    }
}

#[async_trait]
impl CredentialStore for CredentialBackend {
    async fn load(&self, identity: &str) -> Result<Option<Session>> {
        match self {
            Self::Native(store) => store.load(identity).await,
            Self::Plaintext(store) => store.load(identity).await,
        }
    }

    async fn save(&self, session: &Session) -> Result<()> {
        match self {
            Self::Native(store) => store.save(session).await,
            Self::Plaintext(store) => store.save(session).await,
        }
    }

    async fn delete(&self, identity: &str) -> Result<()> {
        match self {
            Self::Native(store) => store.delete(identity).await,
            Self::Plaintext(store) => store.delete(identity).await,
        }
    }
}

/// Plaintext fallback for environments without a usable native credential store.
#[derive(Debug, Clone)]
pub struct PlaintextCredentialStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl PlaintextCredentialStore {
    pub const EXPOSURE_WARNING: &'static str = "Credentials will be stored unencrypted on disk. Anyone with access to this account or backup may read and reuse them.";

    #[must_use]
    pub fn new(path: impl Into<PathBuf>, _consent: PlaintextConsent) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    #[must_use]
    pub const fn exposure_warning() -> &'static str {
        Self::EXPOSURE_WARNING
    }

    async fn read_all(&self) -> Result<HashMap<String, Session>> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("credential file {} is invalid", self.path.display()),
                    source,
                )
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(source) => Err(Error::with_source(
                ErrorKind::Config,
                format!("could not read credential file {}", self.path.display()),
                source,
            )),
        }
    }

    async fn write_all(&self, sessions: &HashMap<String, Session>) -> Result<()> {
        let bytes = serde_json::to_vec(sessions).map_err(|source| {
            Error::with_source(
                ErrorKind::Config,
                "could not encode credential file",
                source,
            )
        })?;
        atomic_private_write(&self.path, &bytes).await
    }
}

#[async_trait]
impl CredentialStore for PlaintextCredentialStore {
    async fn load(&self, identity: &str) -> Result<Option<Session>> {
        let _guard = self.lock.lock().await;
        Ok(self.read_all().await?.remove(identity))
    }

    async fn save(&self, session: &Session) -> Result<()> {
        let _guard = self.lock.lock().await;
        let mut sessions = self.read_all().await?;
        sessions.insert(session.identity.clone(), session.clone());
        self.write_all(&sessions).await
    }

    async fn delete(&self, identity: &str) -> Result<()> {
        let _guard = self.lock.lock().await;
        let mut sessions = self.read_all().await?;
        if sessions.remove(identity).is_some() {
            self.write_all(&sessions).await?;
        }
        Ok(())
    }
}

async fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent).await.map_err(|source| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not create credential directory {}", parent.display()),
            source,
        )
    })?;
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));

    #[cfg(unix)]
    let options = {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        options
    };
    #[cfg(not(unix))]
    let options = {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        options
    };

    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut file = options.open(&temporary).await?;
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        replace_file(&temporary, path).await
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result.map_err(|source| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not write credential file {}", path.display()),
            source,
        )
    })
}

async fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::{os::windows::ffi::OsStrExt, ptr};

        #[link(name = "Kernel32")]
        unsafe extern "system" {
            fn ReplaceFileW(
                replaced_file_name: *const u16,
                replacement_file_name: *const u16,
                backup_file_name: *const u16,
                replace_flags: u32,
                exclude: *mut std::ffi::c_void,
                reserved: *mut std::ffi::c_void,
            ) -> i32;
        }

        if tokio::fs::try_exists(destination).await? {
            const REPLACEFILE_WRITE_THROUGH: u32 = 0x0000_0001;
            let destination: Vec<u16> = destination
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let source: Vec<u16> = source
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: both path buffers are NUL-terminated and remain alive for the call.
            let replaced = unsafe {
                ReplaceFileW(
                    destination.as_ptr(),
                    source.as_ptr(),
                    ptr::null(),
                    REPLACEFILE_WRITE_THROUGH,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if replaced == 0 {
                return Err(std::io::Error::last_os_error());
            }
            return Ok(());
        }
    }
    tokio::fs::rename(source, destination).await
}

fn encode_session(session: &Session) -> Result<String> {
    serde_json::to_string(session).map_err(|source| {
        Error::with_source(ErrorKind::Config, "could not encode credential", source)
    })
}

fn decode_session(payload: &str) -> Result<Session> {
    serde_json::from_str(payload).map_err(|source| {
        Error::with_source(ErrorKind::Config, "stored credential is invalid", source)
    })
}
