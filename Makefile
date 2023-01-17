MAKEFILE_PATH := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
RUNTIME_PATH ?= "/usr/bin/runc"
PROTO_PATH ?= "conmon-rs/common/proto"
BINARY := conmonrs
BUILD_DIR ?= .build
GOTOOLS_GOPATH ?= $(BUILD_DIR)/gotools
GOTOOLS_BINDIR ?= $(GOTOOLS_GOPATH)/bin
GINKGO_FLAGS ?= -vv --trace --race --randomize-all --flake-attempts 3 --show-node-events --timeout 5m -r pkg/client
TEST_FLAGS ?=
PACKAGE_NAME ?= $(shell cargo metadata --no-deps --format-version 1 | jq -r '.packages[2] | [ .name, .version ] | join("-v")')
PREFIX ?= /usr
CI_TAG ?=

default:
	cargo build

release:
	cargo build --release

.PHONY: release-static
release-static:
	RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-unknown-linux-gnu
	strip -s target/x86_64-unknown-linux-gnu/release/conmonrs
	ldd target/x86_64-unknown-linux-gnu/release/conmonrs 2>&1 | grep -qE '(statically linked)|(not a dynamic executable)'

lint: lint-rust lint-go

lint-rust:
	cargo fmt && git diff --exit-code
	cargo clippy --all-targets --all-features -- -D warnings

lint-go: .install.golangci-lint
	$(GOTOOLS_BINDIR)/golangci-lint version
	$(GOTOOLS_BINDIR)/golangci-lint linters
	GL_DEBUG=gocritic $(GOTOOLS_BINDIR)/golangci-lint run

unit:
	cargo test --no-fail-fast

integration: .install.ginkgo release # It needs to be release so we correctly test the RSS usage
	export CONMON_BINARY="$(MAKEFILE_PATH)target/release/$(BINARY)" && \
	export RUNTIME_BINARY="$(RUNTIME_PATH)" && \
	export MAX_RSS_KB=10240 && \
	sudo -E "$(GOTOOLS_BINDIR)/ginkgo" $(TEST_FLAGS) $(GINKGO_FLAGS)

integration-static: .install.ginkgo # It needs to be release so we correctly test the RSS usage
	export CONMON_BINARY="$(MAKEFILE_PATH)target/x86_64-unknown-linux-gnu/release/$(BINARY)" && \
	if [ ! -f "$$CONMON_BINARY" ]; then \
		$(MAKE) release-static; \
	fi && \
	export RUNTIME_BINARY="$(RUNTIME_PATH)" && \
	export MAX_RSS_KB=4500 && \
	sudo -E "$(GOTOOLS_BINDIR)/ginkgo" $(TEST_FLAGS) $(GINKGO_FLAGS)

.install.ginkgo:
	GOBIN=$(abspath $(GOTOOLS_BINDIR)) go install github.com/onsi/ginkgo/v2/ginkgo@latest

.install.golangci-lint:
	curl -sSfL https://raw.githubusercontent.com/golangci/golangci-lint/master/install.sh | \
		BINDIR=$(abspath $(GOTOOLS_BINDIR)) sh -s v1.49.0

clean:
	rm -rf target/

update-proto:
	go install capnproto.org/go/capnp/v3/capnpc-go@latest
	cat $(PROTO_PATH)/go-patch >> $(PROTO_PATH)/conmon.capnp
	capnp compile \
		-I$$GOPATH/src/capnproto.org/go/capnp/std \
		-ogo $(PROTO_PATH)/conmon.capnp
	mv $(PROTO_PATH)/conmon.capnp.go internal/proto/
	git checkout $(PROTO_PATH)/conmon.capnp

.PHONY: lint lint-go lint-rust clean unit integration update-proto

.PHONY: create-release-packages
create-release-packages: release
	if [ "$(PACKAGE_NAME)" != "conmonrs-$(CI_TAG)" ]; then \
		echo "crate version and tag mismatch" ; \
		exit 1 ; \
	fi
	cargo vendor -q && tar zcf $(PACKAGE_NAME)-vendor.tar.gz vendor && rm -rf vendor
	git archive --format tar --prefix=conmonrs-$(CI_TAG)/ $(CI_TAG) | gzip >$(PACKAGE_NAME).tar.gz


.PHONY: install
install:
	mkdir -p "${DESTDIR}$(PREFIX)/bin"
	install -D -t "${DESTDIR}$(PREFIX)/bin" target/release/conmonrs

# Only meant to build the latest HEAD commit + any uncommitted changes
# Not a replacement for the distro package
.PHONY: rpm
rpm:
	rpkg local
