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
    /// removed first.
    pub fn new(name: &str) -> TempDir {
        // The process id first: it has no "-", so no two names and ids
        // give the same folder.
        let dir = std::env::temp_dir().join(format!("smd-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
}
