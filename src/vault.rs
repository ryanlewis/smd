//! Following links between notes: where a link goes, and the Obsidian vault a
//! wiki link is looked up in.
//!
//! A vault is the nearest folder above the file that holds a `.obsidian`
//! folder. A wiki link names a note anywhere in it, as Obsidian does: by path
//! from the vault's top when the name has a `/`, otherwise by file name, the
//! one nearest the linking note winning when several share it. A file outside
//! any vault looks up wiki links in its own folder and those below it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::doc::{Block, BlockKind, Link, is_image};

/// The most files indexed, so a file outside a vault in a huge folder does
/// not walk the whole disk.
const MAX_FILES: usize = 50_000;

/// Folders never looked in.
const SKIP: &[&str] = &[".obsidian", ".git", ".trash", "node_modules", "target"];

/// Where following a link leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A file to open in smd, and the heading to scroll to, if any.
    Note {
        path: PathBuf,
        heading: Option<String>,
    },
    /// A heading in the note already open.
    Heading(String),
    /// A web or mail address, handled as the config says.
    Web(String),
    /// A local file smd does not show, such as a PDF.
    Other(PathBuf),
    /// A link that leads nowhere smd can find, with what it was looking for.
    Missing(String),
}

pub struct Vault {
    /// The folder wiki links are looked up in.
    pub root: PathBuf,
    /// Lower-cased file name to every file with it, made on first use.
    index: OnceLock<HashMap<String, Vec<PathBuf>>>,
}

impl Vault {
    /// The vault `file` is in, or its own folder when it is in none.
    pub fn for_file(file: &Path) -> Vault {
        let dir = folder(file);
        let root = dir
            .ancestors()
            .find(|d| d.join(".obsidian").is_dir())
            .unwrap_or(&dir)
            .to_path_buf();
        Vault {
            root,
            index: OnceLock::new(),
        }
    }

    /// Whether `file` is inside this vault, so the vault can be kept when
    /// moving to it.
    pub fn holds(&self, file: &Path) -> bool {
        folder(file).starts_with(&self.root)
    }

    /// Where `link`, in the note at `from`, leads.
    pub fn follow(&self, link: &Link, from: &Path) -> Target {
        let dest = link.dest.trim();
        if !link.wiki && is_web(dest) {
            return Target::Web(dest.to_string());
        }
        let (name, heading) = match dest.split_once('#') {
            Some((name, heading)) => (name, Some(heading)),
            None => (dest, None),
        };
        // Decoded first, so `%5E` and `%23` count. A block reference, `#^id`,
        // has no heading to go to, and one after a heading, `Heading#^id`,
        // goes to the heading.
        let heading = heading
            .map(|h| {
                if link.wiki {
                    h.to_string()
                } else {
                    percent_decode(h)
                }
            })
            .map(|h| {
                h.split_once("#^")
                    .map_or(h.as_str(), |(head, _)| head)
                    .trim()
                    .to_string()
            })
            .filter(|h| !h.is_empty() && !h.starts_with('^'));
        if name.is_empty() {
            return match heading {
                Some(h) => Target::Heading(h),
                None => Target::Note {
                    path: from.to_path_buf(),
                    heading: None,
                },
            };
        }
        let found = if link.wiki {
            self.wiki(name, from)
        } else {
            self.relative(&percent_decode(name), from)
        };
        match found {
            Some(path) if is_markdown(&path) => Target::Note { path, heading },
            Some(path) => Target::Other(path),
            None => Target::Missing(name.to_string()),
        }
    }

    /// The file a wiki link names: a path from the vault's top, or a file
    /// name anywhere in it. A name with no extension is a note.
    fn wiki(&self, name: &str, from: &Path) -> Option<PathBuf> {
        let name = name.trim().trim_start_matches('/');
        let file = if has_extension(name) {
            name.to_string()
        } else {
            format!("{name}.md")
        };
        if file.contains('/') {
            let candidates = [self.root.join(&file), folder(from).join(&file)];
            if let Some(path) = candidates.into_iter().find(|p| p.is_file()) {
                return Some(path);
            }
        }
        // A path that is not there is matched by its end, as Obsidian does.
        let base = file.rsplit('/').next().unwrap_or(&file).to_lowercase();
        let suffix = format!("/{}", file.to_lowercase());
        let found = self.index().get(&base)?;
        let matching = found
            .iter()
            .filter(|p| !file.contains('/') || lower(p).ends_with(&suffix));
        nearest(matching, from)
    }

    /// The file a markdown link points at, relative to the linking note. A
    /// path with no extension may leave off `.md`.
    fn relative(&self, name: &str, from: &Path) -> Option<PathBuf> {
        let path = folder(from).join(name);
        if path.is_file() {
            return Some(path);
        }
        let md = folder(from).join(format!("{name}.md"));
        (!has_extension(name) && md.is_file()).then_some(md)
    }

    fn index(&self) -> &HashMap<String, Vec<PathBuf>> {
        self.index.get_or_init(|| {
            let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();
            let mut dirs = vec![self.root.clone()];
            let mut seen = 0;
            while let Some(dir) = dirs.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    if kind.is_dir() {
                        if !SKIP.contains(&name.as_str()) {
                            dirs.push(entry.path());
                        }
                    } else if seen < MAX_FILES {
                        seen += 1;
                        index
                            .entry(name.to_lowercase())
                            .or_default()
                            .push(entry.path());
                    }
                }
            }
            index
        })
    }

    /// Point every wiki-embedded image at its file in the vault, so it loads
    /// like any other image. One that cannot be found is left as it is.
    pub fn resolve_embeds(&self, blocks: &mut [Block], from: &Path) {
        for block in blocks {
            match &mut block.kind {
                BlockKind::Image { src, wiki, .. } if *wiki => {
                    if let Some(path) = self.wiki(src, from) {
                        *src = path.to_string_lossy().to_string();
                        *wiki = false;
                    }
                }
                BlockKind::Quote(inner) => self.resolve_embeds(inner, from),
                BlockKind::List { items, .. } => {
                    for item in items {
                        self.resolve_embeds(&mut item.blocks, from);
                    }
                }
                _ => {}
            }
        }
    }
}

/// The folder `file` is in.
fn folder(file: &Path) -> PathBuf {
    match file.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn lower(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// Of `paths`, the one in the folder nearest `from`: the one sharing the
/// most folders with it, then the shortest.
fn nearest<'a>(paths: impl Iterator<Item = &'a PathBuf>, from: &Path) -> Option<PathBuf> {
    let here = folder(from);
    paths
        .max_by_key(|p| {
            let shared = p
                .components()
                .zip(here.components())
                .take_while(|(a, b)| a == b)
                .count();
            (shared, std::cmp::Reverse(p.components().count()))
        })
        .cloned()
}

/// Whether `dest` is an address with a scheme, `https://`, `obsidian://` or
/// `mailto:`, rather than a path.
fn is_web(dest: &str) -> bool {
    let scheme = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphabetic());
    dest.to_ascii_lowercase().starts_with("mailto:")
        || dest.split_once("://").is_some_and(|(s, _)| scheme(s))
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// Whether the last part of `name` has a file extension. Note names may have
/// dots in them, so only a known kind of file counts.
fn has_extension(name: &str) -> bool {
    let last = name.rsplit('/').next().unwrap_or(name);
    let ext = last.rsplit_once('.').map_or("", |(_, ext)| ext);
    is_image(last)
        || [
            "md", "markdown", "pdf", "canvas", "txt", "csv", "json", "html", "mp3", "mp4",
        ]
        .iter()
        .any(|e| ext.eq_ignore_ascii_case(e))
}

/// `%20` and the like decoded, for a markdown link's path. Bad escapes are
/// kept as written.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Whether heading text matches a link's `#heading`. Case, spaces and
/// punctuation are ignored, so `#my-heading` finds "My heading".
pub fn heading_matches(text: &str, wanted: &str) -> bool {
    let key = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let wanted = key(wanted);
    !wanted.is_empty() && key(text) == wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vault in a temporary folder, with `files` written empty.
    struct TempVault(PathBuf);

    impl TempVault {
        fn new(name: &str, files: &[&str]) -> TempVault {
            let root =
                std::env::temp_dir().join(format!("smd-vault-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(".obsidian")).unwrap();
            for file in files {
                let path = root.join(file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "").unwrap();
            }
            TempVault(root)
        }

        fn path(&self, file: &str) -> PathBuf {
            self.0.join(file)
        }
    }

    impl Drop for TempVault {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wiki(dest: &str) -> Link {
        Link {
            dest: dest.into(),
            wiki: true,
        }
    }

    fn note(path: PathBuf, heading: Option<&str>) -> Target {
        Target::Note {
            path,
            heading: heading.map(str::to_string),
        }
    }

    #[test]
    fn the_vault_is_the_folder_with_obsidian_in_it() {
        let v = TempVault::new("root", &["HOME.md", "a/b/deep.md"]);
        let vault = Vault::for_file(&v.path("a/b/deep.md"));
        assert_eq!(vault.root, v.0);
        assert!(vault.holds(&v.path("HOME.md")));
        assert!(!vault.holds(Path::new("/elsewhere/x.md")));
    }

    #[test]
    fn wiki_links_find_notes_by_name_or_path() {
        let v = TempVault::new(
            "names",
            &[
                "HOME.md",
                "work/AFC/README.md",
                "personal/README.md",
                "work/Plan.md",
            ],
        );
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        // By name, ignoring case.
        assert_eq!(
            vault.follow(&wiki("plan"), &home),
            note(v.path("work/Plan.md"), None)
        );
        // By path from the top, and by the end of a path.
        assert_eq!(
            vault.follow(&wiki("work/AFC/README"), &home),
            note(v.path("work/AFC/README.md"), None)
        );
        assert_eq!(
            vault.follow(&wiki("AFC/README"), &home),
            note(v.path("work/AFC/README.md"), None)
        );
        // A shared name goes to the one nearest the linking note.
        assert_eq!(
            vault.follow(&wiki("README"), &v.path("personal/x.md")),
            note(v.path("personal/README.md"), None)
        );
        assert_eq!(
            vault.follow(&wiki("Nope"), &home),
            Target::Missing("Nope".into())
        );
    }

    #[test]
    fn headings_and_block_references() {
        let v = TempVault::new("headings", &["HOME.md", "Plan.md"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        assert_eq!(
            vault.follow(&wiki("Plan#Next steps"), &home),
            note(v.path("Plan.md"), Some("Next steps"))
        );
        assert_eq!(
            vault.follow(&wiki("#Tasks"), &home),
            Target::Heading("Tasks".into())
        );
        assert_eq!(
            vault.follow(&wiki("Plan#^abc123"), &home),
            note(v.path("Plan.md"), None)
        );
        // A backslash ending a name is kept: the parser has already dropped
        // the one that escapes an alias's pipe.
        assert_eq!(
            vault.follow(&wiki("Nope\\"), &home),
            Target::Missing("Nope\\".into())
        );
        // Spaces around the name and before the heading are dropped.
        assert_eq!(
            vault.follow(&wiki(" Plan #Next steps "), &home),
            note(v.path("Plan.md"), Some("Next steps"))
        );
        assert!(heading_matches("✅ Next steps!", "next-steps"));
        assert!(!heading_matches("Next", "next-steps"));
    }

    #[test]
    fn markdown_links_are_relative_and_web_links_go_out() {
        let v = TempVault::new("relative", &["a/HOME.md", "a/My Note.md", "a/doc.pdf"]);
        let vault = Vault::for_file(&v.path("a/HOME.md"));
        let home = v.path("a/HOME.md");
        assert_eq!(
            vault.follow(&"My%20Note.md#Part%20two".into(), &home),
            note(v.path("a/My Note.md"), Some("Part two"))
        );
        assert_eq!(
            vault.follow(&"My%20Note".into(), &home),
            note(v.path("a/My Note.md"), None)
        );
        assert_eq!(
            vault.follow(&"doc.pdf".into(), &home),
            Target::Other(v.path("a/doc.pdf"))
        );
        assert_eq!(
            vault.follow(&"https://x.org/a#b".into(), &home),
            Target::Web("https://x.org/a#b".into())
        );
    }

    #[test]
    fn embedded_images_are_found_in_the_vault() {
        let v = TempVault::new("embeds", &["HOME.md", "attachments/pic.png"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        let mut doc = crate::doc::parse("![[pic.png|300]]\n\n- ![[gone.png]]\n");
        vault.resolve_embeds(&mut doc.blocks, &v.path("HOME.md"));
        let BlockKind::Image { src, wiki, .. } = &doc.blocks[0].kind else {
            panic!("not an image: {:?}", doc.blocks[0]);
        };
        assert_eq!(Path::new(src), v.path("attachments/pic.png"));
        assert!(!wiki);
        // Spaces around the name and a fragment after it are dropped.
        let mut doc = crate::doc::parse("![[ pic.png #small|300]]\n");
        vault.resolve_embeds(&mut doc.blocks, &v.path("HOME.md"));
        let BlockKind::Image { src, .. } = &doc.blocks[0].kind else {
            panic!("not an image: {:?}", doc.blocks[0]);
        };
        assert_eq!(Path::new(src), v.path("attachments/pic.png"));
    }

    fn md(dest: &str) -> Link {
        dest.into()
    }

    #[test]
    fn a_wiki_link_with_a_heading_and_a_block_reference() {
        let v = TempVault::new("wikihb", &["HOME.md", "Plan.md"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        // A block reference after a heading goes to the heading.
        assert_eq!(
            vault.follow(&wiki("Plan#Goals#^abc"), &home),
            note(v.path("Plan.md"), Some("Goals"))
        );
        assert_eq!(
            vault.follow(&md("Plan.md#Goals#^abc"), &home),
            note(v.path("Plan.md"), Some("Goals"))
        );
        assert_eq!(
            vault.follow(&wiki("#Goals#^abc"), &home),
            Target::Heading("Goals".into())
        );
        // With no heading before it, the whole link is a block reference.
        assert_eq!(
            vault.follow(&wiki("Plan##^abc"), &home),
            note(v.path("Plan.md"), None)
        );
        // An empty heading is no heading.
        assert_eq!(
            vault.follow(&wiki("Plan#"), &home),
            note(v.path("Plan.md"), None)
        );
        // A block reference alone in the open note goes to the note itself.
        assert_eq!(
            vault.follow(&wiki("#^abc"), &home),
            note(home.clone(), None)
        );
        assert_eq!(vault.follow(&wiki("#"), &home), note(home.clone(), None));
        // Wiki headings are not percent-decoded.
        assert_eq!(
            vault.follow(&wiki("#100%25"), &home),
            Target::Heading("100%25".into())
        );
    }

    #[test]
    fn block_references_and_spaces_are_found_after_decoding() {
        let v = TempVault::new("decodeorder", &["HOME.md", "Plan.md"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        let plan = v.path("Plan.md");
        // An encoded `^` still makes a block reference.
        assert_eq!(
            vault.follow(&md("Plan.md#%5Eabc"), &home),
            note(plan.clone(), None)
        );
        // An encoded `#^` after a heading is cut like a plain one.
        assert_eq!(
            vault.follow(&md("Plan.md#Goals%23%5Eabc"), &home),
            note(plan.clone(), Some("Goals"))
        );
        assert_eq!(
            vault.follow(&md("#Goals%23%5Eabc"), &home),
            Target::Heading("Goals".into())
        );
        // Space before the block reference is not part of the heading.
        assert_eq!(
            vault.follow(&wiki("Plan#Goals #^abc"), &home),
            note(plan.clone(), Some("Goals"))
        );
        assert_eq!(
            vault.follow(&md("Plan.md#Goals%20#^abc"), &home),
            note(plan, Some("Goals"))
        );
    }

    #[test]
    fn a_markdown_link_to_a_heading_in_the_open_note() {
        let v = TempVault::new("mdheading", &["HOME.md"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        assert_eq!(
            vault.follow(&md("#Part%20two"), &home),
            Target::Heading("Part two".into())
        );
        assert_eq!(vault.follow(&md("#^blk"), &home), note(home.clone(), None));
        assert_eq!(vault.follow(&md("  "), &home), note(home, None));
    }

    #[test]
    fn web_and_mail_links_are_not_looked_up() {
        let v = TempVault::new("web", &["HOME.md"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        for dest in [
            "http://example.com",
            "https://example.com/a%20b#frag",
            "obsidian://open?vault=x",
            "mailto:me@example.com",
            "MAILTO:me@example.com",
            "HTTPS://Example.com",
        ] {
            assert_eq!(vault.follow(&md(dest), &home), Target::Web(dest.into()));
        }
        // Surrounding space is trimmed.
        assert_eq!(
            vault.follow(&md("  https://x.org  "), &home),
            Target::Web("https://x.org".into())
        );
        // A wiki link is always a note name, even one that looks like an address.
        assert_eq!(
            vault.follow(&wiki("https://x.org"), &home),
            Target::Missing("https://x.org".into())
        );
        // A scheme must be letters only, so this is a path.
        assert_eq!(
            vault.follow(&md("a1b://x"), &home),
            Target::Missing("a1b://x".into())
        );
        assert_eq!(
            vault.follow(&md("://x"), &home),
            Target::Missing("://x".into())
        );
    }

    #[test]
    fn links_to_files_that_are_not_notes() {
        let v = TempVault::new(
            "other",
            &[
                "HOME.md",
                "docs/spec.pdf",
                "img/pic.png",
                "Old.markdown",
                "data.csv",
            ],
        );
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        assert_eq!(
            vault.follow(&wiki("spec.pdf"), &home),
            Target::Other(v.path("docs/spec.pdf"))
        );
        assert_eq!(
            vault.follow(&wiki("pic.png"), &home),
            Target::Other(v.path("img/pic.png"))
        );
        assert_eq!(
            vault.follow(&wiki("data.csv#x"), &home),
            Target::Other(v.path("data.csv"))
        );
        assert_eq!(
            vault.follow(&md("docs/spec.pdf"), &home),
            Target::Other(v.path("docs/spec.pdf"))
        );
        assert_eq!(
            vault.follow(&md("img/pic.png"), &home),
            Target::Other(v.path("img/pic.png"))
        );
        // `.markdown` is a note too.
        assert_eq!(
            vault.follow(&md("Old.markdown"), &home),
            note(v.path("Old.markdown"), None)
        );
        assert_eq!(
            vault.follow(&wiki("Old.markdown"), &home),
            note(v.path("Old.markdown"), None)
        );
    }

    #[test]
    fn missing_notes_report_what_was_looked_for() {
        let v = TempVault::new("missing", &["HOME.md", "docs/a.md"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        assert_eq!(
            vault.follow(&md("nope.md#Head"), &home),
            Target::Missing("nope.md".into())
        );
        // The name is shown as written, not decoded.
        assert_eq!(
            vault.follow(&md("no%20pe"), &home),
            Target::Missing("no%20pe".into())
        );
        assert_eq!(
            vault.follow(&wiki("docs/zzz"), &home),
            Target::Missing("docs/zzz".into())
        );
        // A path that exists only by its end in another folder is not a match.
        assert_eq!(
            vault.follow(&wiki("other/a"), &home),
            Target::Missing("other/a".into())
        );
        // A markdown link is relative to the note, not looked up by name.
        assert_eq!(
            vault.follow(&md("a.md"), &home),
            Target::Missing("a.md".into())
        );
        // A dotted name with no known extension gets `.md` added.
        assert_eq!(
            vault.follow(&wiki("v1.2 notes"), &home),
            Target::Missing("v1.2 notes".into())
        );
        // A directory is not a note.
        assert_eq!(
            vault.follow(&md("docs"), &home),
            Target::Missing("docs".into())
        );
    }

    #[test]
    fn markdown_links_go_up_folders_and_decode_escapes() {
        let v = TempVault::new(
            "up",
            &["a/b/HOME.md", "a/Top.md", "a/b/caf\u{e9}.md", "a/b/100%.md"],
        );
        let vault = Vault::for_file(&v.path("a/b/HOME.md"));
        let home = v.path("a/b/HOME.md");
        assert_eq!(
            vault.follow(&md("../Top"), &home),
            note(v.path("a/b/../Top.md"), None)
        );
        assert_eq!(
            vault.follow(&md("caf%C3%A9.md"), &home),
            note(v.path("a/b/caf\u{e9}.md"), None)
        );
        // Bad escapes are kept as written.
        assert_eq!(
            vault.follow(&md("100%.md"), &home),
            note(v.path("a/b/100%.md"), None)
        );
        assert_eq!(
            vault.follow(&md("100%zz.md"), &home),
            Target::Missing("100%zz.md".into())
        );
        // Escapes that are not UTF-8 are kept as written.
        assert_eq!(percent_decode("%ff%fe"), "%ff%fe");
        assert_eq!(percent_decode("a%2"), "a%2");
        // A sign is not a hex digit, though `from_str_radix` takes one.
        assert_eq!(percent_decode("a%+1b"), "a%+1b");
        assert_eq!(percent_decode("%-1"), "%-1");
        assert_eq!(percent_decode("%"), "%");
    }

    #[test]
    fn a_name_shared_by_notes_goes_to_the_nearest_folder() {
        let v = TempVault::new(
            "clash",
            &[
                "Idea.md",
                "work/Idea.md",
                "work/team/Idea.md",
                "work/team/sub/Note.md",
                "work/Other.md",
                "play/Idea.md",
            ],
        );
        let vault = Vault::for_file(&v.path("Idea.md"));
        // Sharing the most folders wins.
        assert_eq!(
            vault.follow(&wiki("Idea"), &v.path("work/team/sub/Note.md")),
            note(v.path("work/team/Idea.md"), None)
        );
        assert_eq!(
            vault.follow(&wiki("Idea"), &v.path("work/Other.md")),
            note(v.path("work/Idea.md"), None)
        );
        assert_eq!(
            vault.follow(&wiki("idea"), &v.path("play/x.md")),
            note(v.path("play/Idea.md"), None)
        );
        // From the top, the shortest path wins.
        assert_eq!(
            vault.follow(&wiki("Idea"), &v.path("Elsewhere.md")),
            note(v.path("Idea.md"), None)
        );
        // A path that is only the end of others picks by the same rule.
        assert_eq!(
            vault.follow(&wiki("team/Idea"), &v.path("play/x.md")),
            note(v.path("work/team/Idea.md"), None)
        );
        assert_eq!(
            vault.follow(&wiki("/work/Idea"), &v.path("play/x.md")),
            note(v.path("work/Idea.md"), None)
        );
    }

    #[test]
    fn a_path_is_tried_from_the_vault_top_then_the_linking_folder() {
        let v = TempVault::new(
            "paths",
            &["HOME.md", "sub/HOME2.md", "sub/x/Page.md", "x/Page.md"],
        );
        let vault = Vault::for_file(&v.path("HOME.md"));
        // From the top first.
        assert_eq!(
            vault.follow(&wiki("x/Page"), &v.path("sub/HOME2.md")),
            note(v.path("x/Page.md"), None)
        );
        // With `.md` written out it is found the same way.
        assert_eq!(
            vault.follow(&wiki("x/Page.md"), &v.path("sub/HOME2.md")),
            note(v.path("x/Page.md"), None)
        );
        // Names are trimmed.
        assert_eq!(
            vault.follow(&wiki(" x/Page "), &v.path("sub/HOME2.md")),
            note(v.path("x/Page.md"), None)
        );
    }

    #[test]
    fn a_path_relative_to_the_linking_folder_is_found_when_not_at_the_top() {
        let v = TempVault::new("rel", &["HOME.md", "sub/x/Page.md"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        assert_eq!(
            vault.follow(&wiki("x/Page"), &v.path("sub/y.md")),
            note(v.path("sub/x/Page.md"), None)
        );
    }

    #[test]
    fn skipped_folders_are_not_indexed() {
        let v = TempVault::new(
            "skip",
            &[
                "HOME.md",
                ".trash/Gone.md",
                ".git/Hidden.md",
                "node_modules/Dep.md",
                "target/Out.md",
                "kept/Kept.md",
            ],
        );
        let vault = Vault::for_file(&v.path("HOME.md"));
        let home = v.path("HOME.md");
        for name in ["Gone", "Hidden", "Dep", "Out"] {
            assert_eq!(
                vault.follow(&wiki(name), &home),
                Target::Missing(name.into())
            );
        }
        assert_eq!(
            vault.follow(&wiki("Kept"), &home),
            note(v.path("kept/Kept.md"), None)
        );
    }

    #[test]
    fn a_note_outside_a_vault_looks_in_its_own_folder_and_below() {
        let root = std::env::temp_dir().join(format!("smd-vault-novault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Removes the folder on drop, even when an assertion fails.
        let _guard = TempVault(root.clone());
        std::fs::create_dir_all(root.join("notes/deep")).unwrap();
        std::fs::write(root.join("notes/a.md"), "").unwrap();
        std::fs::write(root.join("notes/deep/b.md"), "").unwrap();
        std::fs::write(root.join("above.md"), "").unwrap();
        let from = root.join("notes/a.md");
        let vault = Vault::for_file(&from);
        assert!(!root.join("notes/.obsidian").exists());
        assert!(vault.root.starts_with(&root));
        assert_eq!(vault.root, root.join("notes"));
        assert!(vault.holds(&root.join("notes/deep/b.md")));
        assert!(!vault.holds(&root.join("above.md")));
        assert_eq!(
            vault.follow(&wiki("b"), &from),
            note(root.join("notes/deep/b.md"), None)
        );
        assert_eq!(
            vault.follow(&wiki("above"), &from),
            Target::Missing("above".into())
        );
        // A markdown link can still reach above, relative to the note.
        assert_eq!(
            vault.follow(&md("../above.md"), &from),
            note(root.join("notes/../above.md"), None)
        );
    }

    #[test]
    fn a_bare_file_name_uses_the_current_folder() {
        let vault = Vault::for_file(Path::new("x.md"));
        assert!(vault.holds(Path::new("y.md")));
        assert_eq!(folder(Path::new("x.md")), PathBuf::from("."));
        assert_eq!(folder(Path::new("/")), PathBuf::from("."));
    }

    #[test]
    fn extensions_that_count_as_files() {
        assert!(has_extension("a.md"));
        assert!(has_extension("dir.v2/a.PDF"));
        assert!(has_extension("pic.JPG"));
        assert!(!has_extension("v1.2 notes"));
        assert!(!has_extension("dir.pdf/note"));
        assert!(!has_extension("plain"));
        assert!(is_markdown(Path::new("a.MD")));
        assert!(is_markdown(Path::new("a.Markdown")));
        assert!(!is_markdown(Path::new("a.txt")));
        assert!(!is_markdown(Path::new("md")));
    }

    #[test]
    fn heading_matches_ignores_case_spaces_and_punctuation() {
        assert!(heading_matches("My Heading", "my-heading"));
        assert!(heading_matches("Café au lait", "CAFÉ AU LAIT"));
        assert!(heading_matches("1. Intro", "1 intro"));
        assert!(!heading_matches("Anything", ""));
        assert!(!heading_matches("Anything", "---"));
        assert!(!heading_matches("", ""));
        assert!(!heading_matches("Intro two", "intro"));
    }

    #[test]
    fn embeds_inside_quotes_and_lists_are_resolved() {
        let v = TempVault::new("nested", &["HOME.md", "pics/a.png", "pics/b.png"]);
        let vault = Vault::for_file(&v.path("HOME.md"));
        let mut doc = crate::doc::parse("> ![[a.png]]\n\n- ![[b.png]]\n\n![](web.png)\n");
        vault.resolve_embeds(&mut doc.blocks, &v.path("HOME.md"));
        let srcs = srcs(&doc.blocks);
        assert_eq!(
            srcs,
            [
                (v.path("pics/a.png").to_string_lossy().to_string(), false),
                (v.path("pics/b.png").to_string_lossy().to_string(), false),
                ("web.png".to_string(), false),
            ]
        );
    }

    fn srcs(blocks: &[Block]) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        for block in blocks {
            match &block.kind {
                BlockKind::Image { src, wiki, .. } => out.push((src.clone(), *wiki)),
                BlockKind::Quote(inner) => out.extend(srcs(inner)),
                BlockKind::List { items, .. } => {
                    for item in items {
                        out.extend(srcs(&item.blocks));
                    }
                }
                _ => {}
            }
        }
        out
    }
}
