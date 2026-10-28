//! Holding a local process's writes to a few paths: Landlock, through its
//! three system calls (`landlock_create_ruleset`, `landlock_add_rule`,
//! `landlock_restrict_self`).
//!
//! The ruleset is built in the parent, before the fork: each allowed path
//! is opened `O_PATH`, added as a rule that allows every write-side right
//! beneath it, and closed again. Between `fork` and `exec` the child only
//! sets `PR_SET_NO_NEW_PRIVS` and restricts itself with the ruleset it
//! inherited: two system calls on memory prepared before the fork, no
//! allocation, no lock. The restriction is registered after every other
//! hook (the network namespace's writes to `/proc/self/uid_map` come
//! first), is inherited by every descendant, and cannot be lifted.
//!
//! Handled, and so denied outside the allowed paths: the write-side rights
//! the kernel's Landlock ABI knows. ABI 1: writing to a file, removing a
//! file or a directory, making a regular file, a directory, a symbolic
//! link, a socket, a FIFO or a device node; ABI 2 adds `REFER` (linking or
//! renaming into another directory); ABI 3 adds truncating. Not handled,
//! so allowed everywhere: reading, listing and executing, `ioctl` on
//! devices (ABI 5), the network (ABI 4; [`crate::egress`] confines it), and
//! any right a later ABI adds. Pipes, sockets and other files that are not
//! on a mounted filesystem are not paths, and Landlock does not check them.
//!
//! Support is detected once per process: the ABI version, then
//! `/bin/sh` run under an empty scope, which must be refused a write to
//! `/dev/null`. `BRANCHYARD_GUARD=off` in this process's environment
//! reports it unsupported without trying.

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::ENV_GUARD;

/// `LANDLOCK_CREATE_RULESET_VERSION`: ask for the ABI version.
const CREATE_RULESET_VERSION: u32 = 1;
/// `LANDLOCK_RULE_PATH_BENEATH`.
const RULE_PATH_BENEATH: libc::c_long = 1;

const WRITE_FILE: u64 = 1 << 1;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
/// ABI 2.
const REFER: u64 = 1 << 13;
/// ABI 3.
const TRUNCATE: u64 = 1 << 14;

/// The rights that apply to a file rather than a directory, among those
/// handled here.
const FILE_RIGHTS: u64 = WRITE_FILE | TRUNCATE;

/// `struct landlock_ruleset_attr`, up to `handled_access_fs`: the smallest
/// size every kernel with Landlock accepts.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

/// `struct landlock_path_beneath_attr`, which the kernel packs.
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The write-side rights Landlock ABI `abi` knows.
fn write_rights(abi: u32) -> u64 {
    let mut rights = WRITE_FILE
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_CHAR
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_BLOCK
        | MAKE_SYM;
    if abi >= 2 {
        rights |= REFER;
    }
    if abi >= 3 {
        rights |= TRUNCATE;
    }
    rights
}

/// The kernel's Landlock ABI version, or why there is none.
fn abi() -> Result<u32, String> {
    // SAFETY: with a null attribute, size 0 and the version flag, the call
    // reads no memory and returns the ABI version.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<c_void>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if version > 0 {
        return u32::try_from(version).map_err(|_| format!("Landlock reports ABI {version}"));
    }
    let error = io::Error::last_os_error();
    Err(match error.raw_os_error() {
        Some(libc::ENOSYS) => {
            "this kernel has no Landlock (it needs Linux 5.13 or newer built with \
             CONFIG_SECURITY_LANDLOCK), or a seccomp filter hides it"
                .to_owned()
        }
        Some(libc::EOPNOTSUPP) => "Landlock is built into this kernel but turned off: add \
                                   landlock to the lsm= boot parameter"
            .to_owned(),
        _ => format!("asking the kernel for its Landlock ABI failed: {error}"),
    })
}

/// Whether this host can hold a process's writes, and its Landlock ABI;
/// why not otherwise. Detected once per process; `BRANCHYARD_GUARD=off`
/// reports it unsupported without trying.
pub fn supported() -> Result<u32, String> {
    if std::env::var(ENV_GUARD).is_ok_and(|v| v == "off") {
        return Err(format!("{ENV_GUARD}=off turns it off"));
    }
    static PROBE: OnceLock<Result<u32, String>> = OnceLock::new();
    PROBE.get_or_init(probe).clone()
}

/// The ABI, once a shell under an empty scope has been refused a write.
fn probe() -> Result<u32, String> {
    let abi = abi()?;
    let ruleset =
        Ruleset::build(abi, &[]).map_err(|e| format!("building a Landlock ruleset failed: {e}"))?;
    let mut command = Command::new("/bin/sh");
    // `true` is a regular built-in: its redirection failing fails it, and
    // the shell goes on.
    command
        .args(["-c", "if true >/dev/null; then exit 3; fi; exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ruleset.restrict(&mut command);
    let status = command
        .spawn()
        .and_then(|mut child| child.wait())
        .map_err(|e| format!("a process could not be restricted with Landlock: {e}"))?;
    match status.code() {
        Some(0) => Ok(abi),
        Some(3) => Err(format!(
            "Landlock ABI {abi} restricted a process, but it could still write to /dev/null"
        )),
        _ => Err(format!(
            "a process restricted with Landlock failed: {status}"
        )),
    }
}

/// A ruleset that allows writing beneath some paths only, built before the
/// fork and applied by [`Ruleset::restrict`].
pub(crate) struct Ruleset {
    fd: OwnedFd,
}

impl Ruleset {
    /// Allow writes beneath each of `paths`, which must be absolute, and
    /// nowhere else; a path that does not exist is left out (nothing could
    /// be written there without making it in a parent that is out of
    /// scope). Errors when this host has no Landlock ([`supported`]).
    pub fn new(paths: &[PathBuf]) -> io::Result<Ruleset> {
        let abi = supported().map_err(|why| io::Error::new(io::ErrorKind::Unsupported, why))?;
        Ruleset::build(abi, paths)
    }

    fn build(abi: u32, paths: &[PathBuf]) -> io::Result<Ruleset> {
        let handled = write_rights(abi);
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        // SAFETY: `attr` is a valid `landlock_ruleset_attr` prefix of the
        // size passed, alive for the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = i32::try_from(fd).map_err(|_| io::Error::other("a ruleset fd out of range"))?;
        // SAFETY: the kernel returned a new file descriptor that nothing
        // else owns (opened close-on-exec).
        let ruleset = Ruleset {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        };
        for path in paths {
            ruleset.allow(path, handled)?;
        }
        Ok(ruleset)
    }

    /// Add a rule allowing `rights` beneath `path` (only its file rights
    /// when it is not a directory).
    fn allow(&self, path: &std::path::Path, rights: u64) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not an absolute path", path.display()),
            ));
        }
        let opened = rustix::fs::open(
            path,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        );
        let parent = match opened {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(errno) => {
                return Err(io::Error::new(
                    io::Error::from(errno).kind(),
                    format!("{}: {errno}", path.display()),
                ))
            }
        };
        let directory = rustix::fs::fstat(&parent)
            .map(|stat| rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir())?;
        let attr = PathBeneathAttr {
            allowed_access: match directory {
                true => rights,
                false => rights & FILE_RIGHTS,
            },
            parent_fd: parent.as_raw_fd(),
        };
        // SAFETY: `attr` is a valid packed `landlock_path_beneath_attr`,
        // alive for the call; its fd stays open until `parent` drops.
        let added = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                self.fd.as_raw_fd(),
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        if added < 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("allowing writes beneath {}: {error}", path.display()),
            ));
        }
        Ok(())
    }

    /// Restrict the process `command` starts with this ruleset, as the last
    /// thing it does before it execs: register this after every other
    /// `pre_exec` hook. A restriction that fails fails the spawn, so the
    /// process never runs unguarded.
    pub fn restrict(self, command: &mut Command) {
        let fd = self.fd;
        // SAFETY: the hook makes two system calls on a descriptor opened
        // before the fork; it allocates nothing and takes no lock, as code
        // between fork and exec must not.
        unsafe {
            command.pre_exec(move || {
                rustix::thread::set_no_new_privs(true)?;
                let restricted =
                    libc::syscall(libc::SYS_landlock_restrict_self, fd.as_raw_fd(), 0u32);
                match restricted {
                    0 => Ok(()),
                    _ => Err(io::Error::last_os_error()),
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_abi_handles_the_write_rights_it_knows() {
        let first = write_rights(1);
        assert_eq!(first & REFER, 0);
        assert_eq!(first & TRUNCATE, 0);
        assert_ne!(first & WRITE_FILE, 0);
        assert_ne!(write_rights(2) & REFER, 0);
        assert_eq!(write_rights(2) & TRUNCATE, 0);
        assert_eq!(write_rights(3), first | REFER | TRUNCATE);
        // Later ABIs add nothing on the write side that is handled here.
        assert_eq!(write_rights(7), write_rights(3));
        // Reading and executing are never handled.
        for right in [1 << 0, 1 << 2, 1 << 3] {
            assert_eq!(write_rights(7) & right, 0);
        }
    }

    #[test]
    fn the_attributes_have_the_kernels_layout() {
        assert_eq!(std::mem::size_of::<RulesetAttr>(), 8);
        assert_eq!(std::mem::size_of::<PathBeneathAttr>(), 12);
    }
}
