MAKEFILE_PATH := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
RUNTIME_PATH ?= "/usr/bin/runc"
PROTO_PATH ?= "conmon-rs/common/proto"

default:
	cargo build

release:
	cargo build --release

lint:
	cargo fmt

unit:
	cargo test --bins --no-fail-fast

integration: release # It needs to be release so we correctly test the RSS usage
	CONMON_BINARY="$(MAKEFILE_PATH)target/release/conmon-server" RUNTIME_BINARY="$(RUNTIME_PATH)" go test -v pkg/client/*

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
