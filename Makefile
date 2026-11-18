.DEFAULT_GOAL := help

.PHONY: help
help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F ':.*## ' '{printf "  %-17s %s\n", $$1, $$2}'

.PHONY: check
check: fmt-check lint test deny ## Everything a change must pass

.PHONY: fmt
fmt: ## Format every crate
	cargo fmt --all

.PHONY: fmt-check
fmt-check: ## Fail if anything is not formatted
	cargo fmt --all --check

.PHONY: lint
lint: ## Clippy on every target and feature, and the docs, warnings refused
	cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked --quiet

.PHONY: test
test: ## Every test, the end-to-end ones included
	cargo test --workspace --locked

.PHONY: deny
deny: ## Advisories, licences, duplicates and sources of every dependency (needs cargo-deny)
	cargo deny --locked check

.PHONY: tools
tools: ## Install the pinned cargo-deny and worker-build
	cargo install --locked cargo-deny@0.20.2 worker-build@0.9.0

.PHONY: build
build: ## The release binaries, at target/release: mensor, mensor-mind and mensor-hand
	cargo build --release --locked -p mensor

IMAGE ?= mensor

.PHONY: image
image: ## The image, with Postgres, built from this repo for this machine (needs Docker)
	docker build -t $(IMAGE) .

.PHONY: test-postgres
test-postgres: ## The Postgres store, and the end-to-end tests over it, against Postgres in Docker (needs Docker)
	crates/store-postgres/tests/postgres.sh

.PHONY: build-cloudflare
build-cloudflare: ## The Cloudflare worker, at crates/cloudflare/build (needs worker-build)
	cd crates/cloudflare && worker-build --release

.PHONY: test-cloudflare
test-cloudflare: build-cloudflare ## The worker on workerd, run by wrangler (needs Node)
	cargo test --locked -p mensor-cloudflare --test workerd -- --ignored
