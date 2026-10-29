# smd

smd is a terminal markdown reader. It shows mermaid diagrams, maths and headings as images using the kitty graphics protocol, and keeps everything else as plain text you can select. It is at an early stage: for now it shows a markdown file as styled text, and diagrams and maths as their source.

Tables are drawn as text. A table wider than the pane wraps its cells. A table with too many columns to wrap well is shown as one card per row, with a `Header: value` line for each cell.

Code blocks are drawn on a panel a little lighter than the terminal's background (darker on a light one). If the terminal does not say what its background is, or with `--no-graphics`, they are drawn on `code_background` from the config, or indented if that is not set. Code is coloured by the language named on its fence, such as `rust` or `diff`, in the terminal's own colours.

## Build

You need a recent stable Rust toolchain.

```sh
cargo build --release
./target/release/smd notes.md
```

Scroll with `j`/`k`, the arrow keys, PgUp/PgDn, Ctrl-D/Ctrl-U (half a page), Ctrl-F/Ctrl-B and `g`/`G`. Press `q` to quit.

smd reloads the file when it changes on disk, including when an editor saves by renaming a new file over it. If the file is deleted, smd keeps showing the last version and says so on the bottom row.

A paragraph that holds only a local PNG, GIF (first frame) or JPEG image, such as `![diagram](diagram.png)`, is shown as the image, read from the markdown file's folder. This needs a terminal with kitty Unicode-placeholder support; it has been tested in Ghostty, directly and inside herdr. smd asks the terminal at start-up; in a terminal without kitty graphics, or with `--no-graphics`, images, diagrams and maths are shown as their markdown source.

## Links and Obsidian notes

Press Tab and Shift-Tab to move between links, Enter to follow one, Backspace (or `H`) to go back to where you were, and `L` to go forward again. The toolbar on the top row has Back and Forward buttons, dimmed when there is nowhere to go, and the name of the note. Following a link to another note opens it in smd. Web links and files smd does not show (such as a PDF) are opened with the system's opener, `open` or `xdg-open`.

You can also click a link to follow it, and scroll with the mouse wheel. While smd has the mouse, dragging no longer selects text; most terminals let you select with Option (macOS) or Shift held. With `mouse = false` in the config, smd leaves the mouse to the terminal and marks links as terminal hyperlinks (OSC 8) instead, so the terminal opens them: a web link in the browser, and a note in whatever app opens `.md` files.

Obsidian wiki links work: `[[Note]]`, `[[folder/Note|text]]`, `[[Note#Heading]]` and `[[#Heading]]`. smd finds the vault by looking up from the file for a folder with a `.obsidian` folder in it, and looks up a note by name anywhere in the vault, as Obsidian does. A note outside a vault looks in its own folder and the folders below it. An embedded image, `![[diagram.png]]`, is shown like any other image. An embedded note, `![[Note]]`, is shown as a link to it.

Links inside tables and in headings drawn as images cannot be selected yet.

## Config

smd reads `~/.config/smd/config.toml` (or `$XDG_CONFIG_HOME/smd/config.toml`) if there is one. Every setting is optional:

```toml
# Opened when smd is run with no file, so `smd` alone opens your notes.
home = "~/notes/HOME.md"

# What following a web link does: "open" (the default), "copy" to copy it
# to the clipboard through the terminal, or "ignore".
web_links = "open"

# The colour code blocks are drawn on when the terminal does not say what
# its background is. Without it, they are indented instead.
code_background = "#20242a"

# Whether smd takes the mouse, to follow clicked links and scroll with the
# wheel (the default). With false, links are hyperlinks the terminal opens.
mouse = true

# Keys for each action. Naming an action replaces its default keys.
[keys]
next_link = ["tab", "n"]
back = ["backspace", "h"]
```

The actions and their default keys:

| Action | Default keys |
| --- | --- |
| `quit` | `q`, `esc`, `ctrl-c` |
| `down`, `up` | `j`, `down`; `k`, `up` |
| `half_down`, `half_up` | `ctrl-d`; `ctrl-u` |
| `page_down`, `page_up` | `space`, `pagedown`, `ctrl-f`; `pageup`, `ctrl-b` |
| `top`, `bottom` | `g`, `home`; `G`, `end` |
| `next_link`, `prev_link` | `tab`; `shift-tab` |
| `follow` | `enter` |
| `back`, `forward` | `backspace`, `H`, `alt-left`; `L`, `alt-right` |

A key is a character (`j`, `G`, `?`), or a name: `enter`, `tab`, `shift-tab`, `backspace`, `esc`, `space`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `delete`, `insert` or `f1` to `f24`. Put `ctrl-` or `alt-` in front for a modifier. smd stops with an error that names the problem if the file has a mistake in it, such as an unknown setting or one key set for two actions.
