# limen — task runner. `make check` is what CI runs; green means correct.
# Targets only call scripts under tools/. Never launch the build by hand.

SHELL := /bin/bash
.DEFAULT_GOAL := help
# What `cli` and `docker` compile. `debug` links in seconds; a published binary is `release`.
VARIANT ?= debug
# The shared head of AGENTS.md, everything above its `<!-- xoadev:end -->` line, lives here.
AGENTS_URL := https://xoa.dev/_dev/OSS_AGENTS.md

.PHONY: check lint agents-sync agents-check build test cli local-install docker e2e pack pack-drafts pack-release release hooks clean help

check: lint build test ## Everything CI runs (`make -k check` to see every failure at once)
	@echo "check: OK"

lint: ## rustfmt, clippy, shellcheck, actionlint and agents-check. `make lint FIX=1` formats instead of checking
	@FIX=$(FIX) AGENTS_URL='$(AGENTS_URL)' tools/lint.sh

agents-sync: ## Rewrite the shared head of AGENTS.md from AGENTS_URL
	@AGENTS_URL='$(AGENTS_URL)' tools/agents.sh sync

agents-check: ## Fail if the shared head of AGENTS.md differs from AGENTS_URL (warns when offline)
	@AGENTS_URL='$(AGENTS_URL)' tools/agents.sh check

build: ## Compile every crate, tests included
	@tools/cargo.sh build

test: ## Tests of every crate
	@tools/cargo.sh test

cli: ## Only the static `limen` binary (VARIANT=release for the published one; ARCH="x86_64 aarch64" for both)
	@VARIANT=$(VARIANT) ARCH="$(ARCH)" tools/cargo.sh cli

local-install: cli ## Install the binary in ~/.local/bin (or PREFIX)
	@VARIANT=$(VARIANT) tools/local-install.sh

docker: cli ## Build the hub image (limen:local, or IMAGE=…) from the binary of `make cli`
	@VARIANT=$(VARIANT) tools/docker.sh

e2e: cli ## End to end in containers, Debian and OpenWrt (SUITE=debian|openwrt|join for one). Not part of `make check`
	@VARIANT=$(VARIANT) tools/e2e.sh

pack: ## One pack as it is released, in dist/: PACK=<pack> (VERSION=X.Y.Z, else dev)
	@PACK=$(PACK) VERSION=$(VERSION) tools/pack.sh

pack-drafts: ## Run packs-draft.yml on main to rewrite every pack's draft release. Needs gh
	@tools/workflow.sh drafts

pack-release: ## Run packs.yml on main for PACK=<pack>: its draft built, published and attested only with PUBLISH=1. Needs gh
	@tools/workflow.sh pack "$(PACK)" "$(PUBLISH)"

release: ## Run release.yml on main: limen built, tested and started, published only with PUBLISH=1. Needs gh
	@tools/workflow.sh release "$(PUBLISH)"

hooks: ## Install the git hooks of tools/ (pre-push: nothing is pushed to main)
	@tools/hooks-install.sh

clean: ## Remove build output
	@tools/cargo.sh clean

help: ## List the targets
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  %-14s %s\n",$$1,$$2}'
