MAKEFILE_PATH := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
RUNTIME_PATH ?= "/usr/bin/runc"

default:
	cargo build

release:
	cargo build --release

lint:
	cargo fmt

unit:
	cargo test --bins --no-fail-fast

integration: default
	CONMON_BINARY="$(MAKEFILE_PATH)target/debug/conmon-server" RUNTIME_BINARY="$(RUNTIME_PATH)" go test -v pkg/client/*

clean:
	rm -rf target/

update-proto:
	go install capnproto.org/go/capnp/v3/capnpc-go@latest
	cat proto/go-patch >> proto/conmon.capnp
	capnp compile \
		-I$$GOPATH/src/capnproto.org/go/capnp/std \
		-ogo proto/conmon.capnp
	mv proto/conmon.capnp.go internal/proto/
	git checkout proto/conmon.capnp


.PHONY: lint clean unit integration update-proto
