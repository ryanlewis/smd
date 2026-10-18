# smd

smd is a terminal markdown reader. It shows mermaid diagrams, maths, headings and wide tables as images using the kitty graphics protocol, and keeps everything else as plain text you can select. It is at an early stage: for now it shows a markdown file as styled text, and diagrams, maths and tables as their source.

## Build

You need a recent stable Rust toolchain.

```sh
cargo build --release
./target/release/smd notes.md
```

Scroll with `j`/`k`, the arrow keys, PgUp/PgDn and `g`/`G`. Press `q` to quit.

smd reloads the file when it changes on disk, including when an editor saves by renaming a new file over it. If the file is deleted, smd keeps showing the last version and says so on the bottom row.
