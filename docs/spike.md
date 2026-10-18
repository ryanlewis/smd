# Spike: kitty Unicode-placeholder images in a herdr pane

Issue #2. This spike checks whether smd can show images with the kitty graphics protocol, using Unicode placeholders (`U=1`), inside a herdr pane without slowing herdr down.

**Recommendation: go with limits.** The limits are listed at the end.

## What was tested

`examples/spike/` is a throwaway example. It is not part of the smd binary. Run it with `cargo run --release --example spike`. It shows a page of plain text with three extras:

- **A heading drawn as an image.** The example draws the text at run time with an 8x8 bitmap font (`font8x8`). The image is 2432x64 px and two cell rows tall. It is sent as raw RGBA (`f=32`), which is 622,592 bytes before base64.
- **A mermaid diagram.** `examples/spike/diagram.png` (1568x224 px, 43,657 bytes) is sent as PNG (`f=100`). It was rendered once from `diagram.mmd` with mermaid-cli 12.0.0 (`mmdc`, installed with Homebrew), using `mmdc -i diagram.mmd -o diagram.png -t dark -b transparent -s 2`.
- **A heading sent with OSC 66** (`ESC ] 66 ; s=2 ; text BEL`).

Each image is uploaded once (`a=t`) and then placed with a virtual placement (`a=p,U=1,c=…,r=…`). Each image cell is printed as U+10EEEE with a row diacritic and a column diacritic. The foreground colour carries the image id as 24-bit RGB. Scrolling only reprints text lines. Images are never sent again.

On resize, the example asks for the cell size again (`CSI 16 t`), works out new column and row counts, and places the images again without uploading them again. On exit it deletes both images (`a=d,d=I`).

The status line and the summary printed on exit show these timings:

- **upload**: from the first byte of the upload to the terminal's `OK` reply.
- **redraw**: from writing a frame to the reply to a `CSI 5 n` sent straight after it.
- **place**: the same round-trip, measured after placing the images.

Flags:

- `--bench N --delay MS` scrolls N frames on its own.
- `--no-images` is a text-only control run.
- `--reupload` uploads both images again on every frame. It stands in for an app that keeps re-sending image data, which is how this spike reads terminal-browser's problem. terminal-browser itself was not tested.
- `--log FILE` saves the summary to a file.

## Setup

| | |
|---|---|
| Machine | Mac, macOS 27.0.1, Retina display, cell size 17x34 px |
| Outer terminal | Ghostty 1.3.1 |
| herdr | 0.9.5, `kitty_graphics = true` (the default) |
| kitty | **Not observed.** kitty is not installed, and Ryan chose to test only Ghostty. |

The herdr runs used a separate herdr session (`herdr --session smd-spike`) in its own Ghostty window. This avoided switching tabs in the main session. It is the same herdr binary and configuration. The panes were driven with `herdr pane run`, `send-text` and `wait-output`. Each window was captured with `screencapture -l` and checked by eye from the screenshot. The scratch session and windows were removed afterwards.

Each bench scrolls one line every 16 ms for 900 frames, about 17 s.

While a herdr bench ran, a probe script did three things:

- **Typing:** typed a unique token into a `cat` pane next to the spike pane with `herdr pane send-text`, then timed until `herdr pane wait-output` saw it.
- **Tab switching:** timed `herdr tab focus` to a second tab and back.
- **CPU:** sampled `%cpu` of the herdr server, the herdr client and Ghostty every 0.5 s for 10 s.

In the plain Ghostty runs it sampled only Ghostty's CPU.

Note on the harness: `open -n -a Ghostty --args --command=…` also runs the command in a hidden 500x500 Ghostty window. The first runs therefore had a second copy of the example, or a second herdr client, running. The numbers below come from runs with a wrapper that exits in that hidden window, so only one copy ran.

## Results

### Do the images show, stay in place on scroll and survive resize?

| | Ghostty directly | Ghostty + herdr | kitty directly | kitty + herdr |
|---|---|---|---|---|
| Images show | Yes, both | Yes, both | not observed | not observed |
| Stay in place when scrolling | Yes. They move with the text and are cut cleanly at the top and bottom edges. | Yes, the same | not observed | not observed |
| Survive resize | Yes. The window was resized from a script; images are placed again at the new width and text rewraps. | Yes. A herdr split narrowed the pane, the heading was placed again smaller, text rewrapped. | not observed | not observed |
| Survive switching herdr tabs | n/a | Yes. Nothing is left on the other tab, and both images return on switching back. | n/a | not observed |
| Gone after exit | Yes, no leftover image | Yes, no leftover image | not observed | not observed |

What screenshots cannot show: whether scrolling flickers or tears between frames. Screenshots are single frames, and no human watched the runs.

### Is herdr still responsive with images on screen?

These numbers are herdr API round-trips (process start plus herdr's socket plus the pane's echo). They show whether herdr keeps up. They are not the delay a person sees on screen, which needs a human to judge.

| Run (herdr pane 86x41 cells) | Typing in the next pane | Tab away and back | herdr server CPU | herdr client CPU | Ghostty CPU |
|---|---|---|---|---|---|
| Idle, no spike running | 18–29 ms | 16–20 ms | 2.0% | 0.3% | 1.4% |
| Scrolling text only | 17–31 ms | 16–22 ms | 7.3% | 3.8% | 14.3% |
| Scrolling with both images | 17–32 ms | 17–26 ms | 11.9% | 8.2% | 14.7% |
| Re-uploading both images every frame | 17–20 ms | 16–17 ms | **39.4%** | 3.2% | 5.0% |

Typing and tab switching took the same time in every run. With images uploaded once, scrolling them costs herdr about 5 more points of server CPU and 4 more points of client CPU than scrolling plain text.

Re-uploading every frame (about 890 KB of base64 per frame) pushes the herdr server to about 40% CPU, and the scroll rate drops from about 53 to about 36 frames a second. Part of that drop is the example's own work: in this mode it base64-encodes both images again and places them again (two extra round-trips) on every frame. Images still showed during that run. Ghostty's CPU fell to 5% at the same time, which suggests herdr does not forward each re-upload to Ghostty. That was not confirmed.

Plain Ghostty, for comparison (window 151x44 cells, same bench):

| Run | Ghostty CPU |
|---|---|
| Scrolling text only | 12.3% |
| Scrolling with both images | 14.0% |
| Re-uploading both images every frame | 56.8% |

### Rough cost

| | Ghostty directly | Ghostty + herdr |
|---|---|---|
| Upload heading (622 KB raw RGBA, 830 KB base64) | 10–22 ms | 7–11 ms |
| Upload diagram (44 KB PNG) | 2–5 ms | 2–4 ms |
| Place both images after a resize | under 0.1 ms | under 0.1 ms |
| Redraw on scroll, with images (median / p95) | 0.38 / 0.55 ms | 0.22 / 0.49 ms |
| Redraw on scroll, text only (median / p95) | 0.25 / 0.29 ms | 0.19 / 0.23 ms |

How to read this:

- **Inside herdr, herdr very likely answers the replies itself.** herdr keeps its own screen for each pane, so the graphics `OK` and the `CSI 5 n` reply most likely come from herdr, not from Ghostty. Two things point that way: uploads were acknowledged faster inside herdr (7 ms) than in plain Ghostty (10–22 ms), and a 3000-frame run with no delay finished in 0.2 s. This was not confirmed from herdr's source. If it holds, the herdr numbers measure how fast herdr takes in the data, not how fast Ghostty draws it.
- **Directly, Ghostty answers from its parser, not its renderer.** So the redraw figures are the cost of sending a frame, not the time to paint it. For drawing cost, the CPU tables above are the better guide.

### Does OSC 66 text sizing work for headings?

| | Result |
|---|---|
| Ghostty directly | **No.** Ghostty 1.3.1 drew nothing for the OSC 66 heading: the two rows were blank, with not even unscaled text. |
| Ghostty + herdr | **No.** The rows were blank on screen, and `herdr pane read` shows no text there either, so herdr drops OSC 66 rather than passing it on. Because Ghostty ignores OSC 66 anyway, this spike cannot tell whether herdr would pass it on to kitty. |
| kitty | not observed |

## Where Ghostty and herdr differ from kitty's spec

From what was seen here. kitty itself was not tested.

- **Unicode placeholders work** in Ghostty and through herdr, when every cell carries both a row and a column diacritic. Leaving the diacritics out so the terminal infers them (allowed by the spec) was not tested.
- **OSC 66 is not supported** by Ghostty 1.3.1. herdr does not keep it in the pane either.
- **Image deletion** (`a=d,d=I,i=…`) left nothing behind in either. Whether Ghostty freed the image memory was not checked.
- **herdr very likely acts as the terminal for the pane** and sends the graphics `OK` and `CSI 5 n` replies itself (see "Rough cost"). If so, an app inside herdr cannot use those replies to tell whether the outer terminal has caught up. `CSI 16 t` still gave the right cell size (17x34).
- **Re-uploads inside herdr seem to be absorbed by herdr.** Ghostty's CPU stayed low while the herdr server's CPU went up.

## Recommendation: go with limits

Unicode-placeholder images work in Ghostty, with and without herdr. herdr stays responsive while they scroll, so smd is viable. Build it within these limits:

1. **Upload each image once, then place it with `U=1` and print placeholder cells.** Never re-send image data on scroll or redraw. Re-sending is what cost herdr about 40% CPU here. On resize, place the image again and only upload again if the image itself must change (for example, a heading drawn for a new width).
2. **Do not use OSC 66 for headings.** Neither Ghostty nor herdr shows it. Draw headings as images, or as plain styled text.
3. **Write both diacritics on every placeholder cell.** That is what was tested and works. Inferred diacritics are untested.
4. **Get the cell size from `CSI 16 t`.** It works directly and in herdr (17x34 here). Fall back to `TIOCGWINSZ` pixels.
5. **Keep uploads small.** Send PNG (`f=100`) or compressed data (`o=z`) rather than raw RGBA. The 622 KB raw heading took 7–22 ms to upload (herdr and plain Ghostty together), and the 44 KB PNG took 2–5 ms.
6. **Do not rely on terminal replies for flow control inside herdr.** herdr very likely answers them itself.
7. **kitty is unverified.** smd's docs say "kitty graphics protocol", but nothing here was checked in kitty. Test kitty, with and without herdr, before claiming kitty support, and use that test for the OSC 66 question too.

## Follow-ups (not done here)

- Run the same spike in kitty, directly and in herdr, including whether herdr passes OSC 66 on to kitty.
- Have a person watch a held-key scroll in herdr for flicker or tearing. Screenshots cannot show it.
- Try inferred diacritics (a bare U+10EEEE after the first cell). It would cut placeholder bytes about threefold.
- Check what selecting and copying text across an image row gives (U+10EEEE characters, or nothing).
