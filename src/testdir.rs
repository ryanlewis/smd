//! A folder for a test to write files in, removed when the test ends, even
//! when an assertion fails. A test run killed outright, such as by SIGKILL,
//! cannot run `Drop`, so its folder stays behind.

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// A fresh, empty folder under the system temp folder, removed on drop.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    /// The folder for the test named `name`, which must be unique among the
    /// tests that run at once. Anything left there by an earlier run is
    /// removed first; when it cannot be, this panics rather than hand the
    /// test a folder that is not empty.
    pub fn new(name: &str) -> TempDir {
        // The process id first: it has no "-", so no two names and ids
        // give the same folder.
        let dir = std::env::temp_dir().join(format!("smd-{}-{name}", std::process::id()));
        match std::fs::remove_dir_all(&dir) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                panic!("cannot empty {}: {err}", dir.display())
            }
            _ => {}
        }
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A vault in a fresh folder: an `.obsidian` folder and some notes.
pub struct TempVault(pub TempDir);

impl TempVault {
    /// A vault holding `files`, each a name and its text, in the folder for
    /// `name` (see [`TempDir::new`]).
    pub fn new(name: &str, files: &[(&str, &str)]) -> TempVault {
        let root = TempDir::new(name);
        std::fs::create_dir_all(root.join(".obsidian")).unwrap();
        for (file, text) in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        TempVault(root)
    }

    /// A vault holding `files`, each written empty.
    pub fn empty(name: &str, files: &[&str]) -> TempVault {
        let files: Vec<(&str, &str)> = files.iter().map(|&f| (f, "")).collect();
        TempVault::new(name, &files)
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_is_removed_when_the_test_using_it_panics() {
        let path = std::panic::catch_unwind(|| {
            let dir = TempDir::new("testdir-panic");
            std::fs::create_dir_all(dir.join("deep")).unwrap();
            std::fs::write(dir.join("deep/a.md"), "a\n").unwrap();
            assert!(dir.join("deep/a.md").is_file());
            std::panic::panic_any(dir.to_path_buf());
        })
        .unwrap_err()
        .downcast::<PathBuf>()
        .unwrap();
        assert!(!path.exists(), "{path:?} is still there");
    }

    #[test]
    fn a_new_folder_starts_empty() {
        let first = TempDir::new("testdir-fresh");
        std::fs::write(first.join("left.md"), "").unwrap();
        // As if an earlier run was killed before it could clean up.
        std::mem::forget(first);
        let dir = TempDir::new("testdir-fresh");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_leftover_that_cannot_be_emptied_is_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let first = TempDir::new("testdir-stuck");
        let stuck = first.join("stuck");
        std::fs::create_dir(&stuck).unwrap();
        std::fs::write(stuck.join("a.md"), "").unwrap();
        let mode = |mode| std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(mode));
        mode(0o555).unwrap();
        // Root, or a file system that ignores the mode, can change it anyway.
        if std::fs::write(stuck.join("b.md"), "").is_ok() {
            mode(0o755).unwrap();
            return;
        }
        let path = first.to_path_buf();
        std::mem::forget(first);
        let err = std::panic::catch_unwind(|| TempDir::new("testdir-stuck")).unwrap_err();
        mode(0o755).unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        let said = err.downcast::<String>().unwrap();
        assert!(
            said.starts_with(&format!("cannot empty {}: ", path.display())),
            "{said}"
        );
    }
}
