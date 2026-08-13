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

## CLI conventions

Global flags work before or after nested commands:

- `-v, --verbose` can be repeated to increase diagnostic detail.
- `--no-color` disables ANSI color output.
- `--json` requests machine-readable output from commands that support it.
- `--non-interactive` prevents commands from prompting.

The initial compatibility aliases are `cfy a` for `cfy app`, `cfy th` for
`cfy theme`, `cfy v` for `cfy version`, and `show` for nested `info` commands.
Parse and usage errors exit with status `2`; API and child-process failures exit
with status `1`.

Generate shell completion scripts with `cfy completion <shell>`, for example:

```sh
cfy completion bash > ~/.local/share/bash-completion/completions/cfy
cfy completion zsh > ~/.zfunc/_cfy
cfy completion fish > ~/.config/fish/completions/cfy.fish
```
