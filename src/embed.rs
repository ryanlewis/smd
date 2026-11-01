//! Notes embedded in notes: an Obsidian `![[note]]` alone in a paragraph is
//! shown as the note's blocks in place, and `![[note#Heading]]` as that
//! heading's section. An embed of a block, `![[note#^id]]`, shows the whole
//! note. One that cannot be found, that would embed a note already being
//! embedded, or that is deeper than [`MAX_DEPTH`] stays a link.

use std::path::{Path, PathBuf};

use crate::doc::{self, Block, BlockKind, Document, Inline, Link};
use crate::theme::Theme;
use crate::vault::{self, Target, Vault};

/// The most notes nested inside one another below the note shown.
pub const MAX_DEPTH: usize = 3;

/// Swap each lone note embed in `doc`, the note at `path`, for the note.
pub fn embed_notes(vault: &Vault, doc: &mut Document, path: &Path, theme: Option<Theme>) {
    let mut seen = vec![same_file(path)];
    embed_in(vault, &mut doc.blocks, &doc.source, path, theme, &mut seen);
}

/// `seen` holds the notes being embedded, the outermost first.
fn embed_in(
    vault: &Vault,
    blocks: &mut [Block],
    source: &str,
    from: &Path,
    theme: Option<Theme>,
    seen: &mut Vec<PathBuf>,
) {
    for block in blocks {
        if let Some((title, link)) = lone_embed(block, source) {
            if let Some(doc) = embedded(vault, &link, from, theme, seen) {
                block.kind = BlockKind::Embed { title, link, doc };
            }
            continue;
        }
        match &mut block.kind {
            BlockKind::Quote(inner) => embed_in(vault, inner, source, from, theme, seen),
            BlockKind::List { items, .. } => {
                for item in items {
                    embed_in(vault, &mut item.blocks, source, from, theme, seen);
                }
            }
            _ => {}
        }
    }
}

/// The text and link of a paragraph that is nothing but one `![[note]]`.
fn lone_embed(block: &Block, source: &str) -> Option<(String, Link)> {
    let BlockKind::Paragraph(content) = &block.kind else {
        return None;
    };
    let written = source.get(block.span.clone())?.trim();
    let inside = written.strip_prefix("![[")?.strip_suffix("]]")?;
    if inside.contains("[[") || inside.contains("]]") {
        return None;
    }
    let mut link: Option<&Link> = None;
    let mut title = String::new();
    for inline in content {
        let Inline::Text { text, style } = inline else {
            return None;
        };
        let this = style.link.as_ref().filter(|l| l.wiki)?;
        if link.is_some_and(|l| l != this) {
            return None;
        }
        link = Some(this);
        title.push_str(text);
    }
    Some((title, link?.clone()))
}

/// The note, or its section, that `link` in the note at `from` embeds,
/// parsed, with its own embeds swapped in and its links and images made to
/// work from any note.
fn embedded(
    vault: &Vault,
    link: &Link,
    from: &Path,
    theme: Option<Theme>,
    seen: &mut Vec<PathBuf>,
) -> Option<Document> {
    if seen.len() > MAX_DEPTH {
        return None;
    }
    let Target::Note { path, heading } = vault.follow(link, from) else {
        return None;
    };
    let path = absolute(path);
    let file = same_file(&path);
    if seen.contains(&file) {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let mut doc = doc::parse_for(without_frontmatter(&text), theme);
    if let Some(heading) = heading {
        doc.blocks = section(std::mem::take(&mut doc.blocks), &heading)?;
    }
    vault.resolve_embeds(&mut doc.blocks, &path);
    seen.push(file);
    embed_in(vault, &mut doc.blocks, &doc.source, &path, theme, seen);
    seen.pop();
    rebase(vault, &mut doc.blocks, &path);
    Some(doc)
}

/// `source` without the `---` block of properties at its top, if it has
/// one.
fn without_frontmatter(source: &str) -> &str {
    let Some(rest) = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))
    else {
        return source;
    };
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        at += line.len();
        if line.trim_end() == "---" {
            return &rest[at..];
        }
    }
    source
}

/// The heading matching `wanted` and the blocks under it, up to the next
/// heading as high or higher.
fn section(blocks: Vec<Block>, wanted: &str) -> Option<Vec<Block>> {
    let level = |b: &Block| match &b.kind {
        BlockKind::Heading { level, .. } => Some(*level),
        _ => None,
    };
    let start = blocks.iter().position(|b| match &b.kind {
        BlockKind::Heading { content, .. } => vault::heading_matches(&plain(content), wanted),
        _ => false,
    })?;
    let top = level(&blocks[start])?;
    let mut out: Vec<Block> = blocks.into_iter().skip(start).collect();
    let end = out[1..]
        .iter()
        .position(|b| level(b).is_some_and(|l| l <= top))
        .map_or(out.len(), |i| i + 1);
    out.truncate(end);
    Some(out)
}

fn plain(content: &[Inline]) -> String {
    content
        .iter()
        .map(|i| match i {
            Inline::Text { text, .. } => text.as_str(),
            Inline::Break => " ",
        })
        .collect()
}

/// Point the links and images in blocks from the note at `from` at what they
/// lead to there, so they lead to the same place from the note embedding
/// them.
fn rebase(vault: &Vault, blocks: &mut [Block], from: &Path) {
    for block in blocks {
        match &mut block.kind {
            BlockKind::Paragraph(content) | BlockKind::Heading { content, .. } => {
                for inline in content {
                    if let Inline::Text { style, .. } = inline
                        && let Some(link) = &mut style.link
                    {
                        *link = rebased(vault, link, from);
                    }
                }
            }
            BlockKind::Image {
                src, wiki: false, ..
            } if is_relative(src) => {
                *src = absolute(folder(from).join(&*src))
                    .to_string_lossy()
                    .into_owned();
            }
            BlockKind::Quote(inner) => rebase(vault, inner, from),
            BlockKind::List { items, .. } => {
                for item in items {
                    rebase(vault, &mut item.blocks, from);
                }
            }
            // Its own blocks were rebased on its own note.
            BlockKind::Embed { link, .. } => *link = rebased(vault, link, from),
            _ => {}
        }
    }
}

/// `link` in the note at `from` as a path to the file it leads to, with the
/// heading, if any. A web address or a link that leads nowhere is kept.
fn rebased(vault: &Vault, link: &Link, from: &Path) -> Link {
    let to = |path: &Path, heading: Option<&str>| {
        // `follow` decodes these, and takes a `#` as the start of a heading.
        let mut dest = path
            .to_string_lossy()
            .replace('%', "%25")
            .replace('#', "%23");
        if let Some(heading) = heading {
            dest.push('#');
            dest.push_str(&heading.replace('%', "%25"));
        }
        Link { dest, wiki: false }
    };
    match vault.follow(link, from) {
        Target::Note { path, heading } => to(&absolute(path), heading.as_deref()),
        Target::Heading(heading) => to(from, Some(&heading)),
        Target::Other(path) => to(&absolute(path), None),
        Target::Web(_) | Target::Missing(_) => link.clone(),
    }
}

fn is_relative(src: &str) -> bool {
    !src.is_empty()
        && !src.contains("://")
        && !src.starts_with("data:")
        && Path::new(src).is_relative()
}

fn folder(file: &Path) -> &Path {
    file.parent().unwrap_or(Path::new("."))
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// The same path for any way of naming one file, to spot a loop.
fn same_file(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| absolute(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::Images;
    use crate::render::render;

    /// A vault in a temporary folder holding `files`, each a name and its
    /// text.
    struct TempVault(PathBuf);

    impl TempVault {
        fn new(name: &str, files: &[(&str, &str)]) -> TempVault {
            let root =
                std::env::temp_dir().join(format!("smd-embed-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(".obsidian")).unwrap();
            for (file, text) in files {
                let path = root.join(file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            TempVault(root)
        }

        /// The first note, `files[0]`, parsed with its embeds swapped in.
        fn open(&self, file: &str) -> Document {
            let path = self.0.join(file);
            let source = std::fs::read_to_string(&path).unwrap();
            let vault = Vault::for_file(&path);
            let mut doc = doc::parse(&source);
            embed_notes(&vault, &mut doc, &path, None);
            doc
        }

        /// The note `file` as plain lines.
        fn lines(&self, file: &str) -> Vec<String> {
            let doc = self.open(file);
            render(&doc, 40, &Images::new((8, 16)))
                .lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        }
    }

    impl Drop for TempVault {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_lone_embed_shows_the_note_in_a_frame_without_its_frontmatter() {
        let v = TempVault::new(
            "lone",
            &[
                ("a.md", "Before\n\n![[b]]\n\nAfter"),
                ("b.md", "---\ntags: [x]\n---\n# B\n\nBody"),
            ],
        );
        assert_eq!(
            v.lines("a.md"),
            [
                "Before", "", "╭─ b", "│ B", "│", "│ Body", "╰─", "", "After"
            ]
        );
    }

    #[test]
    fn a_heading_embed_shows_only_its_section() {
        let v = TempVault::new(
            "section",
            &[
                ("a.md", "![[b#Two]]"),
                (
                    "b.md",
                    "# One\n\nx\n\n## Two\n\ny\n\n### Deeper\n\nz\n\n## Three\n\nw",
                ),
            ],
        );
        let doc = v.open("a.md");
        let BlockKind::Embed { title, doc, .. } = &doc.blocks[0].kind else {
            panic!("not embedded: {:?}", doc.blocks);
        };
        assert_eq!(title, "b#Two");
        let text: Vec<&str> = doc
            .blocks
            .iter()
            .map(|b| doc.source[b.span.clone()].trim())
            .collect();
        assert_eq!(text, ["## Two", "y", "### Deeper", "z"]);
    }

    #[test]
    fn what_cannot_be_embedded_stays_a_link() {
        let v = TempVault::new(
            "links",
            &[
                (
                    "a.md",
                    "![[gone]]\n\n![[b#Nowhere]]\n\nsee ![[b]] here\n\n![[a]]\n\n![[b]] ![[b]]",
                ),
                ("b.md", "# B"),
            ],
        );
        let doc = v.open("a.md");
        assert_eq!(doc.blocks.len(), 5);
        for block in &doc.blocks {
            assert!(matches!(block.kind, BlockKind::Paragraph(_)), "{block:?}");
        }
    }

    #[test]
    fn a_block_embed_shows_the_whole_note() {
        let v = TempVault::new("block", &[("a.md", "![[b#^id]]"), ("b.md", "x\n\ny ^id")]);
        let doc = v.open("a.md");
        let BlockKind::Embed { doc, .. } = &doc.blocks[0].kind else {
            panic!("not embedded");
        };
        assert_eq!(doc.blocks.len(), 2);
    }

    /// How deep the embeds go from the note's first block.
    fn depth(blocks: &[Block]) -> usize {
        match blocks.first().map(|b| &b.kind) {
            Some(BlockKind::Embed { doc, .. }) => 1 + depth(&doc.blocks),
            _ => 0,
        }
    }

    #[test]
    fn a_loop_of_embeds_stops_at_the_note_it_started_from() {
        let v = TempVault::new("loop", &[("a.md", "![[b]]"), ("b.md", "![[a]]")]);
        assert_eq!(depth(&v.open("a.md").blocks), 1);
    }

    #[test]
    fn embeds_nest_no_deeper_than_the_cap() {
        let files: Vec<(String, String)> = (0..6)
            .map(|i| (format!("n{i}.md"), format!("![[n{}]]", i + 1)))
            .collect();
        let files: Vec<(&str, &str)> = files.iter().map(|(a, b)| (&a[..], &b[..])).collect();
        let v = TempVault::new("deep", &files);
        assert_eq!(depth(&v.open("n0.md").blocks), MAX_DEPTH);
    }

    #[test]
    fn links_in_an_embed_lead_where_they_do_from_its_note() {
        let v = TempVault::new(
            "rebase",
            &[
                ("a.md", "![[sub/b]]"),
                (
                    "sub/b.md",
                    "[[#Top]] [[c]] [rel](c.md) [web](https://x.y/)\n\n![p](p.png)",
                ),
                ("sub/c.md", ""),
                ("c.md", ""),
            ],
        );
        let doc = v.open("a.md");
        let BlockKind::Embed { doc, .. } = &doc.blocks[0].kind else {
            panic!("not embedded");
        };
        let BlockKind::Paragraph(content) = &doc.blocks[0].kind else {
            panic!("no paragraph");
        };
        let dests: Vec<String> = content
            .iter()
            .filter_map(|i| match i {
                Inline::Text { style, .. } => style.link.as_ref().map(|l| l.dest.clone()),
                Inline::Break => None,
            })
            .collect();
        let sub = absolute(v.0.join("sub"));
        let sub = sub.display();
        assert_eq!(
            dests,
            [
                format!("{sub}/b.md#Top"),
                format!("{sub}/c.md"),
                format!("{sub}/c.md"),
                "https://x.y/".to_string(),
            ]
        );
        let BlockKind::Image { src, .. } = &doc.blocks[1].kind else {
            panic!("no image");
        };
        assert_eq!(*src, format!("{sub}/p.png"));
    }

    #[test]
    fn a_task_in_an_embed_cannot_be_ticked() {
        let v = TempVault::new(
            "task",
            &[("a.md", "- [ ] mine\n\n![[b]]"), ("b.md", "- [ ] theirs")],
        );
        let doc = v.open("a.md");
        let layout = render(&doc, 40, &Images::new((8, 16)));
        assert_eq!(layout.tasks.len(), 1);
        assert_eq!(layout.tasks[0].mark, 3);
    }

    #[test]
    fn the_title_links_to_the_note_and_blocks_inside_take_the_embeds_span() {
        let v = TempVault::new("title", &[("a.md", "x\n\n![[b]]"), ("b.md", "one\n\ntwo")]);
        let doc = v.open("a.md");
        let layout = render(&doc, 40, &Images::new((8, 16)));
        assert_eq!(layout.links[0].link.dest, "b");
        assert_eq!(layout.links[0].cells, [(2, 3..4)]);
        let spans: Vec<_> = layout.blocks.iter().map(|b| b.span.clone()).collect();
        assert_eq!(spans, [0..2, 3..9, 3..9, 3..9]);
    }

    #[test]
    fn an_embed_in_a_list_or_quote_is_framed_there_under_its_alias() {
        let v = TempVault::new(
            "nested",
            &[("a.md", "- ![[b|Bee]]\n\n> ![[b]]"), ("b.md", "hi")],
        );
        assert_eq!(
            v.lines("a.md"),
            ["• ╭─ Bee", "  │ hi", "  ╰─", "", "│ ╭─ b", "│ │ hi", "│ ╰─"]
        );
    }

    #[test]
    fn frontmatter_is_left_out_only_when_closed() {
        assert_eq!(without_frontmatter("---\na: 1\n---\nbody"), "body");
        assert_eq!(without_frontmatter("---\na: 1\nbody"), "---\na: 1\nbody");
        assert_eq!(without_frontmatter("body\n---\n"), "body\n---\n");
    }
}
