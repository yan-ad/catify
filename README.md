# Catify

[![CI](https://github.com/yan-ad/catify/actions/workflows/ci.yml/badge.svg)](https://github.com/yan-ad/catify/actions/workflows/ci.yml)
[![Release](https://github.com/yan-ad/catify/actions/workflows/release.yml/badge.svg)](https://github.com/yan-ad/catify/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

Catify is an independent, native-first command-line interface for Shopify development. The project ships the `cfy` and `catify` commands from the same Rust binary and targets behavioral compatibility with Shopify CLI while using substantially less memory at rest.

With Catify, you can:

- initialize, inspect, build, develop, deploy, and release Shopify apps;
- work with extensions and Shopify Functions;
- authenticate with Shopify and manage organizations and stores;
- pull, push, preview, profile, package, and develop Liquid themes;
- search and fetch Shopify developer documentation;
- invoke Hydrogen, Theme Check, and language-server runtimes through isolated adapters when the ecosystem tool itself is intrinsically external.

> [!IMPORTANT]
> Catify is experimental prerelease software. It is not affiliated with, endorsed by, or sponsored by Shopify. Test commands against development stores and source-controlled projects before using them in production workflows.

## Before you begin

You need a Shopify account and a supported platform:

- macOS on Apple Silicon or Intel;
- Linux on x86-64 or arm64;
- Windows on x86-64.

Node.js is not required by the native Rust command core. It can still be required during npm installation or by external project runtimes such as Hydrogen, JavaScript bundlers, and extension toolchains.

## Install Catify

### npm

```sh
npm install --global catify-cli@next
cfy version
```

Catify prereleases use npm's `next` tag. The `0.0.1-pre.0` package still needs to be published; use the shell installer below until `catify-cli@next` is available. The npm package downloads the native binary for the current platform. `catify version` is equivalent to `cfy version`.

### Shell installer

On macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/yan-ad/catify/main/install.sh | sh
```

The shell installer selects the latest stable release when one exists, otherwise it installs the newest prerelease. Pin a release with `CFY_VERSION=0.0.1-pre.0`.

### Cargo

```sh
cargo install cfy-cli --locked
cfy version
```

See the [installation guide](docs/installation.md) for manual downloads, checksums, upgrades, uninstall instructions, supported targets, and prerelease packaging.

## Develop Shopify apps

Authenticate, initialize an app, and start development:

```sh
cfy auth login
cfy app init
cd my-app
cfy app dev
```

Common native workflows include:

```sh
cfy app config link
cfy app config validate
cfy app info
cfy app build
cfy app deploy
cfy app versions list
```

Catify keeps public command names and nesting aligned with Shopify CLI. For example, the command is `cfy app config link`, not `cfy app config-link`.

## Develop Shopify themes

Authenticate to a store and inspect its themes:

```sh
cfy store auth --store example.myshopify.com --scopes read_themes,write_themes
cfy theme list --store example.myshopify.com
```

Develop or synchronize a theme:

```sh
cfy theme pull --store example.myshopify.com --theme 123456789
cfy theme dev --store example.myshopify.com
cfy theme push --store example.myshopify.com --theme 123456789
```

Theme writes use path validation, staged filesystem transactions, rollback, cancellation handling, and explicit protection for live or destructive operations.

## Hydrogen and ecosystem tools

Hydrogen remains a JavaScript ecosystem runtime. Catify exposes the compatible command surface while supervising the external runtime rather than loading it into the Rust CLI process:

```sh
cfy hydrogen dev
cfy hydrogen build
```

The same boundary applies to tools such as Theme Check, the Liquid language server, cloudflared, Git, package managers, and the Shopify Functions WASM runner. Catify owns command parsing, configuration, process lifecycle, signals, output, and diagnostics; the external tool is used only as its actual runtime engine.

## Compatibility

The generated [CLI parity matrix](inventory/CLI-PARITY.md) tracks every command in the pinned Shopify CLI runtime inventory, including implementation type, automated evidence, live verification, remaining gaps, and owning issue.

| Compatibility status | Commands | Share |
|---|---:|---:|
| Native Rust | **82** | **73.9%** |
| Explicit external-runtime adapter | **27** | **24.3%** |
| Partial compatibility | **2** | **1.8%** |
| Exposed upstream command paths | **111 / 111** | **100%** |
| Fully implemented (`native` + `adapter`) | **109 / 111** | **98.2%** |

Adapter commands are not thin default proxies to the `shopify` executable. They are reserved for runtimes that are intrinsically external, such as Hydrogen, Theme Check, and language servers. See [compatibility policy](docs/compatibility.md) for the implementation rules.

## Performance

Latest checked-in benchmark on macOS arm64 against Shopify CLI 4.7.1:

| Metric | Catify | Shopify CLI | Difference |
|---|---:|---:|---:|
| Warm startup median | **10.0 ms** | 936.1 ms | **93.6× faster** |
| Peak RSS | **8.7 MiB** | 215.8 MiB | **24.7× lower** |
| Idle RSS | **8.8 MiB** | 96.6 MiB | **11.0× lower** |
| Installed binary/package size | **17 MiB** | 47 MiB | **2.8× smaller** |

The benchmark measures the CLI process, not every child process in an application workflow. A framework dev server, bundler, tunnel, Hydrogen runtime, or language server still consumes memory while active.

Raw measurements are in [`benchmarks/results/latest.json`](benchmarks/results/latest.json). Methodology and limitations are documented in the [performance report](docs/performance.md).

## Commands and help

```sh
cfy help
cfy commands
cfy help app
cfy app --help
```

Useful global options:

- `--verbose` increases diagnostic detail and can be repeated;
- `--no-color` disables ANSI output;
- `--json` requests machine-readable output where supported;
- `--non-interactive` prevents prompts and requires explicit destructive-operation approval.

Generate shell completions with:

```sh
cfy completion bash
cfy completion zsh
cfy completion fish
cfy completion powershell
```

Exit codes are stable by category:

| Code | Meaning |
|---:|---|
| `0` | Command succeeded |
| `1` | Shopify API, network, or external-runtime failure |
| `2` | Invalid input, CLI usage, or configuration failure |

## Documentation

- [Installation](docs/installation.md)
- [Configuration](docs/configuration.md)
- [CLI parity matrix](inventory/CLI-PARITY.md)
- [Compatibility policy](docs/compatibility.md)
- [Architecture decisions](docs/adr)
- [Release process](docs/release.md)
- [Performance report](docs/performance.md)
- [Public-readiness checklist](docs/public-readiness.md)

## Help and feedback

- Run `cfy doctor env` to inspect the local environment.
- Run `cfy doctor project` inside a Shopify project.
- Search [existing issues](https://github.com/yan-ad/catify/issues).
- Open an issue with the command, sanitized output, platform, Catify version, and expected Shopify CLI behavior.

Never include access tokens, app client secrets, session cookies, private store data, or unredacted environment dumps in an issue.

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request and follow the [Code of Conduct](CODE_OF_CONDUCT.md).

Catify is native-first. New public commands should implement parsing, configuration, filesystem behavior, transport, state, output, and interaction in Rust. External adapters are accepted only when the child tool is the command's actual runtime engine.

## License

Catify is available under the [MIT License](LICENSE).
