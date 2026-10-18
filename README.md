# smd

smd is a terminal markdown reader. It shows mermaid diagrams, maths, headings and wide tables as images using the kitty graphics protocol, and keeps everything else as plain text you can select. It is at an early stage: for now it shows a markdown file as styled text, and diagrams, maths and tables as their source.

## Build

You need a recent stable Rust toolchain.

```sh
cargo build --release
./target/release/smd notes.md
```

Scroll with `j`/`k`, the arrow keys, PgUp/PgDn and `g`/`G`. Press `q` to quit.
