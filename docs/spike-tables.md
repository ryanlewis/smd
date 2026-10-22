# Spike: tables wider than the pane

Today smd draws a table as text when it fits the pane, and as an image shrunk to the pane width when it does not. A wide table ends up with tiny text: the comparison table in `gradle-ci-caching/README.md` shows at about a third of normal size in a 120-column pane.

This spike checks whether wrapping cells works better, and which rule should choose between the ways of showing a table.

**Recommendation: wrap cells as text, fall back to cards, and stop drawing tables as images.** The rule is at the end.

## What was tested

`examples/tables/` is a throwaway example. It is not part of the smd binary. `table.rs` is a copy of `src/table.rs` with cell wrapping added. Run it with:

```sh
cargo run --release --example tables -- [--widths 80,120,160,200] [--all] FILE...
```

For every table and every pane width it prints one line: the choice, how big the text in today's image would be, the same for an image with cells wrapped at 40 characters ("capped"), and the line counts for cards and wrapped text. `--show` draws the choice, and `--all` draws every option, images included (kitty direct placement, so Ghostty or kitty, not inside herdr).

It was run on 20 tables from 5 documents at 80, 120, 160 and 200 columns: `gradle-ci-caching/README.md` and four project notes in the vault. Together they cover 2 to 9 columns, short values, long prose cells, paths, MAC addresses and a 79-row table.

Image text size is measured as a character's width in the image, as shown, against a terminal cell's width. When an image is shrunk to fit, this does not depend on the cell size.

## Findings

**Wrapped text beats today's image almost everywhere.** All three tables in the gradle README wrap with no broken words at 120 columns and up, at full size. Today's image shows them at 34–75%. Text also stays selectable.

**Long rows are fine when the column is wide.** The first attempt rejected wrapping when rows grew past 3 lines on average. That was wrong: a table whose last column is a paragraph read well at 5–7 lines a row, and as a capped image it was 117 rows tall. Rows that grow are not the problem. Narrow columns and broken words are.

**Each column needs room for its longest word first.** Sharing the spare width only in proportion to how much text a column holds starves columns of short, unbreakable values: a MAC address column got 7 characters while a vendor name column got 15. Giving each column its longest word (up to 30 characters, so a URL cannot take the table) before sharing the rest fixed this.

**Some tables have too many columns for any table layout.** Seven to nine columns at 80–120 characters gave columns of 4–12 characters, broken words and rows of 13–37 lines. The capped image was no better, at 26–55%. Cards worked: each row as a block of `Header: value` lines, values wrapped under themselves. They read well and never broke a word in any test.

**The capped image never won.** With cards available, the capped image was never the best choice in 80 cases. When there are few columns, wrapped text is shorter and full size. When there are many, the image text is too small to read.

## Results

| Choice | Cases (of 80) |
|---|---|
| Text as it is | 14 |
| Wrapped text | 57 |
| Cards | 9 |
| Capped image | 0 |

Cards went to five tables with 5 to 9 columns at 80 columns, and to two prose-heavy tables with 7 and 8 columns at 120 to 200 columns.

## The rule

1. The table fits as it is: draw it as text, as today.
2. Otherwise wrap the cells. Each column gets its longest word (at most 30 characters) if all of those fit, or else its natural width or 12 characters, whichever is less. The spare width is shared in proportion to how much more each column wants. Use the wrapped table if it breaks at most 2 words mid-word (breaks after `/`, `-`, `_`, `.`, `,`, `:` or `=` do not count) and takes no more lines than cards would.
3. Otherwise draw cards.

The constants (30, 12, 2) are set from this test set, which is small.

## What this means for smd

- Tables no longer need images. The table image, its cache key, the pixel limit and the missing-glyph check can go. Layout depends on the pane width, but it is text, so redoing it on resize is cheap.
- A table inside a quote or list wraps to the width left after the markers, like other text.
- The non-graphics path improves too: today a wide table without graphics is cut at the edge.

## Open questions

- **Rows run together when they wrap.** With several lines per row and no rule between rows, it can be hard to see where one row ends. Done after the spike: smd draws a dashed rule between body rows when any row wraps.
- **Header cells wrap like any other cell.** A narrow column can split its header over two lines. This is readable, but a header could be kept whole when it fits.
- **The table images have a proportional look that text does not.** Dropping them is a visual change, so it is worth checking against real documents before removing the code.
