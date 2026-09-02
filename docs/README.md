# Catify beta documentation

Catify (`cfy`) is a Rust-based, memory-conscious Shopify CLI alternative. The repository is private during beta and is not yet a drop-in replacement for every authenticated Shopify workflow.

## Install from a release artifact

Download the archive for your platform, verify it against `SHA256SUMS`, extract `cfy`, and put it on `PATH`:

```bash
shasum -a 256 -c SHA256SUMS
mkdir -p ~/.local/bin
tar -xzf cfy-vVERSION-TARGET.tar.gz
install cfy-vVERSION-TARGET/cfy ~/.local/bin/cfy
cfy version
```

Release artifacts, Homebrew generation, and update policy are documented in [`release.md`](release.md). Windows packaging is not published yet.

## Build from source

Requirements:

- Rust **1.94.0+** and rustup components `rustfmt`/`clippy`.
- Python 3 for inventory/compatibility/release helper scripts.
- Node/Ruby are not required for native core commands, but delegated Theme Check/Hydrogen workflows may require Shopify CLI, Node, or their upstream toolchain.

```bash
cargo build -p cfy-cli
./target/debug/cfy --help
```

## Migration from `shopify`

Start with read-only/local commands:

```bash
cfy help
cfy commands --json
cfy doctor env --json
cfy doctor project --json
cfy theme list --store STORE.myshopify.com
cfy theme pull --store STORE.myshopify.com --path ./theme
```

Keep the Shopify CLI installed for authenticated app/store workflows until the open parity issues are complete. Do not assume identical exit-code text, Node plugin behavior, or OAuth client configuration.

## Current command coverage

The pinned Shopify CLI 4.6.1 runtime inventory contains **111 commands**. The compatibility inventory and ownership report are generated with:

```bash
python3 scripts/generate-inventory.py
python3 scripts/check-command-classifications.py \
  inventory/runtime-shopify-cli.json \
  --report inventory/command-classification-report.json
```

Implemented native areas include core diagnostics, config/cache, theme pull/push/dev/list, app project/build/deploy foundations, project graph, process supervision, docs search/fetch, Hydrogen delegation, release packaging, and compatibility reporting.

Open parity work is tracked in GitHub issues **#37–#40**:

- #37 authentication and organization live provider
- #38 complete store backend operations
- #39 remaining theme remote operations
- #40 remaining app backend operations

These are intentionally visible gaps, not silently reported as complete.

## Troubleshooting

- `interactive login is not available`: use `SHOPIFY_CLI_TOKEN=... cfy auth login --non-interactive` while the production Identity provider is being finalized. `CFY_IDENTITY_CLIENT_ID` is currently an advanced development override, not a normal user requirement.
- `Hydrogen tooling is not installed`: install the upstream Hydrogen toolchain or set `CFY_HYDROGEN_BIN` to an explicit executable.
- `Theme Check ... not installed`: set `CFY_THEME_CHECK_BIN` or install Shopify CLI/Theme Check. The adapter never recursively invokes itself.
- `backend is not configured`: the command surface exists, but the authenticated API adapter is still tracked by an open parity issue.
- `cfy doctor env --json` and `cfy doctor project --json` provide diagnostic context without exposing tokens.

## Security and bug reports

Never put Shopify tokens, OAuth client secrets, cookies, or private store data in issues or logs. Catify redacts sensitive headers, token fields, and error causes, but reports should still be minimized.

For a bug report, include:

1. `cfy version --json` and `cfy doctor env --json` output.
2. The exact command with secrets/store identifiers removed.
3. Expected versus actual exit code/output.
4. A minimal reproducible fixture or sanitized log.

Report security vulnerabilities privately to the repository maintainers rather than opening a public issue. Do not publish an exploit before a fix and release are coordinated.

## Contributing

Run the local gates before pushing:

```bash
cargo fmt --all -- --check
cargo test --workspace -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
python3 compatibility/run.py --cfy ./target/debug/cfy
```

Remote CI may be intentionally skipped during early development, but issue comments must state that clearly. Never close a parity issue when only a command parser exists; unsupported backends must remain explicit.
