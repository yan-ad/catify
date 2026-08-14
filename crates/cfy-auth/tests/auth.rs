use async_trait::async_trait;
use cfy_auth::{
    Clock, CredentialBackend, CredentialStore, FallbackPolicy, PlaintextConsent,
    PlaintextCredentialStore, Secret, Session, SessionManager, SessionRefresher,
};
use cfy_core::Result;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Barrier, RwLock};

fn session(identity: &str, access: &str, refresh: &str, expires_at_unix: u64) -> Session {
    Session {
        identity: identity.to_owned(),
        access_token: Secret::new(access),
        refresh_token: Secret::new(refresh),
        expires_at_unix,
        scopes: vec!["openid".to_owned()],
    }
}

#[test]
fn native_failure_never_silently_selects_plaintext() {
    assert!(CredentialBackend::after_native_failure(FallbackPolicy::Deny).is_err());
    assert!(matches!(
        CredentialBackend::after_native_failure(FallbackPolicy::AllowPlaintext {
            path: temporary_path("authorized-fallback.json"),
            consent: PlaintextConsent::Explicit,
        })
        .unwrap(),
        CredentialBackend::Plaintext(_)
    ));
}

fn temporary_path(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "cfy-auth-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn secret_and_session_debug_are_redacted() {
    let session = session(
        "developer@example.com",
        "access-secret",
        "refresh-secret",
        100,
    );
    let debug = format!("{session:?}");
    assert!(!debug.contains("access-secret"));
    assert!(!debug.contains("refresh-secret"));
    assert!(debug.contains("[REDACTED]"));
    assert_eq!(format!("{:?}", session.access_token), "[REDACTED]");
}

#[test]
fn plaintext_backend_requires_explicit_consent_and_exposes_warning() {
    let warning = PlaintextCredentialStore::exposure_warning();
    assert!(warning.contains("unencrypted"));
    let _store =
        PlaintextCredentialStore::new(temporary_path("consent.json"), PlaintextConsent::Explicit);
}

#[tokio::test]
async fn plaintext_backend_saves_loads_and_deletes_sessions() {
    let path = temporary_path("round-trip.json");
    let store = PlaintextCredentialStore::new(&path, PlaintextConsent::Explicit);
    let expected = session("developer@example.com", "access", "refresh", 200);

    store.save(&expected).await.unwrap();
    assert_eq!(
        store.load(&expected.identity).await.unwrap(),
        Some(expected.clone())
    );
    store.delete(&expected.identity).await.unwrap();
    assert_eq!(store.load(&expected.identity).await.unwrap(), None);

    let _ = tokio::fs::remove_file(path).await;
}

#[cfg(unix)]
#[tokio::test]
async fn plaintext_backend_uses_owner_only_file_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let path = temporary_path("permissions.json");
    let store = PlaintextCredentialStore::new(&path, PlaintextConsent::Explicit);
    store
        .save(&session("dev", "access", "refresh", 200))
        .await
        .unwrap();

    let mode = tokio::fs::metadata(&path)
        .await
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn blocked_and_corrupt_plaintext_paths_return_typed_errors_without_secrets() {
    let parent_file = temporary_path("blocked-parent");
    tokio::fs::write(&parent_file, b"not a directory")
        .await
        .unwrap();
    let blocked = PlaintextCredentialStore::new(
        parent_file.join("credentials.json"),
        PlaintextConsent::Explicit,
    );
    let secret = "must-not-leak";
    let error = blocked
        .save(&session("dev", secret, "refresh", 200))
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains(secret));

    let corrupt_path = temporary_path("corrupt.json");
    tokio::fs::write(&corrupt_path, b"not-json").await.unwrap();
    let corrupt = PlaintextCredentialStore::new(&corrupt_path, PlaintextConsent::Explicit);
    assert!(corrupt.load("dev").await.is_err());

    let _ = tokio::fs::remove_file(parent_file).await;
    let _ = tokio::fs::remove_file(corrupt_path).await;
}

#[derive(Default)]
struct MemoryStore {
    sessions: RwLock<HashMap<String, Session>>,
}

#[async_trait]
impl CredentialStore for MemoryStore {
    async fn load(&self, identity: &str) -> Result<Option<Session>> {
        Ok(self.sessions.read().await.get(identity).cloned())
    }

    async fn save(&self, session: &Session) -> Result<()> {
        self.sessions
            .write()
            .await
            .insert(session.identity.clone(), session.clone());
        Ok(())
    }

    async fn delete(&self, identity: &str) -> Result<()> {
        self.sessions.write().await.remove(identity);
        Ok(())
    }
}

struct FixedClock(u64);
impl Clock for FixedClock {
    fn now_unix(&self) -> u64 {
        self.0
    }
}

struct CountingRefresher {
    calls: AtomicUsize,
}

#[async_trait]
impl SessionRefresher for CountingRefresher {
    async fn refresh(&self, current: &Session) -> Result<Session> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        Ok(session(
            &current.identity,
            "new-access",
            "new-refresh",
            10_000,
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_expired_session_reads_perform_one_refresh() {
    let store = Arc::new(MemoryStore::default());
    store
        .save(&session(
            "developer@example.com",
            "old-access",
            "old-refresh",
            50,
        ))
        .await
        .unwrap();
    let refresher = Arc::new(CountingRefresher {
        calls: AtomicUsize::new(0),
    });
    let manager = Arc::new(SessionManager::with_clock(
        Arc::clone(&store),
        Arc::clone(&refresher),
        Arc::new(FixedClock(100)),
        10,
    ));
    let barrier = Arc::new(Barrier::new(32));
    let mut tasks = Vec::new();

    for _ in 0..32 {
        let manager = Arc::clone(&manager);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            manager
                .session("developer@example.com")
                .await
                .unwrap()
                .unwrap()
        }));
    }

    for task in tasks {
        let result = task.await.unwrap();
        assert_eq!(result.access_token.expose(), "new-access");
    }
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn valid_sessions_skip_refresh_and_logout_removes_storage() {
    let store = Arc::new(MemoryStore::default());
    let expected = session("developer@example.com", "access", "refresh", 1_000);
    store.save(&expected).await.unwrap();
    let refresher = Arc::new(CountingRefresher {
        calls: AtomicUsize::new(0),
    });
    let manager = SessionManager::with_clock(
        Arc::clone(&store),
        Arc::clone(&refresher),
        Arc::new(FixedClock(100)),
        60,
    );

    assert_eq!(
        manager.session(&expected.identity).await.unwrap(),
        Some(expected.clone())
    );
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 0);
    manager.logout(&expected.identity).await.unwrap();
    assert_eq!(manager.session(&expected.identity).await.unwrap(), None);
}
