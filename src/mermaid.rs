//! Mermaid diagrams, drawn to PNG by mermaid-cli (`mmdc`) and kept in a disk
//! cache so a file opens fast the second time.
//!
//! Diagrams are drawn at a fixed scale, not the pane width, so the cache and
//! the image key stay the same on resize; [`crate::images::fit`] shrinks them.

use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::images::{Job, Png};
use crate::theme::Theme;

const PROGRAM: &str = "mmdc";

/// Puppeteer's scale factor: two pixels per CSS pixel keeps text sharp.
const SCALE: &str = "2";

/// Part of every cache name. Change it when the way diagrams are drawn
/// changes, so old files are not used.
const VERSION: &str = "1";

/// How long one diagram may take before `mmdc` is stopped.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Each `mmdc` starts a browser, so only a few run at once.
const MAX_RUNNING: usize = 4;

/// A name for the diagram drawn from `source` for `theme`, used both as the
/// image key and as the cache file name.
pub fn name(source: &str, theme: Theme) -> String {
    let mut hash = Fnv::new();
    for part in [VERSION, theme.name(), SCALE, source] {
        hash.write(part.as_bytes());
        hash.write(&[0]);
    }
    format!("{}-{:016x}", theme.name(), hash.0)
}

/// Draw `source` for `theme`, from the cache when it is there.
pub fn job(source: String, theme: Theme) -> Job {
    let cache = cache_dir().map(|dir| dir.join(format!("{}.png", name(&source, theme))));
    Box::new(move || draw(PROGRAM, &source, theme, cache.as_deref()))
}

fn draw(program: &str, source: &str, theme: Theme, cache: Option<&Path>) -> Result<Png, String> {
    if let Some(png) = cache.and_then(|path| Png::new(std::fs::read(path).ok()?).ok()) {
        return Ok(png);
    }
    let data = {
        let _slot = Slot::take();
        run(program, source, theme, TIMEOUT)?
    };
    let png = Png::new(data.clone()).map_err(|e| format!("mermaid: mmdc gave {e}"))?;
    if let Some(path) = cache {
        // A diagram that cannot be cached is still shown.
        let _ = store(path, &data);
    }
    Ok(png)
}

/// Run `mmdc`, reading the diagram from stdin and the PNG from stdout. It is
/// stopped, with the browser it started, after `timeout`.
fn run(program: &str, source: &str, theme: Theme, timeout: Duration) -> Result<Vec<u8>, String> {
    let mmdc_theme = match theme {
        Theme::Dark => "dark",
        Theme::Light => "default",
    };
    let mut command = Command::new(program);
    command
        .args(["-i", "-", "-o", "-", "-e", "png", "-q"])
        .args(["-t", mmdc_theme, "-b", "transparent", "-s", SCALE])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a timeout can stop the browser it starts too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().map_err(|e| match e.kind() {
        ErrorKind::NotFound => "mermaid: mmdc not found on PATH".to_string(),
        _ => format!("mermaid: cannot run mmdc: {e}"),
    })?;
    // Both pipes are read on their own threads so neither can fill and stall
    // mmdc while the other is read.
    let reader = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            data
        })
    };
    let stdout = reader(Box::new(child.stdout.take().expect("piped")));
    let stderr = reader(Box::new(child.stderr.take().expect("piped")));
    // Written on its own thread too, so a mmdc that never reads its input
    // still runs into the timeout below.
    if let Some(mut stdin) = child.stdin.take() {
        let source = source.to_owned();
        std::thread::spawn(move || {
            let _ = stdin.write_all(source.as_bytes());
        });
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                kill_group(&mut child);
                let _ = child.wait();
                return Err(format!("mermaid: mmdc took over {}s", timeout.as_secs()));
            }
            Err(e) => return Err(format!("mermaid: mmdc: {e}")),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "mermaid: {}",
            error_line(&String::from_utf8_lossy(&stderr))
                .unwrap_or_else(|| format!("mmdc failed ({status})"))
        ));
    }
    Ok(stdout)
}

/// Kill `child` and everything in its process group.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill(2) takes plain integers and has no memory effects.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = child.kill();
}

/// The line of `mmdc`'s output that says what went wrong, such as
/// `Parse error on line 2`.
fn error_line(stderr: &str) -> Option<String> {
    let lines = stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = lines
        .clone()
        .find_map(|l| l.strip_prefix("Error:"))
        .or_else(|| lines.clone().next())?;
    let line = line.trim().trim_end_matches(':');
    (!line.is_empty()).then(|| line.to_string())
}

/// Write the PNG under a temporary name, then move it into place, so a
/// reader never sees half a file. The name is unique to this write, as two
/// jobs for the same diagram can finish together.
fn store(path: &Path, data: &[u8]) -> std::io::Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("png.{}.{n}.tmp", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn cache_dir() -> Option<PathBuf> {
    let root = cache_root(
        cfg!(target_os = "macos"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_CACHE_HOME"),
    )?;
    Some(root.join("smd").join("mermaid"))
}

/// The platform cache directory: `~/Library/Caches` on macOS, elsewhere
/// `$XDG_CACHE_HOME` or `~/.cache`. Relative paths are ignored.
fn cache_root(macos: bool, home: Option<OsString>, xdg: Option<OsString>) -> Option<PathBuf> {
    let absolute = |p: Option<OsString>| p.map(PathBuf::from).filter(|p| p.is_absolute());
    let home = absolute(home);
    if macos {
        return home.map(|h| h.join("Library").join("Caches"));
    }
    absolute(xdg).or_else(|| home.map(|h| h.join(".cache")))
}

/// A place among the [`MAX_RUNNING`] renders, given back when dropped.
struct Slot;

static RUNNING: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

impl Slot {
    fn take() -> Slot {
        let (lock, freed) = &RUNNING;
        let mut running = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *running >= MAX_RUNNING {
            running = freed.wait(running).unwrap_or_else(|e| e.into_inner());
        }
        *running += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (lock, freed) = &RUNNING;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        freed.notify_one();
    }
}

/// 64-bit FNV-1a, which unlike the standard hasher is the same in every
/// build, so cache names stay valid across upgrades.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::tests::png;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("smd-mermaid-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_depend_on_source_and_theme_only() {
        let a = name("graph LR\n  A-->B\n", Theme::Dark);
        assert_eq!(a, name("graph LR\n  A-->B\n", Theme::Dark));
        assert_ne!(a, name("graph LR\n  A-->C\n", Theme::Dark));
        assert_ne!(a, name("graph LR\n  A-->B\n", Theme::Light));
        assert!(a.starts_with("dark-") && a.len() == "dark-".len() + 16);
        // Fixed, so files cached by one build are found by the next.
        let mut h = Fnv::new();
        h.write(b"a");
        assert_eq!(h.0, 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn cache_lives_in_the_platform_cache_directory() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            cache_root(true, os("/Users/u"), os("/x")),
            Some(PathBuf::from("/Users/u/Library/Caches"))
        );
        assert_eq!(
            cache_root(false, os("/home/u"), os("/xdg")),
            Some(PathBuf::from("/xdg"))
        );
        assert_eq!(
            cache_root(false, os("/home/u"), os("rel")),
            Some(PathBuf::from("/home/u/.cache"))
        );
        assert_eq!(
            cache_root(false, os("/home/u"), None),
            Some(PathBuf::from("/home/u/.cache"))
        );
        assert_eq!(cache_root(false, None, None), None);
    }

    #[test]
    fn a_cached_diagram_is_used_without_running_mmdc() {
        let dir = scratch("hit");
        let path = dir.join("d.png");
        std::fs::write(&path, png(30, 10)).unwrap();
        let got = draw(
            "smd-no-such-program",
            "graph LR\n",
            Theme::Dark,
            Some(&path),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Png::new(png(30, 10)));
    }

    #[test]
    fn missing_mmdc_is_an_error_and_nothing_is_cached() {
        let dir = scratch("missing");
        let path = dir.join("d.png");
        let got = draw(
            "smd-no-such-program",
            "graph LR\n",
            Theme::Dark,
            Some(&path),
        );
        let cached = path.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Err("mermaid: mmdc not found on PATH".into()));
        assert!(!cached);
    }

    #[cfg(unix)]
    #[test]
    fn a_drawn_diagram_is_cached_and_a_failed_one_gives_its_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("run");
        // A stand-in for mmdc: fails like a parse error on "bad", echoes
        // "text", and otherwise prints a PNG.
        std::fs::write(dir.join("out.png"), png(12, 34)).unwrap();
        let fake = dir.join("fake-mmdc");
        let script = format!(
            "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n  bad) printf '\\nError: Parse error on line 2:\\nmore\\n' >&2; exit 1;;\n  text) printf text;;\n  *) cat '{}';;\nesac\n",
            dir.join("out.png").display()
        );
        std::fs::write(&fake, script).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = fake.to_str().unwrap();
        let source = "graph LR\n  A-->B\n";
        let path = dir.join("cache").join("d.png");
        let got = draw(program, source, Theme::Light, Some(&path));
        let cached = std::fs::read(&path).ok();
        let bad = draw(program, "bad", Theme::Light, Some(&dir.join("bad.png")));
        let not_png = draw(program, "text", Theme::Light, None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Png::new(png(12, 34)));
        assert_eq!(cached, Some(png(12, 34)));
        assert_eq!(bad, Err("mermaid: Parse error on line 2".into()));
        assert_eq!(not_png, Err("mermaid: mmdc gave not a PNG".into()));
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_stops_the_whole_process_group() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("timeout");
        // A stand-in for mmdc that starts a child, as mmdc starts Chromium,
        // records its pid and sleeps.
        let fake = dir.join("fake-mmdc");
        let script = format!(
            "#!/bin/sh\nsleep 300 &\necho $! > '{}'\nwait\n",
            dir.join("child.pid").display()
        );
        std::fs::write(&fake, script).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let got = run(
            fake.to_str().unwrap(),
            "graph LR\n",
            Theme::Dark,
            Duration::from_secs(2),
        );
        let pid: libc::pid_t = std::fs::read_to_string(dir.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The kill is a signal; give the system a moment to reap the child.
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 only checks that the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !gone {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Err("mermaid: mmdc took over 2s".into()));
        assert!(gone, "the child outlived the timeout");
    }

    #[test]
    fn error_line_finds_the_message() {
        assert_eq!(
            error_line("\nError: Parse error on line 2:\nflowchart\n"),
            Some("Parse error on line 2".into())
        );
        assert_eq!(error_line("  boom  \n"), Some("boom".into()));
        assert_eq!(error_line("\n\n"), None);
    }
}
