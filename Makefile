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


.PHONY: lint clean unit integration
