RUNTIME_PATH ?= /usr/bin/runc
PROTO_PATH ?= "conmon-rs/server/proto"
BINARY := conmonrs
CONTAINER_RUNTIME ?= podman
BUILD_DIR ?= .build
GOTOOLS_GOPATH ?= $(BUILD_DIR)/gotools
GOTOOLS_BINDIR ?= $(GOTOOLS_GOPATH)/bin
GINKGO_FLAGS ?= -vv --trace --race --randomize-all --flake-attempts 3 --show-node-events --timeout 5m -r pkg/client
TEST_FLAGS ?=
# The integration tests build and use the release binary if this is unset.
CONMON_BINARY ?=
MAX_RSS_KB ?= 10240
# The generated Go code embeds a schema which differs between capnp versions,
# use the one pinned by flake.lock. Set CAPNP=capnp to use the local one.
CAPNP ?= nix shell --inputs-from . nixpkgs\#capnproto --command capnp
# Evaluated in the recipes, so that a failing cargo stops the build.
CRATE_VERSION := cargo metadata --no-deps --format-version 1 | jq -er '.packages[] | select(.name == "$(BINARY)") | .version'
SPEC_FILE := rpm/conmon-rs.spec
PREFIX ?= /usr
CI_TAG ?=
GOLANGCI_LINT_VERSION := v2.15.0
ZEITGEIST_VERSION := v0.8.0

COLOR:=\\033[36m
NOCOLOR:=\\033[0m
WIDTH:=25

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
	cargo build

.PHONY: release
release: ## Build in release mode.
	cargo build --release

.PHONY: release-static
release-static: ## Build the static release binary for the local architecture using nix.
	nix build .#default

##@ Test targets:
.PHONY: unit
unit: ## Run the unit tests.
	cargo test --no-fail-fast

.PHONY: integration
integration: .install.ginkgo $(if $(CONMON_BINARY),,release) ## Run the integration tests using $CONMON_BINARY or the release binary.
	export CONMON_BINARY="$(abspath $(or $(CONMON_BINARY),target/release/$(BINARY)))" && \
	export RUNTIME_BINARY="$(RUNTIME_PATH)" && \
	export MAX_RSS_KB="$(MAX_RSS_KB)" && \
	"$(GOTOOLS_BINDIR)/ginkgo" $(TEST_FLAGS) $(GINKGO_FLAGS)

##@ Verify targets:

.PHONY: lint
lint: lint-rust lint-go ## Lint Rust and Go sources.

.PHONY: lint-rust
lint-rust: ## Lint the Rust sources.
	cargo fmt && git diff --exit-code
	cargo clippy --all-targets --all-features -- -D warnings

.PHONY: lint-go
lint-go: .install.golangci-lint ## Lint the Go sources.
	$(GOTOOLS_BINDIR)/golangci-lint version
	$(GOTOOLS_BINDIR)/golangci-lint linters
	GL_DEBUG=gocritic $(GOTOOLS_BINDIR)/golangci-lint run

.PHONY: verify-dependencies
verify-dependencies: $(GOTOOLS_BINDIR)/zeitgeist ## Verify the local dependencies.
	$(GOTOOLS_BINDIR)/zeitgeist validate --local-only --base-path . --config dependencies.yaml

.PHONY: verify-spec
verify-spec: ## Verify that the RPM spec version matches the crate version.
	VERSION=$$($(CRATE_VERSION)) && \
	SPEC_VERSION=$$(sed -n 's/^Version: *//p' $(SPEC_FILE)) && \
	if [ "$$VERSION" != "$$SPEC_VERSION" ]; then \
		echo "$(SPEC_FILE) version $$SPEC_VERSION does not match crate version $$VERSION" ; \
		exit 1 ; \
	fi

.PHONY: verify-prettier
verify-prettier: prettier ## Run prettier on the project and fail on changes.
	git diff --exit-code

##@ Utility targets:

.PHONY: prettier
prettier: ## Prettify supported files.
	$(CONTAINER_RUNTIME) run --rm -v ${PWD}:/w:z -w /w --entrypoint bash node:latest -c \
		'npm install -g prettier && prettier -w .'

.PHONY: .install.ginkgo
.install.ginkgo:
	GOBIN=$(abspath $(GOTOOLS_BINDIR)) \
		go install "github.com/onsi/ginkgo/v2/ginkgo@$$(go list -m -f {{.Version}} github.com/onsi/ginkgo/v2)"

.PHONY: .install.golangci-lint
.install.golangci-lint:
	GOBIN=$(abspath $(GOTOOLS_BINDIR)) \
		go install github.com/golangci/golangci-lint/v2/cmd/golangci-lint@$(GOLANGCI_LINT_VERSION)

$(GOTOOLS_BINDIR)/zeitgeist:
	mkdir -p $(GOTOOLS_BINDIR)
	curl -sSfL -o $(GOTOOLS_BINDIR)/zeitgeist \
		https://github.com/kubernetes-sigs/zeitgeist/releases/download/$(ZEITGEIST_VERSION)/zeitgeist-amd64-linux
	chmod +x $(GOTOOLS_BINDIR)/zeitgeist

.PHONY: clean
clean: ## Cleanup the project files.
	rm -rf target/

.INTERMEDIATE: internal/proto/conmon.capnp
internal/proto/conmon.capnp:
	cat $(PROTO_PATH)/conmon.capnp $(PROTO_PATH)/go-patch > $@


.PHONY: update-proto
update-proto: internal/proto/conmon.capnp ## Update the Cap'n Proto protocol.
	$(eval GO_CAPNP_VERSION ?= $(shell grep '^\s*capnproto.org/go/capnp/v3 v3\.' go.mod | grep -o 'v3\..*'))
	go install capnproto.org/go/capnp/v3/capnpc-go@$(GO_CAPNP_VERSION)
	$(CAPNP) compile \
		-I$(shell go env GOMODCACHE)/capnproto.org/go/capnp/v3@$(GO_CAPNP_VERSION)/std \
		-ogo internal/proto/conmon.capnp

.PHONY: create-release-packages
create-release-packages: verify-spec ## Create the release tarballs.
	VERSION=$$($(CRATE_VERSION)) && \
	if [ "v$$VERSION" != "$(CI_TAG)" ]; then \
		echo "crate version v$$VERSION and tag $(CI_TAG) mismatch" ; \
		exit 1 ; \
	fi
	git archive --format tar --prefix=$(BINARY)-$(CI_TAG)/ $(CI_TAG) | gzip >$(BINARY)-$(CI_TAG).tar.gz

.PHONY: install
install: ## Install the binary.
	mkdir -p "${DESTDIR}$(PREFIX)/bin"
	install -D -t "${DESTDIR}$(PREFIX)/bin" target/release/conmonrs

# Only meant to build the latest HEAD commit, not a replacement for the distro
# package.
.PHONY: rpm
rpm: ## Build the RPM locally using packit.
	packit build locally

.PHONY: nixpkgs
nixpkgs: ## Update the NIX package dependencies.
	@nix flake update
