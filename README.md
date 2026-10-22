# smd

smd is a terminal markdown reader. It shows mermaid diagrams, maths and headings as images using the kitty graphics protocol, and keeps everything else as plain text you can select. It is at an early stage: for now it shows a markdown file as styled text, and diagrams and maths as their source.

Tables are drawn as text. A table wider than the pane wraps its cells. A table with too many columns to wrap well is shown as one card per row, with a `Header: value` line for each cell.

Code blocks are drawn on a panel a little lighter than the terminal's background (darker on a light one). If the terminal does not say what its background is, or with `--no-graphics`, they are indented instead.

## Build

You need a recent stable Rust toolchain.

```sh
cargo build --release
./target/release/smd notes.md
```

Scroll with `j`/`k`, the arrow keys, PgUp/PgDn, Ctrl-D/Ctrl-U (half a page), Ctrl-F/Ctrl-B and `g`/`G`. Press `q` to quit.

smd reloads the file when it changes on disk, including when an editor saves by renaming a new file over it. If the file is deleted, smd keeps showing the last version and says so on the bottom row.

A paragraph that holds only a local PNG, GIF (first frame) or JPEG image, such as `![diagram](diagram.png)`, is shown as the image, read from the markdown file's folder. This needs a terminal with kitty Unicode-placeholder support; it has been tested in Ghostty, directly and inside herdr. smd asks the terminal at start-up; in a terminal without kitty graphics, or with `--no-graphics`, images, diagrams and maths are shown as their markdown source.
