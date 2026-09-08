# Release and update strategy

## One-command prereleases

Run:

```bash
make release
```

The command advances the current prerelease (`0.0.1-pre.0` becomes
`0.0.1-pre.1`), synchronizes Cargo and npm versions, runs all release gates,
commits the version bump, creates an annotated tag, and atomically pushes the
commit and tag to `origin/main`.

The tag workflow builds every supported archive, publishes the GitHub Release,
and then publishes `catify-cli` to npm through npm trusted publishing (GitHub
OIDC). Prereleases use the npm `next` dist-tag; stable releases use `latest`.
The npm job verifies that every platform archive exists before publishing and
is idempotent when the exact package version is already present.

Alternative release selections:

```bash
make release BUMP=release       # 0.0.1-pre.1 -> 0.0.1
make release BUMP=patch         # 0.0.1-pre.1 -> 0.0.2
make release BUMP=minor         # 0.0.1-pre.1 -> 0.1.0
make release BUMP=major         # 0.0.1-pre.1 -> 1.0.0
make release VERSION=1.2.3-pre.0
```

Use `make release-local VERSION=<version>` to build and smoke-test artifacts
without committing, tagging, or pushing.

## Artifacts

Release builds are produced by `scripts/package-release.py`. Each target archive contains `cfy`, `VERSION`, and a small README. Archives are deterministic, accompanied by `SHA256SUMS`, and release notes are generated from the last ten commits.

Supported publishing targets are macOS and Linux. Windows packaging is implemented as a ZIP path, but publishing remains explicitly `not-published` until a Windows signing/install smoke process is established.

## npm trusted publishing

The npm package is published by `.github/workflows/release.yml`; local
`NPM_TOKEN` secrets are not required. The npm trusted publisher must match:

- repository: `yan-ad/catify`
- workflow: `release.yml`
- package: `catify-cli`

Install prereleases with:

```bash
npm install --global catify-cli@next
```

Stable releases remain available through the default `latest` dist-tag.

## Homebrew

`scripts/generate-homebrew-formula.py` generates the formula after the macOS archive URL and SHA256 are known. The formula remains a release artifact until it is submitted to a tap.

## `cfy update`

Update must not replace binaries installed by a package manager. The planned flow is:

1. Detect install provenance (managed package, Homebrew, standalone archive, or source).
2. For Homebrew/package-manager installs, print the package-manager command and exit without mutation.
3. For standalone installs, download the target archive over HTTPS, verify `SHA256SUMS`, and atomically replace the binary. On Windows, replacement is scheduled after the running process exits.

Shell-installer releases write a sibling `.catify-version` marker next to `cfy`. Catify also recognizes the legacy `~/.local/bin/cfy` plus `catify -> cfy` layout shipped before `0.0.1-pre.3`, so those installations can self-heal without manual metadata creation.
4. Require `--check` for network-only version checks and `--yes` for mutation; never run update implicitly during another command.
5. Keep update disabled in non-interactive mode unless `CFY_UPDATE_ALLOW=1` is explicitly set.

Standalone installations now use the native updater. Package-manager installs
remain managed by their package manager and receive the corresponding upgrade
instruction instead of being replaced directly.
