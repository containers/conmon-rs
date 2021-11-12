default:
	cargo build

release:
	cargo build --release

lint:
	cargo fmt

unit:
	cargo test --bins --no-fail-fast

integration:
	cargo test --test integration --release -- --nocapture

clean:
	rm -rf target/


.PHONY: lint clean unit integration
