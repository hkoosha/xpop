export RUST_BACKTRACE := 'full'

[private]
@def:
  just build

[private]
@c:
  for i in {0..100}; do echo; done

clean:
  cargo clean

fmt: c
  cargo fmt

clippy: fmt c
  cargo clippy -- -A clippy::needless_return

fix: fmt c
  cargo clippy --fix -- -A clippy::needless_return

run *args: build c
  @cargo run -- {{ args }}

build: c
  cargo build

release:
  cargo build --release
  ls -lh target/release/xpop

install: release
  sudo install -v \
    -g root -o root \
    ./target/release/xpop /usr/bin/xpop

exe-help: (run '-h')
exe-xterm: (run 'xterm')

