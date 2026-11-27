# apitool — a Postman-like API client in Rust and egui.
#
#   make run      build and launch
#   make test     run the tests
#   make check    everything CI gates on; run before push or tag
#
# Target names follow ~/git/dev_docs/project-conventions/references/makefile.md.
# The scripts live in docs/scripts (a symlink into ~/git/dev_docs/testing).

SHELL   := /bin/bash
BASH5   := /opt/homebrew/bin/bash
OUT     := build
VERSION ?=

# Local builds skip the shipping LTO; the release workflow turns it back on.
export CARGO_PROFILE_RELEASE_LTO           ?= false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS ?= 16

# The one pin for rustfmt and clippy, here and in CI (release.yml reads this line for its
# clippy, which lints the Windows and Linux cfg code this Mac can't; verify.sh too). Bump
# it on purpose, with the fixes.
LINT_RUST := 1.97

.DEFAULT_GOAL := help
.PHONY: help build run test lint fmt check shots clean release

help: ## Show this help
	@grep -E '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Dev build
	cargo build

run: ## Build and launch
	cargo run

# The JetStream and key-value tests need nats-server on PATH or a running docker
# (src/nats.rs, `real`); without either they fail rather than skip.
test: ## Unit and UI tests
	cargo test --no-fail-fast

lint: ## Format and lint checks
	cargo +$(LINT_RUST) fmt --check
	cargo +$(LINT_RUST) clippy --all-targets -- -D warnings

fmt: ## Format in place, with the rustfmt lint checks against
	cargo +$(LINT_RUST) fmt

# verify.sh adds what `lint test` can't: clippy for Windows through cargo-xwin, a log per
# step and a summary that fails on a skipped step.
check: ## Everything CI gates on: fmt, clippy, tests, Windows clippy
	$(BASH5) docs/scripts/verify.sh -o $(OUT)/verify </dev/null

shots: ## Screenshot tour of every screen into /tmp/apitool-shots
	APITOOL_SHOTS=1 cargo test --lib ux_tour -- --ignored

clean: ## Remove build output
	rm -rf $(OUT) target

release: check ## Bump, push and tag; needs VERSION=x.y.z
	@test -n "$(VERSION)" || { echo "VERSION is required: make release VERSION=x.y.z" >&2; exit 2; }
	$(BASH5) docs/scripts/release.sh $(VERSION) </dev/null
