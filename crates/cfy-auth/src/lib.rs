//! Secure credential persistence and race-safe Shopify session lifecycle.

mod session;
mod storage;

pub use session::{Clock, SessionManager, SessionRefresher, SystemClock};
pub use storage::{
    CredentialBackend, CredentialStore, FallbackPolicy, NativeCredentialStore, PlaintextConsent,
    PlaintextCredentialStore, Secret, Session,
};
