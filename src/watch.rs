//! Notices when the file on disk changes by checking its metadata.
//!
//! Checking by path, not by open file, means an editor that saves by writing a
//! new file and renaming it over the old one is seen like any other save.

use std::fs::Metadata;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

/// What changed since the last check.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// The file has new contents.
    Changed(String),
    /// The file is gone.
    Deleted,
    /// The file is there but cannot be read.
    Unreadable(String),
}

/// The parts of the metadata that change when a file is saved.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    ino: u64,
}

impl Stamp {
    fn of(meta: &Metadata) -> Self {
        Stamp {
            modified: meta.modified().ok(),
            len: meta.len(),
            #[cfg(unix)]
            ino: std::os::unix::fs::MetadataExt::ino(meta),
        }
    }
}

/// Last thing seen at the path.
#[derive(Debug, PartialEq)]
enum Seen {
    File(Stamp),
    Deleted,
    Unreadable,
}

pub struct Watcher {
    path: PathBuf,
    seen: Seen,
}

impl Watcher {
    /// Start watching `path`. Create the watcher before reading the file, so
    /// a save that lands in between is seen on the first check.
    pub fn new(path: PathBuf) -> Self {
        let seen = match std::fs::metadata(&path) {
            Ok(meta) => Seen::File(Stamp::of(&meta)),
            Err(_) => Seen::Unreadable,
        };
        Watcher { path, seen }
    }

    /// Look at the file again. Returns `None` when nothing has changed.
    pub fn check(&mut self) -> Option<Change> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return self.update(Seen::Deleted, Change::Deleted);
            }
            Err(err) => return self.update(Seen::Unreadable, Change::Unreadable(err.to_string())),
        };
        let stamp = Stamp::of(&meta);
        if self.seen == Seen::File(stamp) {
            return None;
        }
        // Record the stamp even if the read fails, so a file that stays
        // unreadable is reported once rather than on every check.
        self.seen = Seen::File(stamp);
        Some(match std::fs::read_to_string(&self.path) {
            Ok(source) => Change::Changed(source),
            Err(err) => Change::Unreadable(err.to_string()),
        })
    }

    fn update(&mut self, seen: Seen, change: Change) -> Option<Change> {
        if self.seen == seen {
            return None;
        }
        self.seen = seen;
        Some(change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh directory under the system temp dir, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("smd-watch-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nothing_changed() {
        let dir = Scratch::new("same");
        let path = dir.0.join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new(path);
        assert_eq!(w.check(), None);
    }

    #[test]
    fn append_is_seen() {
        let dir = Scratch::new("append");
        let path = dir.0.join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new(path.clone());
        fs::write(&path, "one\ntwo\n").unwrap();
        assert_eq!(w.check(), Some(Change::Changed("one\ntwo\n".into())));
        assert_eq!(w.check(), None);
    }

    #[test]
    fn rename_over_is_seen() {
        let dir = Scratch::new("rename");
        let path = dir.0.join("a.md");
        let tmp = dir.0.join("a.md.tmp");
        fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new(path.clone());
        // Same length, written straight away: only the inode (or the mtime)
        // tells the two files apart.
        fs::write(&tmp, "two\n").unwrap();
        fs::rename(&tmp, &path).unwrap();
        assert_eq!(w.check(), Some(Change::Changed("two\n".into())));
    }

    #[test]
    fn delete_is_reported_once_and_return_is_seen() {
        let dir = Scratch::new("delete");
        let path = dir.0.join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new(path.clone());
        fs::remove_file(&path).unwrap();
        assert_eq!(w.check(), Some(Change::Deleted));
        assert_eq!(w.check(), None);
        fs::write(&path, "back\n").unwrap();
        assert_eq!(w.check(), Some(Change::Changed("back\n".into())));
    }

    #[test]
    fn unreadable_contents_are_reported() {
        let dir = Scratch::new("utf8");
        let path = dir.0.join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new(path.clone());
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(w.check(), Some(Change::Unreadable(_))));
        assert_eq!(w.check(), None);
    }
}
