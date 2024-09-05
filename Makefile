MAKEFILE_PATH := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
RUNTIME_PATH ?= "/usr/bin/runc"
PROTO_PATH ?= "conmon-rs/common/proto"
BINARY := conmonrs
CONTAINER_RUNTIME ?= podman
BUILD_DIR ?= .build
GOTOOLS_GOPATH ?= $(BUILD_DIR)/gotools
GOTOOLS_BINDIR ?= $(GOTOOLS_GOPATH)/bin
GINKGO_FLAGS ?= -vv --trace --race --randomize-all --flake-attempts 3 --show-node-events --timeout 5m -r pkg/client
TEST_FLAGS ?=
PACKAGE_NAME ?= $(shell cargo metadata --no-deps --format-version 1 | jq -r '.packages[2] | [ .name, .version ] | join("-v")')
PREFIX ?= /usr
CI_TAG ?=
GOLANGCI_LINT_VERSION := v1.60.3
ZEITGEIST_VERSION := v0.4.4

COLOR:=\\033[36m
NOCOLOR:=\\033[0m
WIDTH:=25

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
release-static: ## Build the static release binary.
	RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-unknown-linux-gnu
	strip -s target/x86_64-unknown-linux-gnu/release/conmonrs
	ldd target/x86_64-unknown-linux-gnu/release/conmonrs 2>&1 | grep -qE '(statically linked)|(not a dynamic executable)'

##@ Test targets:
.PHONY: unit
unit: ## Run the unit tests.
	cargo test --no-fail-fast

.PHONY: integration
integration: .install.ginkgo release ## Run the integration tests using the release binary.
	export CONMON_BINARY="$(MAKEFILE_PATH)target/release/$(BINARY)" && \
	export RUNTIME_BINARY="$(RUNTIME_PATH)" && \
	export MAX_RSS_KB=10240 && \
	"$(GOTOOLS_BINDIR)/ginkgo" $(TEST_FLAGS) $(GINKGO_FLAGS)

.PHONY: integration-static
integration-static: .install.ginkgo ## Run the integration tests using the static release binary.
	export CONMON_BINARY="$(MAKEFILE_PATH)target/x86_64-unknown-linux-gnu/release/$(BINARY)" && \
	if [ ! -f "$$CONMON_BINARY" ]; then \
		$(MAKE) release-static; \
	fi && \
	export RUNTIME_BINARY="$(RUNTIME_PATH)" && \
	export MAX_RSS_KB=9500 && \
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

.PHONY: verify-prettier
verify-prettier: prettier ## Run prettier on the project.
	./hack/tree_status.sh

##@ Utility targets:

.PHONY: prettier
prettier: ## Prettify supported files.
	$(CONTAINER_RUNTIME) run -it --privileged -v ${PWD}:/w -w /w --entrypoint bash node:latest -c \
		'npm install -g prettier && prettier -w .'

.PHONY: .install.ginkgo
.install.ginkgo:
	GOBIN=$(abspath $(GOTOOLS_BINDIR)) \
		go install "github.com/onsi/ginkgo/v2/ginkgo@$$(go list -m -f {{.Version}} github.com/onsi/ginkgo/v2)"

.PHONY: .install.golangci-lint
.install.golangci-lint:
	curl -sSfL https://raw.githubusercontent.com/golangci/golangci-lint/master/install.sh | \
		BINDIR=$(abspath $(GOTOOLS_BINDIR)) sh -s $(GOLANGCI_LINT_VERSION)

$(GOTOOLS_BINDIR)/zeitgeist:
	mkdir -p $(GOTOOLS_BINDIR)
	curl -sSfL -o $(GOTOOLS_BINDIR)/zeitgeist \
		https://storage.googleapis.com/k8s-artifacts-sig-release/kubernetes-sigs/zeitgeist/$(ZEITGEIST_VERSION)/zeitgeist-amd64-linux
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
	capnp compile \
		-I$(shell go env GOMODCACHE)/capnproto.org/go/capnp/v3@$(GO_CAPNP_VERSION)/std \
		-ogo internal/proto/conmon.capnp

.PHONY: create-release-packages
create-release-packages: release ## Create the release tarballs.
	if [ "$(PACKAGE_NAME)" != "conmonrs-$(CI_TAG)" ]; then \
		echo "crate version and tag mismatch" ; \
		exit 1 ; \
	fi
	git archive --format tar --prefix=conmonrs-$(CI_TAG)/ $(CI_TAG) | gzip >$(PACKAGE_NAME).tar.gz

.PHONY: install
install: ## Install the binary.
	mkdir -p "${DESTDIR}$(PREFIX)/bin"
	install -D -t "${DESTDIR}$(PREFIX)/bin" target/release/conmonrs

# Only meant to build the latest HEAD commit + any uncommitted changes
# Not a replacement for the distro package
.PHONY: rpm
rpm: # Build the RPM locally
	rpkg local

.PHONY: nixpkgs
nixpkgs: ## Update the NIX package dependencies.
	@nix run -f channel:nixpkgs-unstable nix-prefetch-git -- \
		--no-deepClone https://github.com/nixos/nixpkgs > nix/nixpkgs.json
