check:
	cargo check --release
	cargo check --release --no-default-features
	cargo check --release --no-default-features --features "proto"
