RUNTIME_PATH ?= /usr/bin/runc
PROTO_PATH ?= conmon-rs/server/proto
BINARY := conmonrs
CONTAINER_RUNTIME ?= podman
BUILD_DIR ?= .build
GOTOOLS_GOPATH ?= $(BUILD_DIR)/gotools
GOTOOLS_BINDIR ?= $(GOTOOLS_GOPATH)/bin
# The integration suite is a module of its own in pkg/client/integration, so
# that its dependencies stay out of the go.mod of the client, which CRI-O
# requires and which follows CRI-O main. The modules only the suite needs
# follow the latest releases. Recursing into pkg/client runs the unit tests of
# the client as well. They do not use ginkgo, which warns about the version of
# it in the module graph of the client anyway. They form a suite of their own,
# and without --keep-going a failing one would skip the integration suite.
INTEGRATION_DIR := pkg/client/integration
GINKGO_FLAGS ?= -vv --trace --race --randomize-all --show-node-events --timeout 5m --keep-going -r pkg/client
# Extra ginkgo flags, CI passes --flake-attempts here so that a flake is
# visible locally.
TEST_FLAGS ?=
# The integration tests build and use the release binary if this is unset.
CONMON_BINARY ?=
# The resident set size every spec checks the server against, and how much it
# may grow over the execs of the memory leak specs. A dynamically linked
# release build stays well below 10 MiB, the static glibc builds CI tests get
# 5500 KiB (see .github/workflows/ci.yml).
MAX_RSS_KB ?= 10240
MAX_RSS_GROWTH_KB ?= 2000
# The generated Go code embeds a schema which differs between capnp versions,
# use the one pinned by flake.lock. Set CAPNP=capnp to use the local one.
CAPNP ?= nix shell --inputs-from . nixpkgs\#capnproto --command capnp
# Evaluated in the recipes, so that a failing cargo stops the build.
CRATE_VERSION := cargo metadata --no-deps --format-version 1 | jq -er '.packages[] | select(.name == "$(BINARY)") | .version'
SPEC_FILE := rpm/conmon-rs.spec
PREFIX ?= /usr
CI_TAG ?=
# The recipes read it from the environment, quoted.
export CI_TAG
GOLANGCI_LINT_VERSION := v2.15.0
# The images run pinned by the digest of their index, which a bump of the
# version updates as well, see dependencies.yaml and verify-digests.
PRETTIER_VERSION := 3.9.10
NODE_VERSION := 24.22.1
NODE_DIGEST := sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205
NODE_IMAGE := docker.io/library/node:$(NODE_VERSION)-slim@$(NODE_DIGEST)
PRETTIER = $(CONTAINER_RUNTIME) run --rm -v $(CURDIR):/w:z -w /w --entrypoint npx \
	$(NODE_IMAGE) --yes prettier@$(PRETTIER_VERSION)
ZEITGEIST_VERSION := v0.8.0
# The sha256 of the checksums.txt of the release, a bump of the version
# updates it as well, see .install.zeitgeist.
ZEITGEIST_CHECKSUMS_SHA256 := ec1a11bcdb987d7095ec8beb87c339b99671455be523af17e41b3402ae3407d4
ACTIONLINT_VERSION := v1.7.12
ZIZMOR_VERSION := 1.31.1
ZIZMOR_DIGEST := sha256:42d409dbb851d4c5d35a058cf042b828bea1e3f2884378c526eb0974780e18d9
ZIZMOR_IMAGE := ghcr.io/zizmorcore/zizmor:$(ZIZMOR_VERSION)@$(ZIZMOR_DIGEST)
# Extra zizmor flags, CI passes --format=github for annotations.
ZIZMOR_FLAGS ?=
GRYPE_VERSION := v0.121.0
GRYPE_DIGEST := sha256:621d56d1d68106b787022c81546f7e0a36202a25d1239f0c20195fab8c16f7ea
GRYPE_IMAGE := docker.io/anchore/grype:$(GRYPE_VERSION)@$(GRYPE_DIGEST)
# The SPDX SBOMs make scan checks, like the conmonrs.$ARCH.spdx.json files of
# the bucket.
SBOMS ?=

COLOR:=\\033[36m
NOCOLOR:=\\033[0m
WIDTH:=30

.PHONY: all
all: default

.PHONY: help
help:  ## Display this help.
	@awk \
		-v "col=${COLOR}" -v "nocol=${NOCOLOR}" \
		' \
			BEGIN { \
				FS = ":.*##" ; \
				printf "Usage:\n  make %s<target>%s\n", col, nocol \
			} \
			/^[./a-zA-Z_-]+:.*?##/ { \
				printf "  %s%-${WIDTH}s%s %s\n", col, $$1, nocol, $$2 \
			} \
			/^##@/ { \
				printf "\n%s\n", substr($$0, 5) \
			} \
		' $(MAKEFILE_LIST)

##@ Build targets:

.PHONY: default
default: ## Build in debug mode.
	cargo build --locked

.PHONY: release
release: ## Build in release mode.
	cargo build --locked --release

.PHONY: release-static
release-static: ## Build the static release binary for the local architecture using nix.
	nix build .#default

##@ Test targets:
# The features of the CI unit tests, which use cargo-nextest. The fuzzing
# feature only adds the entry points of the fuzz targets.
.PHONY: unit
unit: ## Run the unit tests with the default and with the tracing feature.
	cargo test --no-fail-fast
	cargo test --no-fail-fast --features tracing

.PHONY: unit-go
unit-go: ## Run the unit tests of the Go client, which need no server.
	go test -race ./pkg/client

.PHONY: integration
integration: .install.ginkgo $(if $(CONMON_BINARY),,release) ## Run the integration tests using $CONMON_BINARY or the release binary.
	export CONMON_BINARY="$(abspath $(or $(CONMON_BINARY),target/release/$(BINARY)))" && \
	export RUNTIME_BINARY="$(RUNTIME_PATH)" && \
	export MAX_RSS_KB="$(MAX_RSS_KB)" && \
	export MAX_RSS_GROWTH_KB="$(MAX_RSS_GROWTH_KB)" && \
	"$(GOTOOLS_BINDIR)/ginkgo" $(TEST_FLAGS) $(GINKGO_FLAGS)

##@ Verify targets:

.PHONY: lint
lint: lint-rust lint-go lint-shell ## Lint Rust, Go and shell sources.

# The checks of the lint-rust job of CI, which runs each target on its own.
# lint-msrv is left out, it needs the rust-version toolchain of rustup.
.PHONY: lint-rust
lint-rust: lint-rustfmt lint-clippy lint-rustdoc ## Lint the Rust sources.

.PHONY: lint-rustfmt
lint-rustfmt: ## Check the formatting of the Rust sources.
	cargo fmt && git diff --exit-code

# The default features are what gets shipped, all features cover the optional
# tracing code.
.PHONY: lint-clippy
lint-clippy: ## Lint the Rust sources and the fuzz targets with clippy.
	cargo clippy --locked --all-targets -- -D warnings
	cargo clippy --locked --all-targets --all-features -- -D warnings
	cargo clippy --locked -p conmonrs-fuzz --all-targets -- -D warnings

# Like the gh-pages workflow publishes it, with the private items, which
# document the server internals.
.PHONY: lint-rustdoc
lint-rustdoc: ## Check the documentation of the Rust sources.
	RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --features tracing --document-private-items

# The oldest toolchain the locked dependencies support, installed through
# rustup.
.PHONY: lint-msrv
lint-msrv: ## Check the Rust sources with the rust-version toolchain of Cargo.toml.
	MSRV=$$(sed -n 's/^rust-version = "\(.*\)"$$/\1/p' Cargo.toml) && \
	cargo "+$$MSRV" check --locked --all-targets --all-features

.PHONY: lint-go
lint-go: .install.golangci-lint ## Lint the Go sources.
	$(GOTOOLS_BINDIR)/golangci-lint version
	$(GOTOOLS_BINDIR)/golangci-lint run
	cd $(INTEGRATION_DIR) && $(abspath $(GOTOOLS_BINDIR))/golangci-lint run

.PHONY: lint-shell
lint-shell: ## Lint the shell scripts, the files whose first line is a sh or bash shebang.
	shellcheck $$(git ls-files -z | xargs -0 awk \
		'FNR == 1 && $$0 ~ "^#!(/usr/bin/env |/bin/)(ba)?sh$$" { print FILENAME } { nextfile }')

# The workflows in .github/workflows, zizmor runs in a container. zizmor only
# runs its online audits with GH_TOKEN set.
.PHONY: lint-workflows
lint-workflows: ## Lint the GitHub workflows with actionlint and zizmor.
	go run github.com/rhysd/actionlint/cmd/actionlint@$(ACTIONLINT_VERSION)
	$(CONTAINER_RUNTIME) run --rm -e GH_TOKEN -v $(CURDIR):/w:z,ro -w /w \
		$(ZIZMOR_IMAGE) $(ZIZMOR_FLAGS) .

.PHONY: lint-typos
lint-typos: ## Check the spelling, typos has to be installed.
	typos --config .github/typos.toml

.PHONY: verify
verify: verify-dependencies verify-spec verify-vex verify-proto verify-prettier verify-deny lint-typos lint-workflows ## Run the checks of CI besides make lint and the tests.

.PHONY: verify-dependencies
verify-dependencies: .install.zeitgeist ## Verify the local dependencies.
	$(GOTOOLS_BINDIR)/zeitgeist validate --local-only --base-path . --config dependencies.yaml

# Reads the GitHub API, set GITHUB_TOKEN for its rate limit.
.PHONY: verify-dependencies-upstream
verify-dependencies-upstream: .install.zeitgeist ## Check the dependencies for newer upstream versions.
	$(GOTOOLS_BINDIR)/zeitgeist validate --local-only=false --base-path . --config dependencies.yaml

# zeitgeist only checks the tags, which a pull ignores next to a digest. The
# image of the smoke test is the one of the build-static workflow. A tag which
# moved fails as well, so the advisories workflow runs it, not the pull
# requests.
.PHONY: verify-digests
verify-digests: ## Verify that the pinned images have the current digests of their tags.
	.github/verify-digests $(NODE_IMAGE) $(ZIZMOR_IMAGE) $(GRYPE_IMAGE) \
		"$$(sed -n 's/^ *IMAGE: //p' .github/workflows/build-static.yml)"

# Also catches new generated files. make removes the intermediate
# internal/proto/conmon.capnp only after this recipe.
.PHONY: verify-proto
verify-proto: update-proto ## Verify that the generated Go code matches the Cap'n Proto schema.
	@STATUS=$$(git status --porcelain -- internal/proto ':!internal/proto/conmon.capnp') && \
	if [ -n "$$STATUS" ]; then \
		echo "The generated code in internal/proto is not up to date:" ; \
		echo "$$STATUS" ; \
		git diff internal/proto ; \
		exit 1 ; \
	fi

.PHONY: verify-deny
verify-deny: ## Check the dependencies with cargo-deny, which has to be installed.
	cargo deny check

.PHONY: verify-spec
verify-spec: ## Verify that the RPM spec version matches the crate version.
	VERSION=$$($(CRATE_VERSION)) && \
	SPEC_VERSION=$$(sed -n 's/^Version: *//p' $(SPEC_FILE)) && \
	if [ "$$VERSION" != "$$SPEC_VERSION" ]; then \
		echo "$(SPEC_FILE) version $$SPEC_VERSION does not match crate version $$VERSION" ; \
		exit 1 ; \
	fi

.PHONY: verify-vex
verify-vex: ## Verify the OpenVEX document of the binaries.
	.github/verify-vex

# grype runs in a container, its vulnerability database stays in the build
# directory.
.PHONY: scan
scan: ## Scan $SBOMS for vulnerabilities with grype, applying the OpenVEX document.
	CONTAINER_RUNTIME="$(CONTAINER_RUNTIME)" \
	GRYPE_IMAGE=$(GRYPE_IMAGE) \
	GRYPE_DB_DIR="$(abspath $(BUILD_DIR))/grype-db" \
		.github/scan-sboms $(SBOMS)

.PHONY: verify-prettier
verify-prettier: ## Check the formatting of the supported files with prettier.
	$(PRETTIER) --check .

##@ Utility targets:

.PHONY: prettier
prettier: ## Prettify supported files.
	$(PRETTIER) -w .

# ginkgo publishes no binaries, an installed one of the version of the go.mod
# of the integration suite is kept, which CI caches.
.PHONY: .install.ginkgo
.install.ginkgo:
	VERSION=$$(go -C $(INTEGRATION_DIR) list -m -f {{.Version}} github.com/onsi/ginkgo/v2) && \
	if ! "$(GOTOOLS_BINDIR)/ginkgo" version 2>/dev/null | grep -qx "Ginkgo Version $${VERSION#v}"; then \
		GOBIN=$(abspath $(GOTOOLS_BINDIR)) go install "github.com/onsi/ginkgo/v2/ginkgo@$$VERSION"; \
	fi

# The release binaries of golangci-lint and zeitgeist, checked against the
# checksums of their release, see .github/install-binary. One of another
# version gets replaced.
TOOLS_OS = $(shell uname -s | tr '[:upper:]' '[:lower:]')
TOOLS_ARCH = $(shell uname -m | sed -e 's/^x86_64$$/amd64/' -e 's/^aarch64$$/arm64/')

# The releases of golangci-lint are immutable, the checksums file of one
# cannot change.
GOLANGCI_LINT_RELEASE = https://github.com/golangci/golangci-lint/releases/download/$(GOLANGCI_LINT_VERSION)
GOLANGCI_LINT_NAME = golangci-lint-$(GOLANGCI_LINT_VERSION:v%=%)-$(TOOLS_OS)-$(TOOLS_ARCH)

.PHONY: .install.golangci-lint
.install.golangci-lint:
	if [ "$$("$(GOTOOLS_BINDIR)/golangci-lint" version --short 2>/dev/null)" != "$(GOLANGCI_LINT_VERSION:v%=%)" ]; then \
		.github/install-binary -m "$(GOLANGCI_LINT_NAME)/golangci-lint" \
			"$(GOLANGCI_LINT_RELEASE)/$(GOLANGCI_LINT_NAME).tar.gz" \
			"$(GOLANGCI_LINT_RELEASE)/golangci-lint-$(GOLANGCI_LINT_VERSION:v%=%)-checksums.txt" \
			"$(GOTOOLS_BINDIR)/golangci-lint"; \
	fi

# The remote variant of zeitgeist, which also checks the upstream versions.
# Its releases are not immutable, so the checksums file of the release is
# pinned by ZEITGEIST_CHECKSUMS_SHA256, which a bump of ZEITGEIST_VERSION
# updates as well, the install fails otherwise.
ZEITGEIST_RELEASE = https://github.com/kubernetes-sigs/zeitgeist/releases/download/$(ZEITGEIST_VERSION)

.PHONY: .install.zeitgeist
.install.zeitgeist:
	if [ "$$("$(GOTOOLS_BINDIR)/zeitgeist" version 2>/dev/null | awk '$$1 == "GitVersion:" { print $$2 }')" != "$(ZEITGEIST_VERSION)" ]; then \
		.github/install-binary -s "$(ZEITGEIST_CHECKSUMS_SHA256)" \
			"$(ZEITGEIST_RELEASE)/zeitgeist-remote-$(TOOLS_ARCH)-$(TOOLS_OS)" \
			"$(ZEITGEIST_RELEASE)/checksums.txt" "$(GOTOOLS_BINDIR)/zeitgeist"; \
	fi

.PHONY: clean
clean: ## Cleanup the project files.
	rm -rf target/ result

.INTERMEDIATE: internal/proto/conmon.capnp
internal/proto/conmon.capnp: $(PROTO_PATH)/conmon.capnp $(PROTO_PATH)/go-patch
	cat $^ > $@

.PHONY: update-proto
update-proto: internal/proto/conmon.capnp ## Update the Cap'n Proto protocol.
	$(eval GO_CAPNP_VERSION ?= $(shell grep '^\s*capnproto.org/go/capnp/v3 v3\.' go.mod | grep -o 'v3\..*'))
	GOBIN=$(abspath $(GOTOOLS_BINDIR)) go install capnproto.org/go/capnp/v3/capnpc-go@$(GO_CAPNP_VERSION)
	$(CAPNP) compile \
		-I$(shell go env GOMODCACHE)/capnproto.org/go/capnp/v3@$(GO_CAPNP_VERSION)/std \
		-o$(abspath $(GOTOOLS_BINDIR))/capnpc-go internal/proto/conmon.capnp

.PHONY: create-release-packages
create-release-packages: verify-spec ## Create the release tarballs.
	VERSION=$$($(CRATE_VERSION)) && \
	if [ "v$$VERSION" != "$$CI_TAG" ]; then \
		echo "crate version v$$VERSION and tag $$CI_TAG mismatch" ; \
		exit 1 ; \
	fi && \
	git archive --format tar.gz --prefix="$(BINARY)-$$CI_TAG/" -o "$(BINARY)-$$CI_TAG.tar.gz" "$$CI_TAG"

.PHONY: install
install: ## Install the binary.
	mkdir -p "${DESTDIR}$(PREFIX)/bin"
	install -D -t "${DESTDIR}$(PREFIX)/bin" target/release/conmonrs

# Only meant to build the latest HEAD commit, not a replacement for the distro
# package. The spec passes the commit on to --version.
.PHONY: rpm
rpm: ## Build the RPM locally using packit.
	BUILD_COMMIT=$$(git rev-parse HEAD) packit build locally

.PHONY: nixpkgs
nixpkgs: ## Update the NIX package dependencies.
	@nix flake update
