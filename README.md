# Crabpify

Crabpify (`cfy`) is an independent, memory-efficient CLI aiming for behavioral compatibility with common Shopify CLI workflows. It is experimental and is not affiliated with, endorsed by, or sponsored by Shopify.

## Development

```sh
cargo run -p cfy-cli -- --help
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Work is driven by the [GitHub issue roadmap](https://github.com/yan-ad/crabpify/issues). Architecture and compatibility decisions live in [`docs/adr`](docs/adr) and [`docs/compatibility.md`](docs/compatibility.md).

Configuration precedence, file locations, and persistence guarantees are
documented in [`docs/configuration.md`](docs/configuration.md).

## CLI conventions

Global flags work before or after nested commands:

- `-v, --verbose` can be repeated to increase diagnostic detail.
- `--no-color` disables ANSI color output.
- `--json` requests machine-readable output from commands that support it.
- `--non-interactive` prevents commands from prompting.

The initial compatibility aliases are `cfy a` for `cfy app`, `cfy th` for
`cfy theme`, `cfy v` for `cfy version`, and `show` for nested `info` commands.

### Output and diagnostics

`--json` reserves stdout for one machine-readable command result. Runtime errors
are emitted as JSON on stderr, so pipelines never receive human log lines on
stdout. Diagnostic logs are disabled by default; pass `-v` to enable cause and
debug details, or repeat it as future commands add finer levels.

Known Shopify token environment variables are redacted from human output, JSON
values, errors, and debug causes. Commands must pass output through the shared
`Output` boundary rather than writing directly to stdout or stderr.

Exit codes are stable by category:

| Status | Categories |
| --- | --- |
| `0` | Success |
| `1` | Shopify API and external process failures |
| `2` | Invalid input, CLI usage, and configuration failures |

Generate shell completion scripts with `cfy completion <shell>`, for example:

```sh
cfy completion bash > ~/.local/share/bash-completion/completions/cfy
cfy completion zsh > ~/.zfunc/_cfy
cfy completion fish > ~/.config/fish/completions/cfy.fish
```
