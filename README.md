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
