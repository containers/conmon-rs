MAKEFILE_PATH := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
RUNTIME_PATH ?= "/usr/bin/runc"
PROTO_PATH ?= "conmon-rs/common/proto"
BINARY := conmon-server

default:
	cargo build

release:
	cargo build --release

.PHONY: release-static
release-static:
	mkdir -p ~/.cargo/git
	podman run -it \
		--pull always \
		-v "$(shell pwd)":/volume \
		-v ~/.cargo/registry:/root/.cargo/registry \
		-v ~/.cargo/git:/root/.cargo/git \
		clux/muslrust:stable \
		bash -c "\
			apt-get update && \
			apt-get install -y capnproto && \
			rustup component add rustfmt && \
			make release && \
			strip -s target/x86_64-unknown-linux-musl/release/$(BINARY)"

lint:
	cargo fmt

unit:
	cargo test --bins --no-fail-fast

integration: release # It needs to be release so we correctly test the RSS usage
	CONMON_BINARY="$(MAKEFILE_PATH)target/release/$(BINARY)" RUNTIME_BINARY="$(RUNTIME_PATH)" go test -v pkg/client/*

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

.PHONY: lint clean unit integration update-proto
