# Installing Catify

Catify ships as a native `cfy` executable. Official release archives and their
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
npm install --global catify-cli
cfy version
```

The package downloads the matching GitHub Release archive and verifies its SHA-256
checksum before installing it. Node.js 18 or newer is required for installation.
The command implementation remains the native Rust binary.

Upgrade or remove it with:

```sh
npm update --global catify-cli
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
CFY_VERSION=0.1.0 sh install.sh
```

To upgrade, rerun the installer. To uninstall a default installation:

```sh
rm ~/.local/bin/cfy
```

## Manual installation

1. Open the [latest GitHub Release](https://github.com/yan-ad/catify/releases/latest).
2. Download the archive matching the target table above and `SHA256SUMS`.
3. Verify the archive checksum.
4. Extract `cfy` (`cfy.exe` on Windows) and place it in a directory on `PATH`.
5. Run `cfy version`.

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
# Update workspace.package.version in Cargo.toml and version in package.json.
cargo test --workspace --locked
npm test
python3 scripts/check-release-version.py --tag v0.1.0
git tag v0.1.0
git push origin v0.1.0
```

The tag builds all supported archives and creates the GitHub Release first. npm
publishing is intentionally gated by the repository variable `NPM_PUBLISH=true`.

For the first npm publication:

1. Log in with `npm login` and run `CFY_SKIP_DOWNLOAD=1 npm publish --access public`.
2. On npmjs.com, configure the `catify-cli` trusted publisher with user `yan-ad`,
   repository `catify`, and workflow filename `release.yml`.
3. In GitHub repository settings, create the Actions variable `NPM_PUBLISH` with
   value `true`.

Subsequent tags publish through npm trusted publishing (OIDC), without a long-lived
`NPM_TOKEN`, after the GitHub Release assets are available.
