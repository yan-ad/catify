use crate::{CredentialStore, Session};
use async_trait::async_trait;
use cfy_core::{Error, Result};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

#[async_trait]
pub trait SessionRefresher: Send + Sync {
    async fn refresh(&self, session: &Session) -> Result<Session>;
}

pub trait Clock: Send + Sync {
    fn now_unix(&self) -> u64;
}

#[derive(Debug)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

/// Loads, refreshes, persists, and removes identity sessions.
pub struct SessionManager<S, R, C = SystemClock> {
    store: Arc<S>,
    refresher: Arc<R>,
    clock: Arc<C>,
    refresh_skew_seconds: u64,
    refresh_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl<S, R> SessionManager<S, R, SystemClock>
where
    S: CredentialStore,
    R: SessionRefresher,
{
    #[must_use]
    pub fn new(store: Arc<S>, refresher: Arc<R>) -> Self {
        Self::with_clock(store, refresher, Arc::new(SystemClock), 60)
    }
}

impl<S, R, C> SessionManager<S, R, C>
where
    S: CredentialStore,
    R: SessionRefresher,
    C: Clock,
{
    #[must_use]
    pub fn with_clock(
        store: Arc<S>,
        refresher: Arc<R>,
        clock: Arc<C>,
        refresh_skew_seconds: u64,
    ) -> Self {
        Self {
            store,
            refresher,
            clock,
            refresh_skew_seconds,
            refresh_locks: StdMutex::new(HashMap::new()),
        }
    }

    pub async fn session(&self, identity: &str) -> Result<Option<Session>> {
        let Some(session) = self.store.load(identity).await? else {
            return Ok(None);
        };
        if session.is_valid_at(self.clock.now_unix(), self.refresh_skew_seconds) {
            return Ok(Some(session));
        }

        let lock = {
            let mut locks = self
                .refresh_locks
                .lock()
                .map_err(|_| Error::config("session refresh lock was poisoned"))?;
            Arc::clone(
                locks
                    .entry(identity.to_owned())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = lock.lock().await;

        let Some(current) = self.store.load(identity).await? else {
            return Ok(None);
        };
        if current.is_valid_at(self.clock.now_unix(), self.refresh_skew_seconds) {
            return Ok(Some(current));
        }

        let refreshed = self.refresher.refresh(&current).await?;
        if refreshed.identity != identity {
            return Err(Error::config("refreshed session identity did not match"));
        }
        self.store.save(&refreshed).await?;
        Ok(Some(refreshed))
    }

    pub async fn save(&self, session: &Session) -> Result<()> {
        self.store.save(session).await
    }

    pub async fn logout(&self, identity: &str) -> Result<()> {
        self.store.delete(identity).await
    }
}
