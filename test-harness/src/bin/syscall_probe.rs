//! Child-process fixture for checks that must not alter the test runner's signals.

use std::{
    io,
    os::unix::process::CommandExt,
    process::{
        Child,
        Command,
        Stdio,
    },
};

macro_rules! log {
    ($scope:ident $( @ $level:ident )? $fmt:literal $($args:tt)*) => {
        eprintln!($fmt $($args)*);
    };
}

include!(concat!(env!("OUT_DIR"), "/syscall_methods.rs"));

struct SignalSender(Child);

impl Drop for SignalSender {
    fn drop(&mut self) {
        // This unreaped child owns the separate group created below; its PID
        // cannot be reused before wait(). Linux Child IDs fit in pid_t.
        dragons::try_kill_group(self.0.id() as libc::pid_t);
        if let Err(err) = self.0.wait() {
            eprintln!("could not reap signal sender: {err}");
        }
    }
}

fn parent_death_signal() -> io::Result<libc::c_int> {
    let mut signal: libc::c_int = 0;
    // SAFETY: PR_GET_PDEATHSIG receives an aligned, writable c_int that stays
    // alive until return. On Linux, c_ulong has pointer width; all variadic
    // arguments have that width. The returned status is checked before use.
    let result = unsafe {
        libc::prctl(
            libc::PR_GET_PDEATHSIG,
            &mut signal as *mut libc::c_int as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(signal)
}

fn interrupted_poll() -> io::Result<libc::c_int> {
    dragons::sigaction()?;
    // Repeated delivery avoids depending on the first signal landing after
    // poll starts. The sender is bounded even if poll regresses to blind retry.
    let sender = SignalSender(
        Command::new("sh")
            .args([
                "-c",
                "i=0; while [ \"$i\" -lt 200 ]; do kill -CHLD \"$1\" || exit; i=$((i + 1)); sleep 0.01; done",
                "signal-sender",
            ])
            .arg(std::process::id().to_string())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?,
    );
    let result = dragons::poll(&mut [], 1_000);
    drop(sender);
    match result {
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(libc::EINTR),
        Err(err) => Err(err),
        Ok(_) => {
            Err(io::Error::other("poll did not report the delivered signal"))
        }
    }
}

fn main() -> io::Result<()> {
    let value = match std::env::args().nth(1).as_deref() {
        Some("pdeathsig") => parent_death_signal()?,
        Some("interrupt") => interrupted_poll()?,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected pdeathsig or interrupt",
            ));
        }
    };
    println!("{value}");
    Ok(())
}
