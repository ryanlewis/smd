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
    root: PathBuf,
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
        // A block reference, `#^id`, has no heading to go to.
        let heading = heading
            .filter(|h| !h.is_empty() && !h.starts_with('^'))
            .map(|h| {
                if link.wiki {
                    h.to_string()
                } else {
                    percent_decode(h)
                }
            });
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
    }
}
