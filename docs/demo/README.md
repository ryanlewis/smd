# Demo notes and README media

This folder holds the notes shown in the README's recording and screenshots, and the scripts that record them. The empty `.obsidian` folder makes it an Obsidian vault, so the wiki links between the notes resolve and the toolbar shows.

- `Weather station.md`: the main note, with emoji headings, a mermaid diagram, maths, a Rust code block, a table, an image and wiki links.
- `Sensors.md` and `Calibration.md`: small notes it links to.
- `mandelbrot.png`: the image in the main note, made with `ffmpeg -f lavfi -i "mandelbrot=size=480x240:start_scale=2.5:maxiter=200" -frames:v 1 mandelbrot.png`.

## Recording the GIF by hand

`docs/images/demo.gif` is a screen recording made by hand, turned into a GIF by a script.

```sh
cargo build --release
docs/demo/record/open.sh
```

`open.sh` opens smd on `Weather station.md` in a new Ghostty window with the clean config described below, centres it on its display, and saves the window's place to `$TMPDIR/smd-demo/window.txt`. Minimise other windows on that display so only the desktop shows around it. Then record the whole display with Cmd-Shift-5 > Record Entire Screen. With more than one display, click the one the window is on. Use the note, stop the recording from the menu bar, and press q to close the window.

```sh
docs/demo/record/to-gif.sh "$HOME/Desktop/Screen Recording 2026-10-29 at 21.40.00.mov" --start 2 --end 25
```

`to-gif.sh` crops the recording to the window, keeping a margin of desktop around it (`--margin`, default 48 points). It keeps the part between `--start` and `--end` (seconds, or mm:ss). It writes `docs/images/demo.gif`, lowering the width and frame rate until the GIF is under 3 MB (`--max-mb`), and prints the size. If you recorded only part of the screen, pass the crop in video pixels with `--crop W:H:X:Y`.

## Recording automatically

Asciinema and vhs cannot show kitty graphics, so `record.sh` records a real Ghostty window. You need macOS, `/Applications/Ghostty.app`, `ffmpeg`, `mmdc`, the Xcode command line tools (for `swiftc`) and a Retina display (the crop sizes assume 2x).

```sh
cargo build --release
docs/demo/record/record.sh          # the stills; record.sh video or all also replaces demo.gif
```

The script:

1. Opens smd on `Weather station.md` in a new Ghostty window with a clean config (`record/ghostty.conf`): 100x32 cells, no user Ghostty or smd settings, and smd as the only process, so no prompt or path shows.
2. Takes stills with `screencapture -l <window id>`: `diagram.png`, `maths.png`, `table.png` and `links.png`.
3. Records the window's area with `screencapture -V 30 -R ...` while it scrolls through the note, presses Tab and Enter to follow the last link, and clicks the toolbar's Back button.
4. Trims the start to 2 seconds before the first scroll, crops off the title bar, and writes `docs/images/demo.gif` (1000 px wide, 12 fps) and the PNGs (256 colours) with ffmpeg.

It drives keys with `osascript` and clicks with a small Swift helper, so the terminal you run it from needs Accessibility and Screen Recording permission (System Settings > Privacy & Security). Leave the mouse and keyboard alone while it runs, about a minute. The Ghostty window moves to the front and closes when it is done.

Open the results and check every frame you keep: the GIF should start at the top of the note and end back on it after the Back click.
