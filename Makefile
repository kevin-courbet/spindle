generate:
	./codegen/generate.sh

build: generate
	cargo build
