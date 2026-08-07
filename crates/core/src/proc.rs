//! Querying a running process's working directory.
//!
//! This is how shell tab labels follow `cd`. v1 had two paths — parsing OSC 7 out
//! of the PTY stream, and polling the child process — and its own notes concluded
//! that only the second was reliable: OSC 7 detection was per-chunk and stateless,
//! so a sequence split across two reads was missed entirely, and its
//! percent-decoding mangled non-ASCII paths. v2 ports the reliable half only.
//!
//! Framework-independent (it's a syscall), hence `core` rather than the gui crate.

use std::path::{Path, PathBuf};

/// Working directory of a live process, or `None` if it can't be read — the
/// process exited, or the platform isn't supported.
#[cfg(target_os = "macos")]
pub fn cwd_of(pid: u32) -> Option<PathBuf> {
    use std::os::raw::{c_int, c_void};

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    // `struct proc_vnodepathinfo` holds two `vnode_info_path` values: the current
    // directory then the root directory. Each is a `vnode_info` followed by a
    // `char[MAXPATHLEN]`. These offsets are the layout of that struct, which is
    // stable ABI — but they are magic numbers, so don't "tidy" them.
    const PROC_PIDVNODEPATHINFO: c_int = 9;
    const BUF_SIZE: usize = 2352; // sizeof(struct proc_vnodepathinfo)
    const CWD_PATH_OFFSET: usize = 152; // offsetof(pvi_cdir.vip_path)
    const MAXPATHLEN: usize = 1024;

    let mut buf = vec![0u8; BUF_SIZE];
    let ret = unsafe {
        proc_pidinfo(
            pid as c_int,
            PROC_PIDVNODEPATHINFO,
            0,
            buf.as_mut_ptr() as *mut c_void,
            BUF_SIZE as c_int,
        )
    };
    if ret <= 0 {
        return None;
    }
    let slice = buf.get(CWD_PATH_OFFSET..CWD_PATH_OFFSET + MAXPATHLEN)?;
    let nul = slice.iter().position(|&b| b == 0)?;
    // Lossy rather than strict: a path that isn't valid UTF-8 should still produce
    // a usable label instead of no label at all.
    let s = String::from_utf8_lossy(&slice[..nul]);
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s.into_owned()))
    }
}

#[cfg(target_os = "linux")]
pub fn cwd_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn cwd_of(_pid: u32) -> Option<PathBuf> {
    None
}

/// Short label for a directory: its basename, or the path itself when there isn't
/// one (`/`, or a relative path with no parent).
pub fn dir_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            if path == Path::new("/") {
                "/".to_string()
            } else {
                path.display().to_string()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_the_basename() {
        assert_eq!(dir_label(Path::new("/Users/x/code/sacrament")), "sacrament");
        assert_eq!(dir_label(Path::new("/tmp")), "tmp");
    }

    #[test]
    fn root_labels_as_itself() {
        assert_eq!(dir_label(Path::new("/")), "/");
    }

    #[test]
    fn a_trailing_slash_does_not_produce_an_empty_label() {
        assert_eq!(dir_label(Path::new("/tmp/")), "tmp");
    }

    #[test]
    fn our_own_cwd_is_readable() {
        // Proves the syscall path works on this platform rather than silently
        // returning None forever, which a label-only feature would hide.
        let mine = cwd_of(std::process::id()).expect("should read our own cwd");
        assert_eq!(
            mine.canonicalize().ok(),
            std::env::current_dir().ok().and_then(|p| p.canonicalize().ok())
        );
    }
}
