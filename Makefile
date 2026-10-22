RUNTIME_PATH ?= /usr/bin/runc
PROTO_PATH ?= "conmon-rs/server/proto"
BINARY := conmonrs
CONTAINER_RUNTIME ?= podman
BUILD_DIR ?= .build
GOTOOLS_GOPATH ?= $(BUILD_DIR)/gotools
GOTOOLS_BINDIR ?= $(GOTOOLS_GOPATH)/bin
GINKGO_FLAGS ?= -vv --trace --race --randomize-all --show-node-events --timeout 5m -r pkg/client
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
PRETTIER_VERSION := 3.9.10
NODE_VERSION := 24.22.1
PRETTIER = $(CONTAINER_RUNTIME) run --rm -v ${PWD}:/w:z -w /w --entrypoint npx docker.io/library/node:$(NODE_VERSION)-slim \
	--yes prettier@$(PRETTIER_VERSION)
ZEITGEIST_VERSION := v0.8.0
ACTIONLINT_VERSION := v1.7.12
ZIZMOR_VERSION := 1.31.1
# Extra zizmor flags, CI passes --format=github for annotations.
ZIZMOR_FLAGS ?=

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

# TestConmonClient is the integration suite, see the integration target.
.PHONY: unit-go
unit-go: ## Run the unit tests of the Go client, which need no server.
	go test -race -skip '^TestConmonClient$$' ./pkg/client

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

.PHONY: lint-rust
lint-rust: ## Lint the Rust sources.
	cargo fmt && git diff --exit-code
	cargo clippy --all-targets -- -D warnings
	cargo clippy --all-targets --all-features -- -D warnings

.PHONY: lint-go
lint-go: .install.golangci-lint ## Lint the Go sources.
	$(GOTOOLS_BINDIR)/golangci-lint version
	$(GOTOOLS_BINDIR)/golangci-lint linters
	GL_DEBUG=gocritic $(GOTOOLS_BINDIR)/golangci-lint run

.PHONY: lint-shell
lint-shell: ## Lint the shell scripts, the files whose first line is a sh or bash shebang.
	shellcheck $$(git ls-files -z | xargs -0 awk \
		'FNR == 1 && $$0 ~ "^#!(/usr/bin/env |/bin/)(ba)?sh$$" { print FILENAME } { nextfile }')

# The workflows in .github/workflows, zizmor runs in a container. zizmor only
# runs its online audits with GH_TOKEN set.
.PHONY: lint-workflows
lint-workflows: ## Lint the GitHub workflows with actionlint and zizmor.
	go run github.com/rhysd/actionlint/cmd/actionlint@$(ACTIONLINT_VERSION)
	$(CONTAINER_RUNTIME) run --rm -e GH_TOKEN -v ${PWD}:/w:z,ro -w /w ghcr.io/zizmorcore/zizmor:$(ZIZMOR_VERSION) \
		$(ZIZMOR_FLAGS) .

.PHONY: lint-typos
lint-typos: ## Check the spelling, typos has to be installed.
	typos --config .github/typos.toml

.PHONY: verify
verify: verify-dependencies verify-spec verify-proto verify-prettier verify-deny lint-typos lint-workflows ## Run the checks of CI besides make lint and the tests.

.PHONY: verify-dependencies
verify-dependencies: $(GOTOOLS_BINDIR)/zeitgeist ## Verify the local dependencies.
	$(GOTOOLS_BINDIR)/zeitgeist validate --local-only --base-path . --config dependencies.yaml

# Reads the GitHub API, set GITHUB_TOKEN for its rate limit.
.PHONY: verify-dependencies-upstream
verify-dependencies-upstream: $(GOTOOLS_BINDIR)/zeitgeist ## Check the dependencies for newer upstream versions.
	$(GOTOOLS_BINDIR)/zeitgeist validate --local-only=false --base-path . --config dependencies.yaml

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

.PHONY: verify-prettier
verify-prettier: ## Check the formatting of the supported files with prettier.
	$(PRETTIER) --check .

##@ Utility targets:

.PHONY: prettier
prettier: ## Prettify supported files.
	$(PRETTIER) -w .

# ginkgo publishes no binaries, an installed one of the version of go.mod is
# kept, which CI caches.
.PHONY: .install.ginkgo
.install.ginkgo:
	VERSION=$$(go list -m -f {{.Version}} github.com/onsi/ginkgo/v2) && \
	if ! "$(GOTOOLS_BINDIR)/ginkgo" version 2>/dev/null | grep -qx "Ginkgo Version $${VERSION#v}"; then \
		GOBIN=$(abspath $(GOTOOLS_BINDIR)) go install "github.com/onsi/ginkgo/v2/ginkgo@$$VERSION"; \
	fi

.PHONY: .install.golangci-lint
.install.golangci-lint:
	GOBIN=$(abspath $(GOTOOLS_BINDIR)) \
		go install github.com/golangci/golangci-lint/v2/cmd/golangci-lint@$(GOLANGCI_LINT_VERSION)

# Built from source for any architecture, the Go checksum database verifies
# the module. The remote variant also checks the upstream versions.
$(GOTOOLS_BINDIR)/zeitgeist:
	GOBIN=$(abspath $(GOTOOLS_BINDIR)) \
		go install sigs.k8s.io/zeitgeist/remote/zeitgeist@$(ZEITGEIST_VERSION)

.PHONY: clean
clean: ## Cleanup the project files.
	rm -rf target/ result

.INTERMEDIATE: internal/proto/conmon.capnp
internal/proto/conmon.capnp:
	cat $(PROTO_PATH)/conmon.capnp $(PROTO_PATH)/go-patch > $@


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
	git archive --format tar --prefix="$(BINARY)-$$CI_TAG/" "$$CI_TAG" | gzip >"$(BINARY)-$$CI_TAG.tar.gz"

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
