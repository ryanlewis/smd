//! POSIX shared memory objects that hold image data for the terminal to read
//! (`t=s`). A local terminal reads the data straight from memory instead of
//! decoding it from base64 sent through the tty. The terminal removes each
//! object once it has read it; [`unlink`] removes one it never read.

use std::io;
#[cfg(unix)]
use std::sync::atomic::{AtomicU32, Ordering};

/// The longest name macOS takes, the leading slash included. Linux takes
/// longer ones.
pub const MAX_NAME: usize = 31;

/// The name of this process's object number `n`. With the process id in it,
/// two smds never pick the same name.
pub fn name(pid: u32, n: u32) -> String {
    format!("/smd-{pid}-{n}")
}

/// Make a new object holding `data` and return its name. Only this user can
/// read it. An object of that name already there is left alone and is an
/// error.
#[cfg(unix)]
pub fn create(data: &[u8]) -> io::Result<String> {
    use std::ffi::CString;
    use std::fs::File;
    use std::os::fd::FromRawFd;

    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = name(std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    debug_assert!(name.len() <= MAX_NAME);
    let c_name = CString::new(name.as_str())?;
    // SAFETY: c_name is a valid C string. The mode is passed as c_uint, which
    // is mode_t on Linux and the promoted variadic type on macOS.
    let fd = unsafe {
        libc::shm_open(
            c_name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just opened and nothing else owns it.
    let file = unsafe { File::from_raw_fd(fd) };
    match fill(&file, data) {
        Ok(()) => Ok(name),
        Err(err) => {
            unlink(&name);
            Err(err)
        }
    }
}

#[cfg(not(unix))]
pub fn create(_: &[u8]) -> io::Result<String> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Write `data` into the new, empty object `file`. Linux writes: there a
/// mapping of a full tmpfs fails with SIGBUS when touched, while `write`
/// returns an error.
#[cfg(target_os = "linux")]
fn fill(mut file: &std::fs::File, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    file.write_all(data)
}

/// Write `data` into the new, empty object `file`. macOS has no `write` on
/// shared memory, so the object is sized and mapped.
#[cfg(all(unix, not(target_os = "linux")))]
fn fill(file: &std::fs::File, data: &[u8]) -> io::Result<()> {
    file.set_len(data.len() as u64)?;
    if data.is_empty() {
        return Ok(());
    }
    use std::os::fd::AsRawFd;
    // SAFETY: maps data.len() bytes of an object just sized to that.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            data.len(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ptr points to data.len() writable bytes no one else sees yet,
    // unmapped right after.
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), data.len());
        libc::munmap(ptr, data.len());
    }
    Ok(())
}

/// Remove the object `name`, if it is still there.
pub fn unlink(name: &str) {
    #[cfg(unix)]
    if let Ok(c_name) = std::ffi::CString::new(name) {
        // SAFETY: c_name is a valid C string.
        unsafe { libc::shm_unlink(c_name.as_ptr()) };
    }
    #[cfg(not(unix))]
    let _ = name;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fit_macos() {
        let longest = name(u32::MAX, u32::MAX);
        assert_eq!(longest, "/smd-4294967295-4294967295");
        assert!(longest.len() <= MAX_NAME);
    }

    /// Whether the object `name` is there, by opening it to read.
    #[cfg(unix)]
    fn exists(name: &str) -> bool {
        let c_name = std::ffi::CString::new(name).unwrap();
        // SAFETY: c_name is a valid C string; the descriptor is closed.
        unsafe {
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0 as libc::c_uint);
            if fd >= 0 {
                libc::close(fd);
            }
            fd >= 0
        }
    }

    #[cfg(unix)]
    #[test]
    fn create_holds_the_data_until_unlinked() {
        let data: Vec<u8> = (0..10_000).map(|i| i as u8).collect();
        let name = create(&data).unwrap();
        assert!(name.starts_with(&format!("/smd-{}-", std::process::id())));
        assert!(name.len() <= MAX_NAME);

        let c_name = std::ffi::CString::new(name.as_str()).unwrap();
        // SAFETY: c_name is a valid C string.
        let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0 as libc::c_uint) };
        assert!(fd >= 0);
        // SAFETY: fd was just opened and nothing else owns it.
        let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let len = file.metadata().unwrap().len();
        // macOS rounds the size up to a whole page.
        if cfg!(target_os = "linux") {
            assert_eq!(len, data.len() as u64);
        } else {
            assert!(len >= data.len() as u64, "{len}");
        }
        // SAFETY: maps the first data.len() bytes to read, unmapped right
        // after.
        let read = unsafe {
            use std::os::fd::AsRawFd;
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                data.len(),
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            let read = std::slice::from_raw_parts(ptr.cast::<u8>(), data.len()).to_vec();
            libc::munmap(ptr, data.len());
            read
        };
        assert_eq!(read, data);

        unlink(&name);
        assert!(!exists(&name));
        // Unlinking again, as after the terminal did, is harmless.
        unlink(&name);
    }

    #[cfg(unix)]
    #[test]
    fn each_object_gets_a_new_name() {
        let a = create(b"a").unwrap();
        let b = create(b"b").unwrap();
        assert_ne!(a, b);
        unlink(&a);
        unlink(&b);
    }
}
