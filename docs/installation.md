# Installing Catify

The project and npm package are named `catify-cli`. It exposes both `catify` and
the shorter `cfy` command; both execute the same native Rust binary. Official release archives and their
`SHA256SUMS` manifest are published on GitHub for every `v*` tag.

## Supported platforms

| Platform | Architecture | Release target |
| --- | --- | --- |
| macOS | Apple Silicon | `aarch64-apple-darwin` |
| macOS | Intel | `x86_64-apple-darwin` |
| Linux (glibc) | x64 | `x86_64-unknown-linux-gnu` |
| Linux (glibc) | arm64 | `aarch64-unknown-linux-gnu` |
| Windows | x64 | `x86_64-pc-windows-msvc` |

## Install with npm

```sh
npm install --global catify-cli@next
cfy version
catify version
```

Catify prereleases are published under npm's `next` dist-tag. The existing
`catify-cli@0.0.1` package predates the matching GitHub release and cannot install;
use the shell installer until `0.0.1-pre.0` is published.

The package downloads the matching GitHub Release archive and verifies its SHA-256
checksum before installing it. Node.js 18 or newer is required for installation.
The command implementation remains the native Rust binary.

Upgrade or remove it with:

```sh
npm update --global catify-cli@next
npm uninstall --global catify-cli
```

## Install with the shell installer

macOS and Linux users can run:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/yan-ad/catify/main/install.sh | sh
```

The default destination is `${XDG_BIN_HOME:-$HOME/.local/bin}`. If that directory
is not already in `PATH`, add it to your shell profile. Common overrides are:

```sh
# Install into a system-wide directory.
CFY_INSTALL_DIR=/usr/local/bin sh install.sh

# Install an exact version.
CFY_VERSION=0.0.1-pre.0 sh install.sh
```

The CLI checks GitHub Releases in the background at most once every 24 hours and
prints a short notice when an upgrade is available. It never replaces the binary
without an explicit upgrade command. Disable or re-enable the check with:

```sh
cfy config autoupgrade off
cfy config autoupgrade on
```

Set `CFY_NO_UPDATE_CHECK=1` for a one-off command that must not check. JSON,
non-interactive, completion, and CI invocations skip the check automatically.

To upgrade a shell installation, rerun the installer. To uninstall a default installation:

```sh
rm ~/.local/bin/cfy
rm ~/.local/bin/catify
```

## Manual installation

1. Open the [GitHub Releases page](https://github.com/yan-ad/catify/releases) and select the newest release or prerelease.
2. Download the archive matching the target table above and `SHA256SUMS`.
3. Verify the archive checksum.
4. Extract `cfy`/`catify` (`.exe` on Windows) and place them in a directory on `PATH`.
5. Run `cfy version`.

## Install with Cargo

```sh
cargo install cfy-cli --locked
cfy version
catify version
```

Cargo installs both command names from the same `catify-cli` package.

Example checksum verification:

```sh
sha256sum -c SHA256SUMS --ignore-missing # Linux
shasum -a 256 cfy-v*.tar.gz              # macOS; compare with SHA256SUMS
```

## Publishing releases and npm

The release workflow requires the Git tag, Cargo workspace version, and npm package
version to match. Prepare a release by updating both version fields, validating the
workspace, and pushing the tag:

```sh
make release
```

The command bumps the prerelease, validates and packages it, commits the
version files, creates the annotated tag, and atomically pushes `main` and the
tag.

The tag builds all supported archives and creates the GitHub Release first. npm
publishing verifies those exact assets before upload and is intentionally gated by
the repository variable `NPM_PUBLISH=true`.

For the first npm publication:

1. Log in with `npm login` and run `CFY_SKIP_DOWNLOAD=1 npm publish --access public`.
2. On npmjs.com, configure the `catify-cli` trusted publisher with user `yan-ad`,
   repository `catify`, and workflow filename `release.yml`.
3. In GitHub repository settings, create the Actions variable `NPM_PUBLISH` with
   value `true`.

Subsequent tags publish through npm trusted publishing (OIDC), without a long-lived
`NPM_TOKEN`, after the GitHub Release assets are available.
