# limen — task runner. `make check` is what CI runs; green means correct.
# Targets only call scripts under tools/. Never launch the build by hand.

SHELL := /bin/bash
.DEFAULT_GOAL := help
# What `cli` and `docker` compile. `debug` links in seconds; a published binary is `release`.
VARIANT ?= debug

.PHONY: check lint build stamp test cli local-install docker e2e hooks clean help

check: lint build test ## Everything CI runs (`make -k check` to see every failure at once)
	@echo "check: OK"

lint: ## ktlint, shellcheck and actionlint. `make lint FIX=1` fixes what can be fixed automatically
	@tools/lint.sh $(if $(FIX),--fix,)

build: stamp ## Compile every module
	@tools/kt build

stamp: ## Write the build stamp (LIMEN_VERSION, `dev` unless CI sets it); not committed
	@tools/stamp.sh

test: stamp ## Tests of every module
	@tools/kt test

cli: stamp ## Compile only the `limen` binary (VARIANT=release for the optimised one; ARCH="x86_64 aarch64" for both)
	@VARIANT=$(VARIANT) ARCH="$(ARCH)" tools/kt cli

local-install: cli ## Install the binary in ~/.local/bin (or PREFIX)
	@VARIANT=$(VARIANT) tools/local-install.sh

docker: cli ## Build the hub image (limen:local, or IMAGE=…) from the binary of `make cli`
	@VARIANT=$(VARIANT) tools/docker.sh

e2e: cli ## End to end: a Debian container with sshd, `limen install`, and the hub against it. Not part of `make check`
	@VARIANT=$(VARIANT) tools/e2e.sh

hooks: ## Install the git hooks of tools/ (pre-push: nothing is pushed to main)
	@tools/hooks-install.sh

clean: ## Remove build output
	@tools/kt clean || true

help: ## List the targets
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  %-14s %s\n",$$1,$$2}'
