generate:
	./codegen/generate.sh

build: generate
	cargo build

test:
	cargo test

check:
	cargo clippy
