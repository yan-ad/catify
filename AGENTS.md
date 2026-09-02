# Crabpify Engineering Policy

## Native-first requirement

Crabpify is a Rust-native Shopify CLI implementation. The shipped command surface must be **majority native** and must not become a thin wrapper around the official Shopify CLI.

### Command implementation priority

1. Implement command parsing, configuration, filesystem behavior, HTTP/GraphQL transport, state machines, output, and interactive UI natively in Rust.
2. Use documented Shopify APIs where available.
3. Isolate undocumented or unstable Shopify contracts behind typed, versioned backend boundaries with fixtures and explicit diagnostics.
4. Use a subprocess adapter only when the underlying ecosystem tool is intrinsically external, such as a JavaScript bundler, Theme Check, Hydrogen, tunnel provider, or language server.
5. Delegation to the `shopify` executable must never be the default implementation for a command that can reasonably be implemented natively.

### Adapter rules

Any command that delegates to another CLI must:

- require an explicit `--delegate` or adapter selection unless the child tool is the command's actual runtime engine;
- preserve TTY, signals, exit codes, stdout, and stderr;
- have a tracked native-migration issue with the blocker documented;
- expose no credentials in argv, environment diagnostics, logs, or debug output;
- remain optional so native commands work when Shopify CLI is not installed.

### Compatibility

- Public command names and nesting must match Shopify CLI one-to-one, for example `cfy app config link`, not `cfy app config-link`.
- Compatibility tests must distinguish native implementations, external tool adapters, and explicitly blocked commands.
- A command must not return fake success. Missing contracts must produce a typed, actionable error and retain an owning issue.

### Validation

Before marking an issue complete, run focused tests plus workspace formatting, tests, and Clippy. Validate interactive flows with reducer/unit tests and a real TTY when available. Do not weaken cross-platform or security checks to make a command pass.
