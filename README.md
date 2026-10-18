# smd

smd is a terminal markdown reader. It shows mermaid diagrams, maths, headings and wide tables as images using the kitty graphics protocol, and keeps everything else as plain text you can select. It is at an early stage: for now the binary only prints its version.

## Build

You need a recent stable Rust toolchain.

```sh
cargo build --release
./target/release/smd --version
```
