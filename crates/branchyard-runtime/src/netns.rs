//! Confining a local process's network: a new user and network namespace
//! whose only way out is one listener handed back to the caller.
//!
//! The child, between `fork` and `exec`, unshares a user namespace (so no
//! privilege is needed) and a network namespace, maps its own user and
//! group IDs to themselves, brings the namespace's loopback up, and binds a
//! TCP listener to `127.0.0.1:<port>` there. It passes that listener to the
//! parent over a socket pair and execs the program. The namespace has no
//! other interface, so the program and its descendants reach only that
//! listener, whose connections the parent accepts in its own namespace
//! (Branchyard's egress proxy serves them). A listener keeps its namespace:
//! a connection the parent accepts was made inside it.
//!
//! Everything the child does runs between `fork` and `exec` and so uses
//! only system calls on memory prepared before the fork: no allocation, no
//! locks.
//!
//! Support is detected at run time by confining `/bin/sh -c 'exit 0'`: a
//! kernel without unprivileged user namespaces, or a security module that
//! forbids them or what they need here, makes that fail, and the reason is
//! kept. `BRANCHYARD_EGRESS_NETNS=off` in this process's environment
//! reports it unsupported without trying.

use std::ffi::CStr;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};

use crate::ENV_EGRESS_NETNS as ENV_NETNS;

/// `SIOCGIFFLAGS` and `SIOCSIFFLAGS`.
const GET_FLAGS: rustix::ioctl::Opcode = 0x8913;
const SET_FLAGS: rustix::ioctl::Opcode = 0x8914;
/// `IFF_UP`.
const UP: i16 = 0x1;

/// `struct ifreq` with the flags member of its union, padded to the
/// union's size on 64-bit targets (larger than needed on 32-bit ones).
#[repr(C)]
struct InterfaceFlags {
    name: [u8; 16],
    flags: i16,
    _pad: [u8; 22],
}

/// Whether this host can confine a process here, and why not.
pub fn supported() -> Result<(), String> {
    if std::env::var(ENV_NETNS).is_ok_and(|v| v == "off") {
        return Err(format!("{ENV_NETNS}=off turns it off"));
    }
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "exit 0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let (mut child, _listener) = spawn(&mut command, 0).map_err(|e| {
                format!("this host does not allow an unprivileged network namespace ({e})")
            })?;
            match child.wait() {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(format!("a confined probe failed: {status}")),
                Err(e) => Err(format!("a confined probe failed: {e}")),
            }
        })
        .clone()
}

/// What the child needs, prepared before the fork.
struct Prepared {
    uid_map: Vec<u8>,
    gid_map: Vec<u8>,
    sender: OwnedFd,
    port: u16,
}

/// Spawn `command` confined, with its listener on `127.0.0.1:port` in its
/// namespace (`0` for any port); returns the child and the listener.
pub fn spawn(command: &mut Command, port: u16) -> io::Result<(Child, TcpListener)> {
    let (receiver, sender) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let prepared = Prepared {
        uid_map: format!("{uid} {uid} 1").into_bytes(),
        gid_map: format!("{gid} {gid} 1").into_bytes(),
        sender,
        port,
    };
    // SAFETY: `confine` makes only system calls, on memory prepared above;
    // it allocates nothing and takes no lock, as code between fork and exec
    // must not.
    unsafe {
        command.pre_exec(move || confine(&prepared));
    }
    // The listener was sent before the exec, so it is queued once the
    // spawn has succeeded: the receive below does not wait.
    let child = command.spawn()?;
    let mut byte = [0u8; 1];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let received = rustix::net::recvmsg(
        &receiver,
        &mut [IoSliceMut::new(&mut byte)],
        &mut ancillary,
        RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
    );
    let mut listener = None;
    if received.is_ok() {
        for message in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(fds) = message {
                for fd in fds {
                    listener.get_or_insert(fd);
                }
            }
        }
    }
    match listener {
        Some(fd) => Ok((child, TcpListener::from(fd))),
        None => {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            Err(io::Error::other(
                "the confined process did not hand back its listener",
            ))
        }
    }
}

fn confine(prepared: &Prepared) -> io::Result<()> {
    use rustix::thread::UnshareFlags;
    // SAFETY: between fork and exec there is one thread, and file
    // descriptors are not unshared.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWUSER | UnshareFlags::NEWNET)? };
    // Kernels before 3.19 have no setgroups file and need no denial.
    match write(c"/proc/self/setgroups", b"deny") {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        other => other?,
    }
    write(c"/proc/self/uid_map", &prepared.uid_map)?;
    write(c"/proc/self/gid_map", &prepared.gid_map)?;
    loopback_up()?;
    let listener = rustix::net::socket_with(
        AddressFamily::INET,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::net::bind(
        &listener,
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, prepared.port),
    )?;
    rustix::net::listen(&listener, 128)?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    let fds = [listener.as_fd()];
    if !ancillary.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::from(rustix::io::Errno::NOBUFS));
    }
    rustix::net::sendmsg(
        &prepared.sender,
        &[IoSlice::new(b"L")],
        &mut ancillary,
        SendFlags::empty(),
    )?;
    Ok(())
}

fn write(path: &CStr, bytes: &[u8]) -> io::Result<()> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let written = rustix::io::write(&fd, bytes)?;
    match written == bytes.len() {
        true => Ok(()),
        false => Err(io::Error::from(rustix::io::Errno::IO)),
    }
}

/// Bring the namespace's `lo` up, which gives it `127.0.0.1`.
fn loopback_up() -> io::Result<()> {
    let socket = rustix::net::socket_with(
        AddressFamily::INET,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let mut request = InterfaceFlags {
        name: [0; 16],
        flags: 0,
        _pad: [0; 22],
    };
    request.name[..2].copy_from_slice(b"lo");
    // SAFETY: both opcodes take a `struct ifreq`, of which `InterfaceFlags`
    // is the name and flags members, at least as large.
    unsafe {
        rustix::ioctl::ioctl(
            &socket,
            rustix::ioctl::Updater::<GET_FLAGS, InterfaceFlags>::new(&mut request),
        )?;
        request.flags |= UP;
        rustix::ioctl::ioctl(
            &socket,
            rustix::ioctl::Updater::<SET_FLAGS, InterfaceFlags>::new(&mut request),
        )?;
    }
    Ok(())
}
