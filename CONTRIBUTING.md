# Contributing to Catify

Thanks for helping build Catify. This project aims to provide a native-first, memory-efficient CLI with observable command compatibility for Shopify development workflows.

By participating, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Before opening a change

1. Search the [issue tracker](https://github.com/yan-ad/catify/issues) and the [CLI parity matrix](inventory/CLI-PARITY.md).
2. Comment on the owning issue or open a focused issue before starting a large command, API contract, or architecture change.
3. Keep the scope small enough to implement, review, and verify independently.
4. Never use a merchant production store for destructive testing. Prefer fixtures, mock servers, temporary directories, development stores, and disposable apps.

## Native-first policy

Catify must remain majority native and must not become a thin wrapper around Shopify CLI.

Implement these concerns in Rust whenever practical:

- command parsing and exact command nesting;
- project discovery and configuration precedence;
- filesystem transactions, validation, and rollback;
- HTTP, REST, and GraphQL transport;
- state machines, retries, cancellation, and cleanup;
- human and JSON output;
- interactive selectors and terminal interfaces.

An external process is appropriate only when it is the workflow's actual runtime engine, such as a package manager, Git, Hydrogen, Theme Check, a language server, cloudflared, a bundler, or the Shopify Functions WASM runner.

An adapter must:

- remain optional where native commands do not require it;
- preserve TTY input, signals, exit status, stdout, and stderr;
- avoid credentials in argv, logs, errors, and diagnostics;
- have typed discovery and version checks;
- produce an actionable error when unavailable;
- never silently delegate a command that can reasonably be implemented natively.

See [`AGENTS.md`](AGENTS.md) and the [compatibility policy](docs/compatibility.md) for the full engineering rules.

## Development setup

Requirements:

- Rust 1.94 or newer;
- Git;
- Python 3 for repository tooling;
- Node.js and npm only for npm packaging tests or JavaScript runtime fixtures.

Clone and validate the workspace:

```sh
git clone https://github.com/yan-ad/catify.git
cd catify
cargo build -p cfy-cli --bin cfy
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Run the CLI from source:

```sh
cargo run -p cfy-cli --bin cfy -- --help
cargo run -p cfy-cli --bin cfy -- app info
```

The repository also exposes the `catify` binary name. Tests and documentation should remain valid for both `cfy` and `catify`, including the `.exe` suffix on Windows.

## Implementing a command

Public command names and nesting must match Shopify CLI one-to-one:

```text
cfy app config link     # correct
cfy app config-link     # incorrect
```

For each command:

1. Capture the upstream command path, flags, defaults, exit behavior, and non-interactive behavior.
2. Identify whether the contract is public, internal-but-versioned, or intrinsically external.
3. Add the Rust domain/service boundary before adding large orchestration logic to `cfy-cli`.
4. Use typed errors and the shared output boundary.
5. Add unit tests for reducers, parsers, transforms, and state machines.
6. Add integration or fixture tests for filesystem and HTTP behavior.
7. Use live verification only for safe read-only operations or explicitly approved development resources.
8. Update the command's GitHub issue with real evidence and remaining limitations.

A command must not return fake success. Unsupported contracts need a typed, actionable failure and an owning issue.

## CLI parity updates are required

Every public command change must update the parity source and regenerate both reports:

- `inventory/cli-command-status.json`
- `inventory/CLI-PARITY.md`
- `inventory/CLI-PARITY.json`

The status must be honest:

- `native`: the public behavior is implemented in Rust;
- `adapter`: Catify owns orchestration but invokes an intrinsic external runtime;
- `partial`: the exact command exists, but observable behavior still has documented gaps.

Do not mark a command as tested or live-verified without naming its evidence. Run the parity generator and stale checks used by the repository before committing.

## Tests and validation

Start with focused validation for the affected crates:

```sh
cargo test -p cfy-app -p cfy-cli
cargo clippy -p cfy-app -p cfy-cli --all-targets -- -D warnings
```

Before handing off a completed command or pull request, run:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
make release-check
```

When relevant, also run:

- compatibility scenarios in `compatibility/`;
- inventory and command-owner checks in `scripts/`;
- `make release VERSION=<version>` for packaging changes;
- a real TTY flow for selectors, raw input, browser login, and signal handling;
- platform-specific tests for path handling, CRLF output, executable suffixes, and installer behavior.

Do not weaken a cross-platform, security, or parity assertion merely to make a test pass.

## Security expectations

- Never commit tokens, cookies, app secrets, store passwords, private keys, or captured merchant data.
- Use sensitive-header APIs and secret wrapper types.
- Keep credentials out of process arguments and public JSON structures.
- Bind callback and GraphiQL servers to loopback only and use CSRF/session guards.
- Validate all paths before writes, reject traversal and symlink escapes, and use atomic transactions with rollback.
- Require explicit approval for live-theme changes, deletions, releases, mutations, and other destructive operations.
- Redact secrets from error chains and debug output, not only from success output.

If you discover a vulnerability, do not open a public issue containing exploit details. Contact the maintainers privately through the contact information on their GitHub profiles.

## Documentation

Update documentation when a change affects:

- installation or release packaging;
- public command names, flags, JSON output, or exit codes;
- environment variables or configuration precedence;
- credential storage and security behavior;
- compatibility or performance claims.

User-facing examples should use `cfy` as the primary short command and can mention `catify` as the equivalent long command.

## Pull requests

A pull request should include:

- a concise explanation of the problem and root cause;
- the implementation approach and native/external boundary;
- focused and workspace validation performed;
- parity status changes;
- screenshots or terminal recordings for meaningful TUI changes;
- known limitations or live verification still required.

Keep unrelated refactors out of the pull request. Do not commit generated build directories, credentials, personal Shopify configuration, or local benchmark noise.

## Releases

Release preparation uses the repository Makefile:

```sh
make release VERSION=0.0.1-pre.0
```

This validates versions, builds artifacts, packages deterministic archives, and performs extracted-artifact and installer smoke tests. Creating or pushing a release tag must remain an explicit maintainer action.

See [docs/release.md](docs/release.md) for the complete release process.
