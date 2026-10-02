# limen — task runner. `make check` is what CI runs; green means correct.
# Targets only call scripts under tools/. Never launch the build by hand.

SHELL := /bin/bash
.DEFAULT_GOAL := help
# What `cli` and `docker` compile. `debug` links in seconds; a published binary is `release`.
VARIANT ?= debug

.PHONY: check lint build test cli local-install docker e2e pack hooks clean help

check: lint build test ## Everything CI runs (`make -k check` to see every failure at once)
	@echo "check: OK"

lint: ## rustfmt, clippy, shellcheck and actionlint. `make lint FIX=1` formats instead of checking
	@FIX=$(FIX) tools/lint.sh

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

hooks: ## Install the git hooks of tools/ (pre-push: nothing is pushed to main)
	@tools/hooks-install.sh

clean: ## Remove build output
	@tools/cargo.sh clean

help: ## List the targets
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  %-14s %s\n",$$1,$$2}'
