# Catify CLI parity matrix

> Upstream: `@shopify/cli/4.6.1 darwin-arm64 node-v22.15.1`. Generated from `inventory/runtime-shopify-cli.json` and `inventory/cli-command-status.json`.

## Summary

- Total upstream commands: **111**
- Implemented (`native` + `adapter`): **57**
- Commands with automated evidence: **67**
- Live-verified commands: **3**

| Status | Count | Meaning |
|---|---:|---|
| `adapter` | 27 | Implemented through an explicit external runtime adapter. |
| `blocked` | 18 | Command path exists but required backend behavior is incomplete. |
| `library-only` | 8 | Core/backend exists, but the public command is not fully wired. |
| `missing` | 12 | No compatible command implementation yet. |
| `name-mismatch` | 6 | Behavior exists under a non-compatible command path. |
| `native` | 30 | Implemented in Rust and exposed at the upstream command path. |
| `partial` | 10 | Exact command path exists, but behavior is not yet fully compatible. |

## Commands

| Command | Status | Tested | Live | Owner | Implementation / gap |
|---|---|:---:|:---:|---:|---|
| `app build` | `native` | yes | no | [#26](https://github.com/yan-ad/catify/issues/26) | Rust build pipeline is exposed at the exact upstream command path; apps without extensions run without an external runtime, while extension builds use the explicit adapter protocol. Evidence: crates/cfy-build/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. |
| `app bulk cancel` | `library-only` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust backend/library exists, but the exact public CLI command is not fully wired. |
| `app bulk execute` | `library-only` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust backend/library exists, but the exact public CLI command is not fully wired. |
| `app bulk status` | `library-only` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust backend/library exists, but the exact public CLI command is not fully wired. |
| `app config link` | `native` | yes | yes | [#40](https://github.com/yan-ad/catify/issues/40) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; crates/cfy-app/src/lib.rs tests. |
| `app config pull` | `native` | yes | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; crates/cfy-app/src/lib.rs tests. |
| `app config use` | `native` | yes | no | [#24](https://github.com/yan-ad/catify/issues/24) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; crates/cfy-app/src/lib.rs tests. |
| `app config validate` | `native` | yes | no | [#24](https://github.com/yan-ad/catify/issues/24) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; crates/cfy-app/src/lib.rs tests. |
| `app deploy` | `library-only` | no | no | [#27](https://github.com/yan-ad/catify/issues/27) | Rust backend/library exists, but the exact public CLI command is not fully wired. |
| `app dev` | `library-only` | no | no | [#29](https://github.com/yan-ad/catify/issues/29) | Rust backend/library exists, but the exact public CLI command is not fully wired. |
| `app dev clean` | `library-only` | no | no | [#29](https://github.com/yan-ad/catify/issues/29) | Rust backend/library exists, but the exact public CLI command is not fully wired. |
| `app env pull` | `native` | yes | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; crates/cfy-config/src/app_env.rs tests. |
| `app env show` | `native` | yes | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; crates/cfy-config/src/app_env.rs tests. |
| `app execute` | `blocked` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app function build` | `blocked` | no | no | [#25](https://github.com/yan-ad/catify/issues/25) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app function info` | `blocked` | no | no | [#25](https://github.com/yan-ad/catify/issues/25) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app function replay` | `blocked` | no | no | [#25](https://github.com/yan-ad/catify/issues/25) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app function run` | `blocked` | no | no | [#25](https://github.com/yan-ad/catify/issues/25) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app function schema` | `blocked` | no | no | [#25](https://github.com/yan-ad/catify/issues/25) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app function typegen` | `blocked` | no | no | [#25](https://github.com/yan-ad/catify/issues/25) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app generate extension` | `library-only` | no | no | [#24](https://github.com/yan-ad/catify/issues/24) | Rust backend/library exists, but the exact public CLI command is not fully wired. |
| `app graphiql` | `blocked` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app import-custom-data-definitions` | `blocked` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app import-extensions` | `library-only` | no | no | [#24](https://github.com/yan-ad/catify/issues/24) | Rust backend/library exists, but the exact public CLI command is not fully wired. |
| `app info` | `partial` | yes | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs. Project discovery works, but full app and extension details are incomplete. |
| `app init` | `partial` | yes | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs. Creates a minimal local skeleton; template selection/dependency setup are incomplete. |
| `app logs` | `blocked` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app logs sources` | `blocked` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `app release` | `native` | yes | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-app/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. |
| `app versions list` | `native` | yes | no | [#40](https://github.com/yan-ad/catify/issues/40) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-app/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. |
| `app webhook trigger` | `blocked` | no | no | [#40](https://github.com/yan-ad/catify/issues/40) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `auth login` | `native` | yes | yes | [#37](https://github.com/yan-ad/catify/issues/37) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-auth/tests/auth.rs; crates/cfy-cli/tests/cli.rs. |
| `auth logout` | `partial` | yes | yes | [#37](https://github.com/yan-ad/catify/issues/37) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-auth/tests/auth.rs; crates/cfy-cli/tests/cli.rs. Local credential deletion exists; remote OAuth revocation is not implemented. |
| `commands` | `partial` | yes | no | [#36](https://github.com/yan-ad/catify/issues/36) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; compatibility/scenarios.json. Listing exists but does not yet mirror the full 111-command upstream table and columns. |
| `config autocorrect off` | `missing` | no | no | [#42](https://github.com/yan-ad/catify/issues/42) | No compatible Catify command implementation exists yet. |
| `config autocorrect on` | `missing` | no | no | [#42](https://github.com/yan-ad/catify/issues/42) | No compatible Catify command implementation exists yet. |
| `config autocorrect status` | `missing` | no | no | [#42](https://github.com/yan-ad/catify/issues/42) | No compatible Catify command implementation exists yet. |
| `config autoupgrade off` | `native` | yes | no | [#42](https://github.com/yan-ad/catify/issues/42) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-config/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. |
| `config autoupgrade on` | `native` | yes | no | [#42](https://github.com/yan-ad/catify/issues/42) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-config/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. |
| `config autoupgrade status` | `native` | yes | no | [#42](https://github.com/yan-ad/catify/issues/42) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-config/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. |
| `doc fetch` | `native` | yes | no | [#41](https://github.com/yan-ad/catify/issues/41) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-docs/src/lib.rs tests; compatibility/scenarios.json. |
| `doc search` | `native` | yes | no | [#41](https://github.com/yan-ad/catify/issues/41) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-docs/src/lib.rs tests; compatibility/scenarios.json. |
| `help` | `native` | yes | no | [#36](https://github.com/yan-ad/catify/issues/36) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; compatibility/scenarios.json. |
| `hydrogen build` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen check` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen codegen` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen customer-account-push` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen debug cpu` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen deploy` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen dev` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen env list` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen env pull` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen env push` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen generate route` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen generate routes` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen init` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen link` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen list` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen login` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen logout` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen preview` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen setup` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen setup css` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen setup markets` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen setup vite` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen shortcut` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen unlink` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `hydrogen upgrade` | `adapter` | yes | no | [#43](https://github.com/yan-ad/catify/issues/43) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-hydrogen/src/lib.rs tests. |
| `organization list` | `blocked` | no | no | [#37](https://github.com/yan-ad/catify/issues/37) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `plugins add` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `plugins inspect` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `plugins install` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `plugins link` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `plugins remove` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `plugins reset` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `plugins uninstall` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `plugins unlink` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `plugins update` | `missing` | no | no | [#45](https://github.com/yan-ad/catify/issues/45) | No compatible Catify command implementation exists yet. |
| `search` | `native` | yes | no | [#41](https://github.com/yan-ad/catify/issues/41) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-docs/src/lib.rs tests; compatibility/scenarios.json. |
| `store auth` | `blocked` | no | no | [#38](https://github.com/yan-ad/catify/issues/38) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `store auth list` | `name-mismatch` | no | no | [#38](https://github.com/yan-ad/catify/issues/38) | Related behavior exists under a flat or otherwise incompatible command path. Rename/nesting must be corrected before this counts as parity. |
| `store bulk cancel` | `name-mismatch` | no | no | [#38](https://github.com/yan-ad/catify/issues/38) | Related behavior exists under a flat or otherwise incompatible command path. Rename/nesting must be corrected before this counts as parity. |
| `store bulk execute` | `name-mismatch` | no | no | [#38](https://github.com/yan-ad/catify/issues/38) | Related behavior exists under a flat or otherwise incompatible command path. Rename/nesting must be corrected before this counts as parity. |
| `store bulk status` | `name-mismatch` | no | no | [#38](https://github.com/yan-ad/catify/issues/38) | Related behavior exists under a flat or otherwise incompatible command path. Rename/nesting must be corrected before this counts as parity. |
| `store create preview` | `name-mismatch` | no | no | [#38](https://github.com/yan-ad/catify/issues/38) | Related behavior exists under a flat or otherwise incompatible command path. Rename/nesting must be corrected before this counts as parity. |
| `store execute` | `native` | yes | no | [#38](https://github.com/yan-ad/catify/issues/38) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-store/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. |
| `store graphiql` | `partial` | yes | no | [#38](https://github.com/yan-ad/catify/issues/38) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-store/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. Builds a URL but does not yet launch a fully authenticated GraphiQL session. |
| `store info` | `native` | yes | no | [#38](https://github.com/yan-ad/catify/issues/38) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-store/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. |
| `store list` | `blocked` | no | no | [#38](https://github.com/yan-ad/catify/issues/38) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `store open` | `partial` | yes | no | [#38](https://github.com/yan-ad/catify/issues/38) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-store/src/lib.rs tests; crates/cfy-cli/tests/cli.rs. Builds the Admin URL but does not yet reproduce the complete browser/auth workflow. |
| `theme check` | `adapter` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; crates/cfy-cli/src/theme_check.rs tests. |
| `theme console` | `blocked` | no | no | [#39](https://github.com/yan-ad/catify/issues/39) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `theme delete` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme dev` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme duplicate` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme info` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme init` | `partial` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. Creates a minimal directory instead of cloning/selecting an upstream theme template. |
| `theme language-server` | `adapter` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | External runtime adapter is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; crates/cfy-cli/src/theme_check.rs tests. |
| `theme list` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme metafields pull` | `name-mismatch` | no | no | [#39](https://github.com/yan-ad/catify/issues/39) | Related behavior exists under a flat or otherwise incompatible command path. Rename/nesting must be corrected before this counts as parity. |
| `theme open` | `partial` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. Returns a preview URL but does not yet reproduce all browser/environment behavior. |
| `theme package` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme preview` | `blocked` | no | no | [#39](https://github.com/yan-ad/catify/issues/39) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `theme profile` | `blocked` | no | no | [#39](https://github.com/yan-ad/catify/issues/39) | Command path exists, but execution still returns an actionable backend-unavailable error. |
| `theme publish` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme pull` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme push` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme rename` | `native` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. |
| `theme share` | `partial` | yes | no | [#39](https://github.com/yan-ad/catify/issues/39) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-api/src/theme.rs tests; crates/cfy-cli/tests/cli.rs. Returns an existing preview URL instead of creating a new randomized unpublished theme. |
| `upgrade` | `partial` | yes | no | [#36](https://github.com/yan-ad/catify/issues/36) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; compatibility/scenarios.json. Safe channel checks exist, but end-to-end self-upgrade is not complete for every install method. |
| `version` | `native` | yes | no | [#36](https://github.com/yan-ad/catify/issues/36) | Rust implementation is exposed at the exact upstream command path. Evidence: crates/cfy-cli/tests/cli.rs; compatibility/scenarios.json. |
