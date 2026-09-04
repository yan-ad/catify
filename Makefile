SHELL := /bin/bash
.SHELLFLAGS := -euo pipefail -c

PYTHON ?= python3
VERSION ?= 0.0.1-pre.0
TAG ?= v$(VERSION)
DIST_DIR ?= dist

UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)
ifeq ($(UNAME_S)-$(UNAME_M),Darwin-arm64)
RELEASE_TARGET := aarch64-apple-darwin
else ifeq ($(UNAME_S)-$(UNAME_M),Darwin-x86_64)
RELEASE_TARGET := x86_64-apple-darwin
else ifeq ($(UNAME_S)-$(UNAME_M),Linux-aarch64)
RELEASE_TARGET := aarch64-unknown-linux-gnu
else ifeq ($(UNAME_S)-$(UNAME_M),Linux-arm64)
RELEASE_TARGET := aarch64-unknown-linux-gnu
else ifeq ($(UNAME_S)-$(UNAME_M),Linux-x86_64)
RELEASE_TARGET := x86_64-unknown-linux-gnu
else
RELEASE_TARGET := unsupported
endif

ARCHIVE := $(DIST_DIR)/cfy-v$(VERSION)-$(RELEASE_TARGET).tar.gz

.PHONY: help release release-check release-build release-package release-smoke tag-release clean-release

help:
	@printf '%s\n' \
	  'make release VERSION=0.0.1-pre.0  Validate, build, package, and smoke-test a local release' \
	  'make release-check VERSION=...    Validate versions and quality gates' \
	  'make tag-release VERSION=...      Run release and create an annotated local tag' \
	  'make clean-release                Remove local release artifacts'

release: release-check release-package release-smoke
	@printf 'Catify v%s release candidate is ready: %s\n' '$(VERSION)' '$(ARCHIVE)'

release-check:
	@test '$(RELEASE_TARGET)' != unsupported || { echo 'unsupported release host: $(UNAME_S)/$(UNAME_M)' >&2; exit 1; }
	@$(PYTHON) scripts/check-release-version.py --tag '$(TAG)'
	@cargo fmt --all -- --check
	@cargo clippy --workspace --all-targets --locked -- -D warnings
	@cargo test --workspace --locked -- --test-threads=1
	@$(PYTHON) -m unittest discover -s scripts/tests
	@npm test
	@$(PYTHON) scripts/generate-cli-matrix.py --check
	@./scripts/check-inventory.sh

release-build:
	@cargo build --release -p cfy-cli --bin cfy --bin catify --locked
	@target/release/cfy version
	@target/release/catify version

release-package: release-build
	@rm -rf '$(DIST_DIR)/staging'
	@$(PYTHON) scripts/package-release.py \
	  --binary target/release/cfy \
	  --version '$(VERSION)' \
	  --target '$(RELEASE_TARGET)' \
	  --output '$(DIST_DIR)'
	@test -f '$(ARCHIVE)'
	@$(PYTHON) scripts/generate-checksums.py '$(ARCHIVE)' --output '$(DIST_DIR)/SHA256SUMS'

release-smoke:
	@bash scripts/smoke-release-artifact.sh '$(ARCHIVE)'
	@bash scripts/test-installers.sh

tag-release: release
	@git diff --quiet && git diff --cached --quiet || { echo 'working tree must be clean before tagging' >&2; exit 1; }
	@! git rev-parse '$(TAG)' >/dev/null 2>&1 || { echo 'tag $(TAG) already exists' >&2; exit 1; }
	@git tag -a '$(TAG)' -m 'Catify $(TAG)'
	@printf 'Created local tag %s. Push it with: git push origin %s\n' '$(TAG)' '$(TAG)'

clean-release:
	@rm -rf '$(DIST_DIR)'
