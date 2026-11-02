//! smd ends when its terminal goes away, whether or not SIGHUP reaches it.
#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long smd may take to end once the terminal is gone.
const ENDS_WITHIN: Duration = Duration::from_secs(1);

/// A new pseudo-terminal, 80 by 24: its master and its slave. Both are
/// opened close-on-exec, so an smd another test starts meanwhile cannot
/// hold the master open, which would keep the pty up. The slave still
/// reaches smd as its stdin or stdout.
fn pty() -> (OwnedFd, OwnedFd) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    /// ptsname writes the name to a buffer every caller shares.
    static PTSNAME: Mutex<()> = Mutex::new(());
    let open = |path: &std::ffi::OsStr| -> OwnedFd {
        File::options()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .unwrap()
            .into()
    };
    let master = open("/dev/ptmx".as_ref());
    let fd = master.as_raw_fd();
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let _shared = PTSNAME.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: fd is a pty master; ptsname's name is copied before the lock
    // is let go, and TIOCSWINSZ only reads size.
    let name = unsafe {
        assert_eq!(libc::grantpt(fd), 0, "{}", std::io::Error::last_os_error());
        assert_eq!(libc::unlockpt(fd), 0, "{}", std::io::Error::last_os_error());
        assert_eq!(libc::ioctl(fd, libc::TIOCSWINSZ, &size), 0);
        let name = libc::ptsname(fd);
        assert!(!name.is_null(), "{}", std::io::Error::last_os_error());
        std::ffi::CStr::from_ptr(name).to_owned()
    };
    let slave = open(std::ffi::OsStr::from_bytes(name.to_bytes()));
    (master, slave)
}

/// Start smd on the slave of a new pty, as its controlling terminal, with
/// SIGHUP ignored as under `nohup` when `nohup`, and the markdown piped in
/// when `piped`. Its errors go to `errors.txt` in a folder named `name`, as
/// the terminal is gone by the time they would be read. Returns smd and the
/// pty's master.
fn smd_on_a_pty(name: &str, nohup: bool, piped: bool) -> (Child, OwnedFd) {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let note = dir.join("note.md");
    std::fs::write(&note, "# A note\n\nSome text.\n").unwrap();
    let (master, slave) = pty();
    let mut command = Command::new(env!("CARGO_BIN_EXE_smd"));
    command
        .arg("--no-graphics")
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", &dir)
        .stdout(slave.try_clone().unwrap())
        .stderr(File::create(dir.join("errors.txt")).unwrap());
    if piped {
        command.stdin(Stdio::piped());
    } else {
        command.arg(&note).stdin(slave.try_clone().unwrap());
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            libc::setsid();
            libc::ioctl(libc::STDOUT_FILENO, libc::TIOCSCTTY as _, 0);
            if nohup {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(slave);
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(b"# A note\n\nSome text.\n").unwrap();
    }
    (child, master)
}

/// Read what smd writes to the terminal until it has switched to the
/// alternate screen, then a little longer, so it is waiting for keys.
fn wait_until_smd_waits_for_keys(master: &OwnedFd) {
    let mut terminal = File::from(master.try_clone().unwrap());
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut settled = None;
    while settled.is_none_or(|at| Instant::now() < at) {
        assert!(Instant::now() < deadline, "smd did not start: {seen:?}");
        let mut pfd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        if unsafe { libc::poll(&mut pfd, 1, 50) } > 0 {
            let mut chunk = [0u8; 4096];
            let n = terminal.read(&mut chunk).unwrap_or(0);
            seen.extend_from_slice(&chunk[..n]);
        }
        let alternate = seen.windows(8).any(|w| w == b"\x1b[?1049h");
        if alternate && settled.is_none() {
            settled = Some(Instant::now() + Duration::from_millis(300));
        }
    }
}

/// Close the terminal under a waiting smd and check that it ends soon, as
/// it would by SIGHUP.
fn ends_when_the_terminal_goes(name: &str, nohup: bool, piped: bool) {
    let (mut child, master) = smd_on_a_pty(name, nohup, piped);
    wait_until_smd_waits_for_keys(&master);
    assert!(child.try_wait().unwrap().is_none(), "smd ended too soon");
    drop(master);
    let closed = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if closed.elapsed() > ENDS_WITHIN {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("smd was still running {ENDS_WITHIN:?} after its terminal went");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let errors = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(name)
        .join("errors.txt");
    let errors = std::fs::read_to_string(errors).unwrap_or_default();
    assert_eq!(status.signal(), Some(libc::SIGHUP), "{status:?}: {errors}");
}

#[test]
fn smd_ends_when_its_terminal_goes_and_sighup_is_ignored() {
    ends_when_the_terminal_goes("hangup-nohup", true, false);
}

#[test]
fn smd_ends_when_its_terminal_goes_while_reading_keys_from_dev_tty() {
    ends_when_the_terminal_goes("hangup-piped", true, true);
}

#[test]
fn smd_ends_when_its_terminal_goes_and_sends_sighup() {
    ends_when_the_terminal_goes("hangup-sighup", false, false);
}
