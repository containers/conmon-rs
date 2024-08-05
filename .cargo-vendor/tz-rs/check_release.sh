#!/bin/sh

cargo +1.45 test --no-default-features
cargo +1.45 test --no-default-features --features=alloc
cargo +1.45 test --no-default-features --features=std

cargo +1.55 test --no-default-features --features=const
cargo +1.55 test --no-default-features --features=const,alloc
cargo +1.55 test --no-default-features --features=const,std

cargo +stable test --no-default-features --features=const
cargo +stable test --no-default-features --features=const,alloc
cargo +stable test --no-default-features --features=const,std

cargo +nightly test --no-default-features --features=const
cargo +nightly test --no-default-features --features=const,alloc
cargo +nightly test --no-default-features --features=const,std
