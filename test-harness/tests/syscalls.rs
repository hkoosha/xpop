//! Native Linux checks for xpop's current FFI wrappers; no Xorg or D-Bus session.

use std::{
    io::{
        self,
        Read,
        Write,
    },
    os::{
        fd::AsRawFd,
        unix::{
            net::UnixStream,
            process::{
                CommandExt,
                ExitStatusExt,
            },
        },
    },
    process::{
        Child,
        Command,
    },
};

macro_rules! log {
    ($scope:ident $( @ $level:ident )? $fmt:literal $($args:tt)*) => {
        eprintln!($fmt $($args)*);
    };
}

include!(concat!(env!("OUT_DIR"), "/syscall_methods.rs"));

struct Job(Child);

impl Drop for Job {
    fn drop(&mut self) {
        // Child caches a reaped exit status, avoiding PID reuse on cleanup.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn invalid_process_and_group_selectors_are_rejected() {
    for pid in [0, -1] {
        assert_eq!(
            dragons::pgid(pid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    // Zero/current-group, -1/broadcast, group 1/negated broadcast, and overflow.
    // These must be rejected before invoking kill, never signalled in the test.
    for gid in [0, -1, 1, libc::pid_t::MIN] {
        assert_eq!(
            dragons::kill_group(gid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn nonexistent_process_lookup_preserves_errno() {
    // This is above Linux's maximum allocatable PID, but fits the FFI argument.
    let error = dragons::pgid(libc::pid_t::MAX).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn invalid_poll_descriptor_is_an_error() {
    let mut descriptors = [libc::pollfd {
        fd: libc::c_int::MAX,
        events: libc::POLLIN,
        revents: 0,
    }];
    assert_eq!(
        dragons::poll(&mut descriptors, 0)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EBADF)
    );
}

#[test]
fn poll_distinguishes_idle_readable_and_disconnected_sockets() -> io::Result<()>
{
    let (mut reader, mut writer) = UnixStream::pair()?;
    let mut descriptors = [libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    assert!(dragons::poll(&mut descriptors, 0)?);
    writer.write_all(b"x")?;
    assert!(!dragons::poll(&mut descriptors, 0)?);
    assert_ne!(descriptors[0].revents & libc::POLLIN, 0);
    let mut byte = [0];
    reader.read_exact(&mut byte)?;
    assert_eq!(byte, *b"x");
    drop(writer);
    assert!(!dragons::poll(&mut descriptors, 0)?);
    assert_ne!(descriptors[0].revents & libc::POLLHUP, 0);
    // Negative descriptors are intentionally ignored by poll, not invalid FDs.
    descriptors[0].fd = -1;
    assert!(dragons::poll(&mut descriptors, 0)?);
    Ok(())
}

#[test]
fn owned_child_group_terminates_and_has_a_terminal_status() -> io::Result<()> {
    let mut job =
        Job(Command::new("sleep").arg("60").process_group(0).spawn()?);
    let gid =
        libc::pid_t::try_from(job.0.id()).expect("Linux child PID fits pid_t");
    assert_eq!(dragons::pgid(gid)?, gid);
    assert!(job.0.try_wait()?.is_none());
    dragons::kill_group(gid)?;
    let status = job.0.wait()?;
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert_eq!(job.0.try_wait()?, Some(status));
    Ok(())
}

fn probe_value(command: &mut Command) -> io::Result<libc::c_int> {
    let output = command.output()?;
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    text.trim().parse().map_err(io::Error::other)
}

#[test]
fn pre_exec_installs_the_parent_death_signal_in_the_child() -> io::Result<()> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_syscall-probe"));
    command.arg("pdeathsig");
    dragons::pre_exec(&mut command);
    assert_eq!(probe_value(&mut command)?, libc::SIGTERM);
    Ok(())
}

#[test]
fn signal_interruption_is_reported_instead_of_blindly_retried() -> io::Result<()>
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_syscall-probe"));
    command.arg("interrupt");
    assert_eq!(probe_value(&mut command)?, libc::EINTR);
    Ok(())
}
