//! Notes embedded in notes: an Obsidian `![[note]]` alone in a paragraph is
//! shown as the note's blocks in place, and `![[note#Heading]]` as that
//! heading's section. An embed of a block, `![[note#^id]]`, shows the whole
//! note. One that cannot be found, that would embed a note already being
//! embedded, or that is deeper than [`MAX_DEPTH`] stays a link.

use std::path::{Path, PathBuf};

use crate::doc::{self, Block, BlockKind, Document, Inline, Link};
use crate::images;
use crate::theme::Theme;
use crate::vault::{self, Target, Vault};

/// The most notes nested inside one another below the note shown.
pub const MAX_DEPTH: usize = 3;

/// Swap each lone note embed in `doc`, the note at `path`, for the note.
pub fn embed_notes(vault: &Vault, doc: &mut Document, path: &Path, theme: Option<Theme>) {
    let mut embedder = Embedder {
        vault,
        theme,
        host: path,
        seen: vec![same_file(path)],
    };
    embedder.embed_in(&mut doc.blocks, &doc.source, path);
}

struct Embedder<'a> {
    vault: &'a Vault,
    theme: Option<Theme>,
    /// The note shown, as it was named.
    host: &'a Path,
    /// The notes being embedded, the note shown first.
    seen: Vec<PathBuf>,
}

impl Embedder<'_> {
    fn embed_in(&mut self, blocks: &mut [Block], source: &str, from: &Path) {
        for block in blocks {
            if let Some((title, link)) = lone_embed(block, source) {
                if let Some(doc) = self.embedded(&link, from) {
                    block.kind = BlockKind::Embed { title, link, doc };
                }
                continue;
            }
            match &mut block.kind {
                BlockKind::Quote(inner) | BlockKind::Callout { blocks: inner, .. } => {
                    self.embed_in(inner, source, from)
                }
                BlockKind::List { items, .. } => {
                    for item in items {
                        self.embed_in(&mut item.blocks, source, from);
                    }
                }
                _ => {}
            }
        }
    }

    /// The note, or its section, that `link` in the note at `from` embeds,
    /// parsed, with its own embeds swapped in and its links and images made
    /// to work from the note shown.
    fn embedded(&mut self, link: &Link, from: &Path) -> Option<Document> {
        if self.seen.len() > MAX_DEPTH {
            return None;
        }
        let Target::Note { path, heading } = self.vault.follow(link, from) else {
            return None;
        };
        let path = absolute(path);
        let file = same_file(&path);
        if self.seen.contains(&file) {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let mut doc = doc::parse_for(&text, self.theme);
        doc.blocks
            .retain(|b| !matches!(b.kind, BlockKind::Frontmatter(_)));
        if let Some(heading) = heading {
            doc.blocks = section(std::mem::take(&mut doc.blocks), &heading)?;
        }
        self.seen.push(file);
        self.embed_in(&mut doc.blocks, &doc.source, &path);
        self.seen.pop();
        self.rebase(&mut doc.blocks, &path);
        Some(doc)
    }

    /// Point the links and images in blocks from the note at `from` at what
    /// they lead to there, so they lead to the same place from the note
    /// shown.
    fn rebase(&self, blocks: &mut [Block], from: &Path) {
        for block in blocks {
            match &mut block.kind {
                BlockKind::Paragraph(content) | BlockKind::Heading { content, .. } => {
                    for inline in content {
                        if let Inline::Text { style, .. } = inline
                            && let Some(link) = &mut style.link
                        {
                            *link = self.rebased(link, from);
                        }
                    }
                }
                BlockKind::Image { src, wiki, .. } => {
                    let found = if *wiki {
                        self.vault.wiki(src, from)
                    } else {
                        is_relative(src).then(|| folder(from).join(&*src))
                    };
                    if let Some(path) = found {
                        *src = absolute(path).to_string_lossy().into_owned();
                        *wiki = false;
                    }
                }
                BlockKind::Quote(inner) | BlockKind::Callout { blocks: inner, .. } => {
                    self.rebase(inner, from)
                }
                BlockKind::List { items, .. } => {
                    for item in items {
                        self.rebase(&mut item.blocks, from);
                    }
                }
                // Its own blocks were rebased on its own note.
                BlockKind::Embed { link, .. } => *link = self.rebased(link, from),
                _ => {}
            }
        }
    }

    /// `link` in the note at `from` as a path to the file it leads to, with
    /// the heading, if any. A link back to the note shown leads into it, as
    /// that note's own links do. A web address is kept, and so is a wiki
    /// link that leads nowhere, which leads nowhere from any note in the
    /// vault.
    fn rebased(&self, link: &Link, from: &Path) -> Link {
        let (path, heading) = match self.vault.follow(link, from) {
            Target::Note { path, heading } => (path, heading),
            Target::Heading(heading) => (from.to_path_buf(), Some(heading)),
            Target::Other(path) => (path, None),
            Target::Missing(name) if !link.wiki => (folder(from).join(name), None),
            Target::Web(_) | Target::Missing(_) => return link.clone(),
        };
        // `follow` decodes these, and takes a `#` as the start of a heading.
        let encode = |s: &str| s.replace('%', "%25");
        let mut dest = if same_file(&path) == self.seen[0] {
            match &heading {
                Some(_) => String::new(),
                None => self
                    .host
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            }
        } else {
            absolute(path).to_string_lossy().into_owned()
        };
        dest = encode(&dest).replace('#', "%23");
        if let Some(heading) = heading {
            dest.push('#');
            dest.push_str(&encode(&heading));
        }
        Link { dest, wiki: false }
    }
}

/// The text and link of a paragraph that is nothing but one `![[note]]`.
fn lone_embed(block: &Block, source: &str) -> Option<(String, Link)> {
    let BlockKind::Paragraph(content) = &block.kind else {
        return None;
    };
    // A paragraph in a callout can start at its line's quote marker.
    let written = source.get(block.span.clone())?.trim();
    let written = written.trim_start_matches(|c: char| c == '>' || c.is_whitespace());
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

fn is_relative(src: &str) -> bool {
    images::is_local(src) && Path::new(src).is_relative()
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
    fn an_embed_in_a_callout_is_framed_there_and_its_links_rebased() {
        let v = TempVault::new(
            "callout",
            &[
                ("a.md", "> [!note] Seen\n> ![[sub/b]]"),
                ("sub/b.md", "[[c]]\n\n> [!tip]- Folded\n> [[c]]"),
                ("sub/c.md", ""),
            ],
        );
        let doc = v.open("a.md");
        let BlockKind::Callout { blocks, .. } = &doc.blocks[0].kind else {
            panic!("no callout: {:?}", doc.blocks);
        };
        let BlockKind::Embed { doc: inner, .. } = &blocks[0].kind else {
            panic!("not embedded: {blocks:?}");
        };
        let c = format!("{}/c.md", absolute(v.0.join("sub")).display());
        let BlockKind::Callout { blocks, .. } = &inner.blocks[1].kind else {
            panic!("no inner callout");
        };
        for paragraph in [&inner.blocks[0], &blocks[0]] {
            let BlockKind::Paragraph(content) = &paragraph.kind else {
                panic!("no paragraph");
            };
            let Inline::Text { style, .. } = &content[0] else {
                panic!("no text");
            };
            assert_eq!(style.link.as_ref().unwrap().dest, c);
        }
        // The folded callout inside the embed is not a stop.
        let layout = render(&doc, 40, &Images::new((8, 16)));
        assert_eq!(layout.folds.len(), 0);
    }

    /// `path` relative to the current folder, as when smd is started there.
    fn relative(path: &Path) -> PathBuf {
        let here = std::env::current_dir().unwrap();
        let ups = here.components().count() - 1;
        let mut out: PathBuf = std::iter::repeat_n("..", ups).collect();
        out.push(path.strip_prefix("/").unwrap());
        out
    }

    /// The link destinations in the first paragraph of the first embed.
    fn embedded_dests(doc: &Document) -> Vec<String> {
        let BlockKind::Embed { doc, .. } = &doc.blocks[0].kind else {
            panic!("not embedded: {:?}", doc.blocks);
        };
        let BlockKind::Paragraph(content) = &doc.blocks[0].kind else {
            panic!("no paragraph");
        };
        content
            .iter()
            .filter_map(|i| match i {
                Inline::Text { style, .. } => style.link.as_ref().map(|l| l.dest.clone()),
                Inline::Break => None,
            })
            .collect()
    }

    #[test]
    fn a_wiki_image_in_an_embed_is_found_from_a_vault_named_relatively() {
        let v = TempVault::new(
            "relimage",
            &[
                ("a.md", "![[sub/b]]"),
                ("sub/b.md", "![[p.png]]"),
                ("pics/p.png", ""),
            ],
        );
        let path = relative(&v.0.join("a.md"));
        let source = std::fs::read_to_string(&path).unwrap();
        let mut doc = doc::parse(&source);
        embed_notes(&Vault::for_file(&path), &mut doc, &path, None);
        let BlockKind::Embed { doc, .. } = &doc.blocks[0].kind else {
            panic!("not embedded");
        };
        let BlockKind::Image { src, .. } = &doc.blocks[0].kind else {
            panic!("no image");
        };
        assert_eq!(
            same_file(Path::new(src)),
            same_file(&v.0.join("pics/p.png"))
        );
    }

    #[test]
    fn a_link_from_an_embed_back_to_the_note_shown_stays_in_it() {
        let v = TempVault::new(
            "back",
            &[
                ("a.md", "![[sub/b]]\n\n# Top"),
                ("sub/b.md", "[[a#Top]] [[a]]"),
            ],
        );
        let path = relative(&v.0.join("a.md"));
        let source = std::fs::read_to_string(&path).unwrap();
        let mut doc = doc::parse(&source);
        let vault = Vault::for_file(&path);
        embed_notes(&vault, &mut doc, &path, None);
        let dests = embedded_dests(&doc);
        assert_eq!(dests, ["#Top", "a.md"]);
        let back = Link::from("a.md");
        assert_eq!(
            vault.follow(&back, &path),
            Target::Note {
                path: path.clone(),
                heading: None
            }
        );
    }

    #[test]
    fn a_link_missing_from_an_embed_does_not_find_a_file_beside_the_note_shown() {
        let v = TempVault::new(
            "missing",
            &[
                ("a.md", "![[sub/b]]"),
                ("sub/b.md", "[t](todo.md)"),
                ("todo.md", ""),
            ],
        );
        let doc = v.open("a.md");
        let dests = embedded_dests(&doc);
        let vault = Vault::for_file(&v.0.join("a.md"));
        let got = vault.follow(&Link::from(dests[0].as_str()), &v.0.join("a.md"));
        assert!(matches!(got, Target::Missing(_)), "{got:?}");
    }
}
