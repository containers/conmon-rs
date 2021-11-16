default:
	cargo build

release:
	cargo build --release

lint:
	cargo fmt

unit:
	cargo test --bins --no-fail-fast

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
