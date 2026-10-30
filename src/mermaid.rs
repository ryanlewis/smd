//! Mermaid diagrams, drawn to PNG and kept in a disk cache so a file opens
//! fast the second time. smd draws them itself with merman; mermaid-cli
//! (`mmdc`), which starts a headless browser, can be chosen instead.
//!
//! Diagrams are drawn at a fixed scale, not the pane width, so the cache and
//! the image key stay the same on resize; [`crate::images::fit`] shrinks them.

use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use merman::svg::export::{NativeFontOptions, RasterOptions};
use merman::{
    Engine, MermaidConfig, OperationControl, PngRequest, RenderOutput, RenderRequest,
    SvgEnvironment, SvgRequest,
};
use serde::Deserialize;

use crate::images::{Job, Png};
use crate::raster;
use crate::theme::Theme;

const PROGRAM: &str = "mmdc";

/// Two pixels per CSS pixel keeps text sharp.
const SCALE: u8 = 2;

/// Part of every cache name. Change it when the way diagrams are drawn
/// changes, so old files are not used.
const VERSION: &str = "2";

/// Default size cap of the disk cache.
const CACHE_CAP: u64 = 100 * 1024 * 1024;

/// How long one diagram may take before it is given up on.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Diagrams drawn at once. Each `mmdc` starts a browser, and merman keeps a
/// core busy.
const MAX_RUNNING: usize = 4;

/// What draws the diagrams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Renderer {
    /// merman, inside smd.
    #[default]
    Builtin,
    /// mermaid-cli, found on `PATH`.
    Mmdc,
}

impl Renderer {
    fn name(self) -> &'static str {
        match self {
            Renderer::Builtin => "builtin",
            Renderer::Mmdc => "mmdc",
        }
    }
}

static RENDERER: OnceLock<Renderer> = OnceLock::new();

/// Set what draws diagrams for the rest of the run. Only the first call counts.
pub fn set_renderer(renderer: Renderer) {
    let _ = RENDERER.set(renderer);
}

fn renderer() -> Renderer {
    RENDERER.get().copied().unwrap_or_default()
}

/// A name for the diagram drawn from `source` for `theme`, used both as the
/// image key and as the cache file name.
pub fn name(source: &str, theme: Theme) -> String {
    let mut hash = Fnv::new();
    let scale = SCALE.to_string();
    for part in [VERSION, renderer().name(), theme.name(), &scale, source] {
        hash.write(part.as_bytes());
        hash.write(&[0]);
    }
    format!("{}-{:016x}", theme.name(), hash.0)
}

/// Draw `source` for `theme`, from the cache when it is there.
pub fn job(source: String, theme: Theme) -> Job {
    let cache = cache_dir().map(|dir| dir.join(format!("{}.png", name(&source, theme))));
    Box::new(move || match renderer() {
        Renderer::Builtin => draw("merman", || merman(&source, theme), cache.as_deref()),
        Renderer::Mmdc => draw(PROGRAM, || mmdc(PROGRAM, &source, theme), cache.as_deref()),
    })
}

/// Use the cached PNG, or make one with `render` and cache it. `by` names
/// what drew it, for errors.
fn draw(
    by: &str,
    render: impl FnOnce() -> Result<Vec<u8>, String>,
    cache: Option<&Path>,
) -> Result<Png, String> {
    if let Some(png) = cache.and_then(|path| Png::new(std::fs::read(path).ok()?).ok()) {
        // Reading is a use: keep the file ahead of eviction.
        if let Some(path) = cache {
            touch(path);
        }
        return Ok(png);
    }
    let data = {
        let _slot = Slot::take();
        render()?
    };
    let png = Png::new(data.clone()).map_err(|e| format!("mermaid: {by} gave {e}"))?;
    if let Some(path) = cache {
        // A diagram that cannot be cached is still shown.
        if store(path, &data).is_ok()
            && let Some(dir) = path.parent()
        {
            evict(dir, cache_cap());
        }
    }
    Ok(png)
}

/// Draw `source` with merman, in mermaid's own theme for `theme`, on a clear
/// background.
fn merman(source: &str, theme: Theme) -> Result<Vec<u8>, String> {
    let config = MermaidConfig::from_value(serde_json::json!({ "theme": mermaid_theme(theme) }));
    let renderer = merman::Renderer::new().with_engine(Engine::new().with_site_config(config));
    let request = PngRequest {
        svg: SvgRequest {
            environment: environment().clone(),
            ..SvgRequest::default()
        },
        options: RasterOptions::default().with_scale(f32::from(SCALE)),
    };
    let control = OperationControl::new().with_deadline(TIMEOUT);
    match renderer.render(RenderRequest::png(source, control, request)) {
        Ok(RenderOutput::Png(Some(png))) => Ok(png.bytes),
        Ok(_) => Err("mermaid: not a diagram merman knows".to_string()),
        Err(e) => Err(format!("mermaid: {e}")),
    }
}

/// How merman measures text: in [`raster::sans_files`], read once. They are
/// named rather than searched for: merman's search reads every font on the
/// machine, and its platform lookup links CoreText, which the release's
/// cross-build cannot. With none, it estimates widths, and long labels may be
/// cut short.
fn environment() -> &'static SvgEnvironment {
    static ENVIRONMENT: OnceLock<SvgEnvironment> = OnceLock::new();
    ENVIRONMENT.get_or_init(|| {
        let estimated = SvgEnvironment::deterministic();
        let Some((regular, bold)) = raster::sans_files() else {
            return estimated;
        };
        let mut options = NativeFontOptions::default().with_font_file(regular);
        if let Some(bold) = bold {
            options = options.with_font_file(bold);
        }
        options
            .load(&OperationControl::new())
            .ok()
            .and_then(|fonts| estimated.clone().with_native_font_context(fonts).ok())
            .unwrap_or(estimated)
    })
}

/// Mermaid's name for the theme that suits `theme`.
fn mermaid_theme(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "dark",
        Theme::Light => "default",
    }
}

/// Draw `source` with `mmdc`, stopping it after [`TIMEOUT`].
fn mmdc(program: &str, source: &str, theme: Theme) -> Result<Vec<u8>, String> {
    run(program, source, theme, TIMEOUT)
}

/// Set the file's modified time to now, so it counts as recently used.
fn touch(path: &Path) {
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()));
}

/// The most the cache may hold, in bytes. `SMD_MERMAID_CACHE_MAX` overrides it.
fn cache_cap() -> u64 {
    std::env::var("SMD_MERMAID_CACHE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CACHE_CAP)
}

/// Delete the least recently used `.png` files in `dir` until the rest fit in
/// `cap` bytes. Temporary files are another write in progress and are left
/// alone. The jobs run off the UI thread, so this does too.
fn evict(dir: &Path, cap: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? != "png" {
                return None;
            }
            let meta = entry.metadata().ok()?;
            Some((meta.modified().ok()?, path, meta.len()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    files.sort();
    for (_, path, len) in files {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// Run `mmdc`, reading the diagram from stdin and the PNG from stdout. It is
/// stopped, with the browser it started, after `timeout`.
fn run(program: &str, source: &str, theme: Theme, timeout: Duration) -> Result<Vec<u8>, String> {
    let scale = SCALE.to_string();
    let mut command = Command::new(program);
    command
        .args(["-i", "-", "-o", "-", "-e", "png", "-q"])
        .args([
            "-t",
            mermaid_theme(theme),
            "-b",
            "transparent",
            "-s",
            &scale,
        ])
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

    /// Draw with `mmdc` as `program`, through the cache at `cache`.
    fn draw_mmdc(
        program: &str,
        source: &str,
        theme: Theme,
        cache: Option<&Path>,
    ) -> Result<Png, String> {
        draw(PROGRAM, || mmdc(program, source, theme), cache)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("smd-mermaid-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a script with file mode `mode` from a child process. Written
    /// from this one, the write handle could be inherited by a process
    /// another test is starting at that moment, and Linux refuses to run a
    /// file while any process has it open for writing ("Text file busy").
    #[cfg(unix)]
    fn write_script(path: &Path, text: &str, mode: &str) {
        let mut child = Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod \"$2\" \"$1\"", "sh"])
            .arg(path)
            .arg(mode)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(text.as_bytes()).unwrap();
        drop(stdin);
        assert!(child.wait().unwrap().success(), "{}", path.display());
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
    fn merman_draws_a_diagram_in_either_theme() {
        let source = "flowchart LR\n  A[Start] --> B[Done]\n";
        let light = merman(source, Theme::Light).unwrap();
        let dark = merman(source, Theme::Dark).unwrap();
        assert!(Png::new(light.clone()).is_ok());
        // Wider than tall, as a left-to-right flowchart is.
        let be = |at: usize| u32::from_be_bytes(light[at..at + 4].try_into().unwrap());
        assert!(be(16) > be(20));
        assert_ne!(light, dark);
    }

    #[test]
    fn merman_says_what_is_wrong_with_a_diagram() {
        let got = merman("sequenceDiagram\n  A->>: hi\n", Theme::Dark).unwrap_err();
        assert!(
            got.starts_with("mermaid: ") && got.contains("parse error"),
            "{got}"
        );
        let cache = scratch("merman-bad").join("bad.png");
        let got = draw(
            "merman",
            || merman("flowchart LR\n  A[a(b] --> B\n", Theme::Dark),
            Some(&cache),
        );
        assert!(got.is_err() && !cache.exists(), "{got:?}");
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
        let got = draw_mmdc(
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
        let got = draw_mmdc(
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
        let dir = scratch("run");
        // A stand-in for mmdc: fails like a parse error on "bad", echoes
        // "text", and otherwise prints a PNG.
        std::fs::write(dir.join("out.png"), png(12, 34)).unwrap();
        let fake = dir.join("fake-mmdc");
        let script = format!(
            "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n  bad) printf '\\nError: Parse error on line 2:\\nmore\\n' >&2; exit 1;;\n  text) printf text;;\n  *) cat '{}';;\nesac\n",
            dir.join("out.png").display()
        );
        write_script(&fake, &script, "755");
        let program = fake.to_str().unwrap();
        let source = "graph LR\n  A-->B\n";
        let path = dir.join("cache").join("d.png");
        let got = draw_mmdc(program, source, Theme::Light, Some(&path));
        let cached = std::fs::read(&path).ok();
        let bad = draw_mmdc(program, "bad", Theme::Light, Some(&dir.join("bad.png")));
        let not_png = draw_mmdc(program, "text", Theme::Light, None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Png::new(png(12, 34)));
        assert_eq!(cached, Some(png(12, 34)));
        assert_eq!(bad, Err("mermaid: Parse error on line 2".into()));
        assert_eq!(not_png, Err("mermaid: mmdc gave not a PNG".into()));
    }

    #[cfg(unix)]
    #[test]
    fn mmdc_that_cannot_start_or_fails_silently_says_so() {
        let dir = scratch("cannot");
        // Not executable, so it is found but cannot be run.
        let plain = dir.join("plain-mmdc");
        write_script(&plain, "#!/bin/sh\n", "644");
        // Fails with nothing on stderr.
        let quiet = dir.join("quiet-mmdc");
        write_script(&quiet, "#!/bin/sh\ncat >/dev/null\nexit 3\n", "755");
        let timeout = Duration::from_secs(10);
        let cannot = run(plain.to_str().unwrap(), "graph LR\n", Theme::Dark, timeout);
        let failed = run(quiet.to_str().unwrap(), "graph LR\n", Theme::Dark, timeout);
        let _ = std::fs::remove_dir_all(&dir);
        let cannot = cannot.unwrap_err();
        assert!(
            cannot.starts_with("mermaid: cannot run mmdc: ") && cannot.contains("ermission denied"),
            "{cannot}"
        );
        assert_eq!(failed, Err("mermaid: mmdc failed (exit status: 3)".into()));
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_stops_the_whole_process_group() {
        let dir = scratch("timeout");
        // A stand-in for mmdc that starts a child, as mmdc starts Chromium,
        // records its pid and sleeps.
        let fake = dir.join("fake-mmdc");
        let script = format!(
            "#!/bin/sh\nsleep 300 &\necho $! > '{}'\nwait\n",
            dir.join("child.pid").display()
        );
        write_script(&fake, &script, "755");
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

    /// Write `len` bytes to `dir/name` with a modified time `age` seconds ago.
    fn aged(dir: &Path, name: &str, len: usize, age: u64) {
        let path = dir.join(name);
        std::fs::write(&path, vec![0; len]).unwrap();
        let when = std::time::SystemTime::now() - Duration::from_secs(age);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn eviction_removes_the_oldest_files_first_until_under_the_cap() {
        let dir = scratch("evict");
        aged(&dir, "old.png", 10, 300);
        aged(&dir, "mid.png", 10, 200);
        aged(&dir, "new.png", 10, 100);
        evict(&dir, 25);
        assert_eq!(names(&dir), ["mid.png", "new.png"]);
        evict(&dir, 10);
        assert_eq!(names(&dir), ["new.png"]);
        evict(&dir, 10);
        assert_eq!(names(&dir), ["new.png"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_leaves_temporary_files_alone() {
        let dir = scratch("evict-tmp");
        aged(&dir, "a.png.1.0.tmp", 100, 500);
        aged(&dir, "b.png", 10, 100);
        evict(&dir, 0);
        assert_eq!(names(&dir), ["a.png.1.0.tmp"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_hit_keeps_the_file_ahead_of_eviction() {
        let dir = scratch("evict-hit");
        let used = dir.join("used.png");
        aged(&dir, "other.png", 10, 100);
        std::fs::write(&used, png(30, 10)).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(1000);
        std::fs::File::options()
            .write(true)
            .open(&used)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let got = draw_mmdc("smd-no-such-program", "g", Theme::Dark, Some(&used));
        assert!(got.is_ok());
        evict(&dir, std::fs::metadata(&used).unwrap().len());
        let left = names(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(left, ["used.png"]);
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
