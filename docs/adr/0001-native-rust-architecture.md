# ADR 0001: Native Rust architecture with explicit adapters

- Status: Accepted
- Date: 2026-08-13
- Owners: Crabpify maintainers

## Context

Shopify CLI provides a broad command surface but its Node.js process and dependency graph can impose noticeable startup and idle-memory costs. Crabpify needs a small native core while retaining a path to behavioral compatibility with workflows that inherently invoke JavaScript tools.

## Decision

Use Rust for the `cfy` executable and organize the workspace around stable ports:

- `cfy-cli`: parsing, dispatch, output selection, and exit-code mapping only.
- `cfy-core`: domain primitives and errors that may be shared without I/O coupling.
- `cfy-config`: typed TOML/config parsing and future discovery rules.
- `cfy-api`: Shopify GraphQL/HTTP ports; concrete clients remain adapters.
- `cfy-process`: the only general subprocess boundary.
- Future domain crates (`cfy-app`, `cfy-theme`, `cfy-auth`) own workflows and depend on ports, not platform implementations.

`tokio` is the async runtime because commands need concurrent network, filesystem, signal, and child-process work. Synchronous commands must not create extra runtimes.

Errors are typed inside crates and converted at the CLI boundary into stable exit codes and user-facing diagnostics. Raw provider errors may be retained for debug output but are not a stable public interface.

Public stability boundaries are command names, documented flags/environment variables, normalized machine output, exit-code classes, and config formats. Crate internals and adapter protocols are unstable until explicitly promoted.

## Toolchain boundaries

Node.js is not embedded. JavaScript-heavy tools are spawned through `cfy-process` when native replacement would reduce compatibility or duplicate an ecosystem:

- application package-manager scripts and dev servers;
- esbuild or extension-specific compilers;
- Hydrogen/Oxygen tooling;
- Theme Check language server until a compatible native implementation exists;
- cloudflared or user-selected tunnel binaries.

Adapters must declare required executable/version, sanitize inherited environment, forward cancellation, and make child memory visible in benchmarks. A command classified `adapter-backed` must still keep orchestration and Shopify API interactions native where practical.

## Dependency policy

Prefer the standard library and small, maintained, auditable crates. New dependencies require a concrete feature need, compatible license, minimal enabled features, and review of transitive cost. Avoid framework-style SDKs when a narrow protocol crate suffices. Pin the Rust toolchain and commit `Cargo.lock`. Run license/advisory checks in CI.

## Alternatives considered

### Keep TypeScript/Node

Highest code reuse and easiest upstream synchronization, but it does not address the primary startup/RSS objective and preserves the large runtime dependency surface.

### Go

Go offers fast builds, simple distribution, good networking, and easier onboarding. Its garbage-collected runtime has less deterministic idle memory and larger baseline binaries than a carefully configured Rust CLI. Rust also provides stronger ownership guarantees for long-running process/watcher orchestration. Go remains a credible fallback if Rust delivery velocity becomes the dominant risk.

### Fully native rewrite with no subprocesses

Potentially lowest aggregate resource use, but incompatible with arbitrary app package scripts and costly to reproduce fast-changing JS compilers. Rejected in favor of explicit, measurable adapters.

## Consequences

Initial delivery is slower than a TypeScript fork, but the core has no Node runtime requirement. Some workflows still consume child-process memory; benchmark reports must separate `cfy` RSS from process-tree RSS. Compatibility is delivered incrementally according to the committed inventory.
