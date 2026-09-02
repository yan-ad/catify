# Compatibility and upstream reference policy

Catify is an independent project. Compatibility means matching documented observable behavior, not reproducing Shopify CLI internals.

## Compatibility levels

Each inventory command has one implementation classification and may report these compatibility dimensions:

| Level | Command and flags | Exit codes | Output |
|---|---|---|---|
| Exact | Same public invocation and required/optional semantics | Same documented/observed code | Normalized text or JSON snapshot matches |
| Functional | Same user outcome; a documented flag may be deferred | Same success/failure class | Semantically equivalent, wording may differ |
| Partial | Only listed paths are supported | Stable Catify mapping | Catify-specific diagnostic identifies gap |
| None | Command is rejected | `2` for unsupported usage | Clear unsupported message |

Implementation classifications are `native`, `adapter-backed`, `deferred`, and `unsupported`. `deferred` is the default for inventoried commands until an issue accepts another status.

## Exit-code classes

- `0`: success.
- `1`: operational failure such as network, Shopify API, filesystem, or child process.
- `2`: invalid invocation, invalid configuration, or unsupported behavior.
- `130`: interrupted by the user where the platform permits preserving it.

## Snapshot normalization

Compatibility tests must normalize only unstable data:

- replace timestamps, durations, UUIDs, request IDs, ports, and temporary paths with named tokens;
- replace home and repository roots with `<HOME>` and `<REPO>`;
- normalize `\\` to `/` for path comparisons and normalize line endings to LF;
- remove ANSI only when testing non-color output; otherwise compare semantic style tokens;
- sort map/object keys only where the public format does not promise order;
- redact tokens, store names, organization IDs, and user data;
- never remove meaningful command names, flags, error categories, or user-action guidance.

Every normalization rule belongs in the test harness and requires review; snapshots must not use broad regexes that can hide regressions.

## MIT upstream use

Shopify CLI is MIT licensed. Public behavior, documentation, tests, and source may be studied. Copied or substantially adapted code must retain its upstream copyright and MIT notice in the relevant file or `THIRD_PARTY_NOTICES.md`, with source path and commit. Prefer clean, independent implementations from observed behavior.

The command inventory records the exact upstream commit. Dependency licenses are checked in CI with `cargo-deny`; only approved permissive licenses are allowed without maintainer review. Certificate root data used by the Rustls platform verifier is licensed under CDLA-Permissive-2.0. This data-specific permissive license is explicitly allowed in `deny.toml`; it is not a blanket exception for other CDLA licenses. The native filesystem watcher crate `notify` uses CC0-1.0, a public-domain dedication with a permissive fallback; that exact license is allowed for the watcher dependency rather than as permission to bypass dependency review.

## Naming and trademarks

`Catify` and `cfy` are the project and executable names. Do not name releases or packages “Shopify CLI,” use Shopify logos, or imply endorsement. “Shopify” may be used descriptively to explain interoperability. User-facing documentation must retain the independent-project disclaimer.
