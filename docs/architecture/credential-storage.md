# Credential storage and session lifecycle

Issue: [#15](https://github.com/yan-ad/crabpify/issues/15)

## Backend policy

`cfy-auth` defaults to the operating system credential service through the
`keyring` crate:

- macOS: Keychain
- Linux: Secret Service with persistent storage
- Windows: Credential Manager

The CLI must not silently downgrade when the native service is missing or
locked. `FallbackPolicy::Deny` returns a typed configuration error.
Plaintext storage can only be selected by passing both an explicit path and
`PlaintextConsent::Explicit`. Before asking for that consent, the caller must
display `PlaintextCredentialStore::exposure_warning()` verbatim.

The plaintext backend is intended for constrained/headless environments where
the user accepts that credentials are readable by the account and its backups.
Writes are atomic, temporary files are cleaned after failures, and Unix files
are created with mode `0600`.

## Secret handling

- `Secret` and `Session` redact token values from `Debug` output.
- `Secret` memory is zeroized when dropped.
- Storage errors contain operation and path context but never payloads.
- Native keychain calls run on blocking worker threads rather than the async
  executor.

Callers should continue to register token values with the CLI-wide diagnostic
redactor before performing authenticated network operations. This protects
third-party error messages that may echo request data.

## Session lifecycle

`SessionManager` loads a session and returns it directly while it remains valid
outside the configured refresh skew. Expired sessions are refreshed through a
`SessionRefresher` implementation and persisted before being returned.

Refresh is single-flight per identity. Concurrent requests re-read storage
after acquiring the identity lock, so only the first request exchanges the
refresh token; the rest consume the new session. Different identities can
refresh independently.

`logout(identity)` deletes local storage and treats a missing credential as a
successful result. It does **not** imply remote OAuth token revocation; remote
revocation belongs to the login/logout flow implementation in later issues.

## Test strategy

CI compiles the native backend on Linux, macOS, and Windows. Automated tests use
an in-memory backend for lifecycle races because hosted runners may not expose
an interactive keychain daemon. Plaintext integration tests cover save/load,
logout, corrupt files, blocked paths, owner-only Unix permissions, and secret
redaction. A 32-request concurrency test proves that an expired identity causes
exactly one refresh.
