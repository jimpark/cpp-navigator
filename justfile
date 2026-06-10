build:
    cargo build --release

test:
    cargo test

fmt:
    cargo fmt
    cargo clippy --fix --allow-dirty

install: build
    cp target/release/cpp-navigator.exe /c/tools/cpp-navigator.exe
