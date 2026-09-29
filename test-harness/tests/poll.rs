//! Buffered-event replay with real Unix socket readiness and current loop code.
//! OS waits are probed at zero timeout so a missed-event regression fails instead of hanging.
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{
        self,
        Read,
        Write,
    },
    os::{
        fd::{
            AsFd,
            AsRawFd,
            BorrowedFd,
        },
        unix::net::UnixStream,
    },
    rc::Rc,
};
type Z<T = ()> = Result<T, Box<dyn std::error::Error>>;
macro_rules! log {
    ($scope:ident $( @ $level:ident )? $fmt:literal $($args:tt)*) => {
        eprintln!($fmt $($args)*);
    };
}
struct R<T>(Rc<RefCell<T>>);
impl<T> R<T> {
    fn get(&self) -> std::cell::Ref<'_, T> {
        self.0.borrow()
    }
}
impl<T> Clone for R<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
// Protocol-neutral buffered event source. No X server, X protocol, or D-Bus daemon.
struct X11Host {
    socket: UnixStream,
    queued: RefCell<VecDeque<u8>>,
}
impl X11Host {
    fn conn_poll_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
    fn poll(&self) -> Z<Option<u8>> {
        if let Some(event) = self.queued.borrow_mut().pop_front() {
            return Ok(Some(event));
        }
        let mut bytes = [0; 64];
        match (&self.socket).read(&mut bytes) {
            Ok(0) => Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => {
                self.queued.borrow_mut().extend(&bytes[..n]);
                Ok(self.queued.borrow_mut().pop_front())
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
struct Watch(i32);
impl Watch {
    fn fd(&self) -> i32 {
        self.0
    }
    fn to_pollfd(&self) -> libc::pollfd {
        libc::pollfd {
            fd: self.0,
            events: libc::POLLIN,
            revents: 0,
        }
    }
}
struct DBus {
    socket: UnixStream,
}
impl DBus {
    fn watch_fds(&self) -> Vec<Watch> {
        vec![Watch(self.socket.as_raw_fd())]
    }
}
mod dragons {
    use super::*;
    use libc::pollfd;
    thread_local! {
        pub static TIMEOUTS: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
        pub static NEXT_ERROR: std::cell::Cell<Option<io::ErrorKind>> = const { std::cell::Cell::new(None) };
    }
    pub(crate) fn poll(
        fds: &mut [pollfd],
        timeout: i32,
    ) -> io::Result<bool> {
        TIMEOUTS.with_borrow_mut(|it| it.push(timeout));
        if let Some(kind) = NEXT_ERROR.with(|error| error.take()) {
            return Err(io::Error::from(kind));
        }
        poll_impl::poll(fds, 0)
    }
}
struct Ctx {
    x11: R<X11Host>,
    dbus: DBus,
    closed: bool,
    ready: bool,
    discovery: bool,
    handled: Vec<u8>,
    reaps: usize,
}
impl Ctx {
    fn reap_hosted(&mut self) -> Z {
        self.reaps += 1;
        Ok(())
    }
    fn quit(&mut self) -> Z {
        self.closed = true;
        Ok(())
    }
    fn discovery_timeout(&self) -> i32 {
        if self.discovery { 0 } else { -1 }
    }
    fn retry_discovery(&mut self) -> Z {
        if self.discovery {
            self.discovery = false;
            self.x11.get().queued.borrow_mut().push_back(b'U');
        }
        Ok(())
    }
    fn process_dbus_watch(
        &mut self,
        _: i32,
        revents: i16,
    ) -> Z {
        if revents & libc::POLLIN != 0 {
            let mut byte = [0];
            (&self.dbus.socket).read_exact(&mut byte)?;
            self.ready = false;
            self.x11.get().queued.borrow_mut().push_back(b'U');
        }
        Ok(())
    }
    fn process_x11_event(
        &mut self,
        event: u8,
    ) -> Z {
        self.handled.push(event);
        match event {
            b'U' => {
                self.ready = false;
                self.x11.get().queued.borrow_mut().push_back(b'M');
            }
            b'M' => self.ready = true,
            b'E' => {
                return Err(io::Error::other("one event handler failed").into());
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}
fn make_ctx() -> Z<(Ctx, UnixStream, UnixStream)> {
    let (socket, x_peer) = UnixStream::pair()?;
    socket.set_nonblocking(true)?;
    let (bus_socket, bus_peer) = UnixStream::pair()?;
    let ctx = Ctx {
        x11: R(Rc::new(RefCell::new(X11Host {
            socket,
            queued: RefCell::new(VecDeque::new()),
        }))),
        dbus: DBus { socket: bus_socket },
        closed: false,
        ready: false,
        discovery: false,
        handled: Vec::new(),
        reaps: 0,
    };
    Ok((ctx, x_peer, bus_peer))
}
fn check(
    name: &str,
    ok: bool,
    failures: &mut usize,
) {
    println!("{}: {}", if ok { "PASS" } else { "FAIL" }, name);
    *failures += usize::from(!ok);
}
#[test]
fn queued_events_dispatch_without_another_socket_wakeup() -> Z {
    let mut failures = 0;
    {
        let (mut ctx, _x_peer, _bus_peer) = make_ctx()?;
        ctx.x11.get().queued.borrow_mut().push_back(b'U');
        let mut fd = libc::pollfd {
            fd: ctx.x11.get().conn_poll_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(
            poll_impl::poll(std::slice::from_mut(&mut fd), 0)?,
            "buffered event must have no socket wakeup"
        );
        ctx.ekran()?;
        check(
            "buffered recovery dispatches U then M without POLLIN",
            ctx.ready && ctx.handled == b"UM",
            &mut failures,
        );
    }
    {
        let (mut ctx, _x_peer, _bus_peer) = make_ctx()?;
        ctx.discovery = true;
        ctx.ekran()?;
        assert!(!ctx.discovery && ctx.handled.is_empty());
        ctx.ekran()?;
        check(
            "discovery-created events survive disabling the timer",
            ctx.ready && ctx.handled == b"UM",
            &mut failures,
        );
    }
    {
        let (mut ctx, _x_peer, mut bus_peer) = make_ctx()?;
        bus_peer.write_all(b"T")?;
        ctx.ekran()?;
        ctx.ekran()?;
        check(
            "D-Bus-handler-created events dispatch on the next pass",
            ctx.ready && ctx.handled == b"UM",
            &mut failures,
        );
    }
    {
        let (mut ctx, mut x_peer, _bus_peer) = make_ctx()?;
        x_peer.write_all(b"U")?;
        ctx.ekran()?;
        check(
            "socket-delivered events still dispatch",
            ctx.ready && ctx.handled == b"UM",
            &mut failures,
        );
    }
    {
        let (mut ctx, _x_peer, _bus_peer) = make_ctx()?;
        ctx.x11.get().queued.borrow_mut().extend(b"EU");
        ctx.ekran()?;
        check(
            "one failed handler does not strand later recovery",
            ctx.ready && ctx.handled == b"EUM",
            &mut failures,
        );
    }
    {
        let (mut ctx, _x_peer, _bus_peer) = make_ctx()?;
        dragons::TIMEOUTS.with_borrow_mut(Vec::clear);
        ctx.ekran()?;
        check(
            "idle loop still selects one indefinite wait, not a retry timer",
            ctx.handled.is_empty()
                && dragons::TIMEOUTS.with_borrow(|it| it.as_slice() == [-1]),
            &mut failures,
        );
    }
    if failures != 0 {
        return Err(io::Error::other(format!(
            "{failures} scheduling scenario(s) failed"
        ))
        .into());
    }
    Ok(())
}

#[test]
fn interrupted_poll_returns_to_reaping() -> Z {
    let (mut ctx, _x_peer, _bus_peer) = make_ctx()?;
    dragons::NEXT_ERROR
        .with(|error| error.set(Some(io::ErrorKind::Interrupted)));
    ctx.ekran()?;
    assert!(!ctx.closed);
    assert_eq!(ctx.reaps, 1);
    ctx.ekran()?;
    assert_eq!(ctx.reaps, 2);
    Ok(())
}

#[test]
fn non_interrupt_poll_errors_propagate() -> Z {
    let (mut ctx, _x_peer, _bus_peer) = make_ctx()?;
    dragons::NEXT_ERROR
        .with(|error| error.set(Some(io::ErrorKind::PermissionDenied)));
    let error = ctx.ekran().unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    Ok(())
}

include!(concat!(env!("OUT_DIR"), "/poll_methods.rs"));
