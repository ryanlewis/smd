//! Images on the web, fetched only when the config or `--remote-images`
//! turns them on. Each is fetched off the UI thread, a few at a time, and kept
//! in a disk cache for a day, so a note opens without the network the second
//! time that day. A fetch that fails leaves the image as text, as a missing
//! file does.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::images::{self, Job, Png};
use crate::mermaid;

/// How long a fetch may take, redirects and all.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The largest image fetched, in bytes.
const MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Redirects followed before a fetch is given up on.
const MAX_REDIRECTS: u32 = 5;

/// Fetches at once.
const MAX_FETCHES: usize = 4;

/// How long a cached image is used before it is fetched again.
const FRESH: Duration = Duration::from_secs(24 * 60 * 60);

/// The most the cache may hold, in bytes.
const CACHE_CAP: u64 = 100 * 1024 * 1024;

/// The extension of cached images, which may be any format.
const EXT: &str = "img";

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Turn fetching on or off for the rest of the run. Only the first call counts.
pub fn set_enabled(on: bool) {
    let _ = ENABLED.set(on);
}

/// Whether web images are fetched. Off unless turned on.
pub fn enabled() -> bool {
    ENABLED.get().copied().unwrap_or(false)
}

/// Whether `src` is an http or https URL.
pub fn is_remote(src: &str) -> bool {
    let lower = src.get(..8).unwrap_or(src).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Fetch the image at `url`, or take it from the cache.
pub fn job(url: String) -> Job {
    Box::new(move || {
        let path = cache_dir().map(|dir| dir.join(cache_name(&url)));
        cached(&url, path.as_deref(), SystemTime::now(), |url| {
            let _slot = Slot::take();
            fetch(agent(), url, MAX_BYTES)
        })
    })
}

/// The image at `url`: from the file at `cache` if it was fetched within
/// [`FRESH`] of `now`, else from `fetch`. What `fetch` gives is cached only
/// if it is an image, so an error page is not kept. An older cached image is
/// used when `fetch` fails or gives no image, as when offline.
fn cached(
    url: &str,
    cache: Option<&Path>,
    now: SystemTime,
    fetch: impl FnOnce(&str) -> Result<Vec<u8>, String>,
) -> Result<Png, String> {
    let mut stale = None;
    if let Some(path) = cache
        && let Ok(meta) = std::fs::metadata(path)
        && let Ok(data) = std::fs::read(path)
    {
        if meta.modified().is_ok_and(|at| fresh(at, now))
            && let Ok(png) = images::decode_remote(data.clone())
        {
            return Ok(png);
        }
        stale = Some(data);
    }
    let fetched = fetch(url).and_then(|data| Ok((images::decode_remote(data.clone())?, data)));
    let (png, data) = match (fetched, stale) {
        (Ok(fetched), _) => fetched,
        (Err(e), Some(old)) => return images::decode_remote(old).map_err(|_| e),
        (Err(e), None) => return Err(e),
    };
    if let Some(path) = cache {
        // An image that cannot be cached is still shown.
        if mermaid::store(path, &data).is_ok()
            && let Some(dir) = path.parent()
        {
            mermaid::evict_files(dir, CACHE_CAP, EXT);
        }
    }
    Ok(png)
}

/// Whether a file written at `at` is still fresh at `now`. One from the
/// future, as after the clock was set back, is not.
fn fresh(at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(at).is_ok_and(|age| age < FRESH)
}

/// The cache file name for `url`.
fn cache_name(url: &str) -> String {
    let mut hash = mermaid::Fnv::new();
    hash.write(url.as_bytes());
    format!("{:016x}.{EXT}", hash.0)
}

fn cache_dir() -> Option<PathBuf> {
    cache_dir_in(
        cfg!(target_os = "macos"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_CACHE_HOME"),
    )
}

fn cache_dir_in(macos: bool, home: Option<OsString>, xdg: Option<OsString>) -> Option<PathBuf> {
    Some(
        mermaid::cache_root(macos, home, xdg)?
            .join("smd")
            .join("images"),
    )
}

/// The client every fetch shares, so connections are reused.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| new_agent(TIMEOUT))
}

fn new_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(MAX_REDIRECTS)
        .user_agent(concat!("smd/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// GET `url`, following redirects, failing on an error status or a body over
/// `limit` bytes.
fn fetch(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let mut response = agent
        .get(escaped(url))
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    response
        .body_mut()
        .with_config()
        // ureq fails a body of exactly its limit, at the read that finds the end.
        .limit(limit + 1)
        .read_to_vec()
        .map_err(|e| format!("{url}: {e}"))
}

/// `url` with each character ureq would refuse written as `%XX`, as a
/// browser does: a space, `<` or `>` anywhere, a backtick before the query
/// and a double quote in it. Others, such as letters outside ASCII, are sent
/// as they are.
fn escaped(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut query = false;
    for c in url.chars() {
        query |= matches!(c, '?' | '#');
        let refused = c.is_ascii_control()
            || matches!(c, ' ' | '<' | '>')
            || c == if query { '"' } else { '`' };
        if refused {
            out.push_str(&format!("%{:02X}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

/// A place among the [`MAX_FETCHES`] fetches, given back when dropped.
struct Slot;

static RUNNING: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

impl Slot {
    fn take() -> Slot {
        let (lock, freed) = &RUNNING;
        let mut running = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *running >= MAX_FETCHES {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::tests::png;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server on localhost that answers every request with what `reply`
    /// gives for its path, counting the requests. Its URL has no slash at the
    /// end.
    fn serve(reply: impl Fn(&str) -> Vec<u8> + Send + 'static) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let text = String::from_utf8_lossy(&request);
                let path = text.split(' ').nth(1).unwrap_or("/").to_string();
                let _ = stream.write_all(&reply(&path));
            }
        });
        (url, hits)
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        [head.as_bytes(), body].concat()
    }

    fn status(line: &str) -> Vec<u8> {
        format!("HTTP/1.1 {line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
    }

    /// An agent that ignores proxies in the environment, as the server is
    /// on localhost.
    fn local_agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(MAX_REDIRECTS)
            .proxy(None)
            .build()
            .into()
    }

    /// A cache folder, not made yet, removed when dropped.
    fn scratch(name: &str) -> crate::testdir::TempDir {
        let dir = crate::testdir::TempDir::new(&format!("remote-{name}"));
        std::fs::remove_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn only_http_and_https_are_remote() {
        assert!(is_remote("https://x.org/a.png"));
        assert!(is_remote("http://x.org/a.png"));
        assert!(is_remote("HTTPS://x.org/a.png"));
        assert!(!is_remote("ftp://x.org/a.png"));
        assert!(!is_remote("a.png"));
        assert!(!is_remote("http:/a.png"));
        assert!(!is_remote("日本"));
    }

    #[test]
    fn web_images_are_off_unless_turned_on() {
        // No test turns them on, so the default holds.
        assert!(!enabled());
    }

    #[test]
    fn the_cache_lives_in_the_platform_cache_directory() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            cache_dir_in(false, os("/home/u"), os("/xdg")),
            Some(PathBuf::from("/xdg/smd/images"))
        );
        assert_eq!(
            cache_dir_in(false, os("/home/u"), None),
            Some(PathBuf::from("/home/u/.cache/smd/images"))
        );
        assert_eq!(cache_dir_in(false, None, None), None);
        let a = cache_name("https://x.org/a.png");
        assert_eq!(a, cache_name("https://x.org/a.png"));
        assert_ne!(a, cache_name("https://x.org/b.png"));
        assert!(a.ends_with(".img"));
    }

    #[test]
    fn a_cached_image_is_fresh_for_a_day() {
        let now = SystemTime::now();
        assert!(fresh(now, now));
        assert!(fresh(now - Duration::from_secs(23 * 3600), now));
        assert!(!fresh(now - Duration::from_secs(25 * 3600), now));
        assert!(!fresh(now + Duration::from_secs(60), now));
    }

    #[test]
    fn an_image_is_fetched_once_then_read_from_the_cache() {
        let data = png(4, 2);
        let body = data.clone();
        let (url, hits) = serve(move |_| ok(&body));
        let agent = local_agent(TIMEOUT);
        let dir = scratch("cache");
        let path = dir.join(cache_name(&url));
        let get = |now| cached(&url, Some(&path), now, |u| fetch(&agent, u, MAX_BYTES));
        let now = SystemTime::now();
        let want = Png::new(data.clone());
        assert_eq!(get(now), want);
        // Written at `now`, as the file system's clock may run ahead of it.
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(now).unwrap();
        assert_eq!(get(now), want);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Written whole, with no temporary file left behind.
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(names.len(), 1);
        // A day later it is fetched again.
        assert_eq!(get(now + FRESH), want);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_stale_image_is_used_when_it_cannot_be_fetched_again() {
        let served = Arc::new(AtomicUsize::new(0));
        let body = png(4, 2);
        let (url, hits) = serve(move |_| match served.fetch_add(1, Ordering::SeqCst) {
            0 => ok(&body),
            _ => status("500 Internal Server Error"),
        });
        let agent = local_agent(TIMEOUT);
        let dir = scratch("stale");
        let path = dir.join(cache_name(&url));
        let get = |now| cached(&url, Some(&path), now, |u| fetch(&agent, u, MAX_BYTES));
        let now = SystemTime::now();
        let first = get(now);
        let later = get(now + FRESH + Duration::from_secs(60));
        assert_eq!(first, Png::new(png(4, 2)));
        assert_eq!(later, first);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_page_that_is_not_an_image_is_not_cached() {
        // A captive portal's login page, then the image once logged in.
        let served = Arc::new(AtomicUsize::new(0));
        let body = png(4, 2);
        let (url, _) = serve(move |_| match served.fetch_add(1, Ordering::SeqCst) {
            0 => ok(b"<html>log in</html>"),
            _ => ok(&body),
        });
        let agent = local_agent(TIMEOUT);
        let dir = scratch("portal");
        let path = dir.join(cache_name(&url));
        let get = || {
            cached(&url, Some(&path), SystemTime::now(), |u| {
                fetch(&agent, u, MAX_BYTES)
            })
        };
        assert!(get().is_err());
        let second = get();
        assert_eq!(second, Png::new(png(4, 2)));
    }

    #[test]
    fn redirects_are_followed() {
        let body = png(3, 3);
        let (url, hits) = serve(move |path| {
            match path {
            "/old" => b"HTTP/1.1 302 Found\r\nLocation: /new\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            _ => ok(&body),
        }
        });
        let agent = local_agent(TIMEOUT);
        let got = fetch(&agent, &format!("{url}/old"), MAX_BYTES).unwrap();
        assert_eq!(got, png(3, 3));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_url_with_spaces_or_brackets_is_escaped() {
        let (url, _) = serve(|path| match path {
            "/my%20image.png" | "/a%3Cb%3E%60.png" | "/q.png?a=%22b%22" | "/café.png" => {
                ok(&png(1, 1))
            }
            _ => status("404 Not Found"),
        });
        let agent = local_agent(TIMEOUT);
        let paths = [
            "/my image.png",
            "/a<b>`.png",
            "/q.png?a=\"b\"",
            // Already escaped, or sent as it is as before.
            "/my%20image.png",
            "/café.png",
        ];
        for path in paths {
            let got = fetch(&agent, &format!("{url}{path}"), MAX_BYTES);
            assert_eq!(got, Ok(png(1, 1)), "{path}");
        }
    }

    #[test]
    fn a_failed_fetch_is_an_error_and_is_not_cached() {
        let (url, _) = serve(|path| {
            match path {
            "/loop" => b"HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            "/big" => ok(&[0; 100]),
            _ => status("404 Not Found"),
        }
        });
        let agent = local_agent(TIMEOUT);
        let dir = scratch("fail");
        let path = dir.join("x.img");
        let missing = format!("{url}/missing.png");
        let got = cached(&missing, Some(&path), SystemTime::now(), |u| {
            fetch(&agent, u, MAX_BYTES)
        });
        assert!(got.unwrap_err().contains("404"));
        assert!(!path.exists());
        assert!(fetch(&agent, &format!("{url}/loop"), MAX_BYTES).is_err());
        // Over the size cap.
        assert!(fetch(&agent, &format!("{url}/big"), 50).is_err());
        assert!(fetch(&agent, &format!("{url}/big"), 100).is_ok());
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/slow.png", listener.local_addr().unwrap());
        let agent = local_agent(Duration::from_millis(200));
        let started = std::time::Instant::now();
        assert!(fetch(&agent, &url, MAX_BYTES).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(listener);
    }

    #[test]
    fn the_limits_are_ten_mb_an_image_and_a_hundred_for_the_cache() {
        assert!(include_str!("../README.md").contains("a 10 MB limit"));
        assert_eq!(MAX_BYTES, 10_485_760);
        assert_eq!(CACHE_CAP, 104_857_600);
    }

    #[test]
    fn the_cache_directory_comes_from_the_environment() {
        assert_eq!(
            cache_dir(),
            cache_dir_in(
                cfg!(target_os = "macos"),
                std::env::var_os("HOME"),
                std::env::var_os("XDG_CACHE_HOME"),
            )
        );
    }

    #[test]
    fn no_more_than_four_fetches_run_at_once() {
        use std::sync::mpsc;
        // Taken on a thread, so a test that cannot take them fails, not hangs.
        let (sent, got) = mpsc::channel();
        std::thread::spawn(move || {
            let slots: Vec<Slot> = (0..MAX_FETCHES).map(|_| Slot::take()).collect();
            sent.send(slots).unwrap();
        });
        let slots = got.recv_timeout(Duration::from_secs(5)).unwrap();
        let (sent, got) = mpsc::channel();
        let waiting = std::thread::spawn(move || {
            let slot = Slot::take();
            sent.send(()).unwrap();
            drop(slot);
        });
        // A fifth waits for a place.
        assert!(got.recv_timeout(Duration::from_millis(200)).is_err());
        drop(slots);
        assert!(got.recv_timeout(Duration::from_secs(5)).is_ok());
        waiting.join().unwrap();
        // Every place is given back.
        assert_eq!(*RUNNING.0.lock().unwrap_or_else(|e| e.into_inner()), 0);
    }

    #[test]
    fn an_image_that_cannot_be_cached_is_still_shown() {
        let dir = scratch("uncached");
        std::fs::create_dir_all(&dir).unwrap();
        // The cache's directory is a file, so nothing can be written in it.
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        let path = file.join("x.img");
        let got = cached("http://x/a.png", Some(&path), SystemTime::now(), |_| {
            Ok(png(4, 2))
        });
        // Nothing was written anywhere else in it.
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(got, Png::new(png(4, 2)));
    }
}
