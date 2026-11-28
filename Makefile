# limen — task runner. `make check` is what CI runs; green means correct.
# Its own targets only call scripts under tools/; the linters are xoa-base's. Never launch the build by hand.

# What `cli` and `docker` compile. `debug` links in seconds; a published binary is `release`.
VARIANT ?= debug

# xoadev/xoa-base at the commit the workflows call, which Dependabot moves: make reads it from them,
# fetches it into .xoa-base/ when it is missing or at another commit, and git checks it.
XOA_BASE := $(sort $(shell sed -n 's|.*xoadev/xoa-base/\.github/[^@]*@\([0-9a-f]\{40\}\).*|\1|p' .github/workflows/*.y*ml))
$(if $(XOA_BASE),,$(error no workflow calls xoa-base yet: add the one its README shows first))
$(if $(filter 1,$(words $(XOA_BASE))),,$(error the workflows call xoa-base at $(XOA_BASE): one commit, in every uses: line))
# install.sh and the packs' scripts are POSIX sh (OpenWrt has no bash), which their shebang tells shellcheck and
# shfmt; a pack's scripts have no extension: they are its executable files.
SHELL_FILES := $(wildcard tools/*.sh) install.sh $(shell find packs -type f -perm -u+x | sort)
# Neither starts a document: CLAUDE.md is one import line, the template fills a pull request's body.
MD_OPTIONS := --config 'per-file-ignores = { "CLAUDE.md" = ["MD041"], ".github/pull_request_template.md" = ["MD041"] }'
include .xoa-base/$(XOA_BASE)/base.mk
include $(foreach linter,markdown yaml shell actions typos editorconfig rust,.xoa-base/$(XOA_BASE)/lint/$(linter).mk)
.xoa-base/$(XOA_BASE)/%: .xoa-base/$(XOA_BASE)/.fetched ;
.xoa-base/$(XOA_BASE)/.fetched:
	@rm -rf .xoa-base && git init -q $(@D)
	@git -C $(@D) fetch -q --depth 1 https://github.com/xoadev/xoa-base $(XOA_BASE)
	@git -C $(@D) checkout -q FETCH_HEAD && touch $@

.PHONY: check lint format build test release-files cli local-install docker e2e pack pack-drafts pack-release release hooks clean

check: lint build test ## Everything CI runs (`make -k check` to see every failure at once)
	@echo "check: OK"

lint: md-lint yaml-lint shell-lint actions-lint typos-lint editorconfig-lint rust-lint ## xoa-base's linters, Rust's included

format: md-format yaml-format shell-format rust-format ## Format the Markdown, the YAML, the scripts and the Rust

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

e2e: cli ## End to end in containers, Debian and OpenWrt (SUITE=debian|openwrt|join|mqtt for one). Not part of `make check`
	@VARIANT=$(VARIANT) tools/e2e.sh

release-files: ## What a release publishes, into release/: both release binaries, the scenes run with them, the hub image (VERSION=X.Y.Z, else dev). Needs Docker
	@VERSION=$(VERSION) BUILD_NUMBER=$(BUILD_NUMBER) RELEASE_IMAGE=$(RELEASE_IMAGE) tools/release.sh

pack: ## One pack as it is released, in release/: PACK=<pack> (VERSION=X.Y.Z, else dev)
	@PACK=$(PACK) VERSION=$(VERSION) BUILD_NUMBER=$(BUILD_NUMBER) tools/pack.sh

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
