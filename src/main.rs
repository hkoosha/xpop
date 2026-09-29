#![allow(clippy::needless_return)]

use dbus::{
    Message,
    ffidisp::Connection as DBusConn,
};

pub const NAMESPACE: &str = "io.koosha.xpop";

#[derive(Default)]
struct R<T> {
    store: std::rc::Rc<std::cell::RefCell<T>>,
}

impl<T> Clone for R<T> {
    fn clone(&self) -> Self {
        return Self {
            store: self.store.clone(),
        };
    }
}

impl<T> R<T> {
    pub(crate) fn of(it: T) -> Self {
        return Self {
            store: std::rc::Rc::new(std::cell::RefCell::new(it)),
        };
    }

    pub(crate) fn get(&self) -> std::cell::Ref<'_, T> {
        return self.store.borrow();
    }

    pub(crate) fn write(
        &self,
        t: T,
    ) {
        *self.store.borrow_mut() = t;
    }
}

impl<T: Copy> R<T> {
    fn read(&self) -> T {
        return self.store.borrow().clone();
    }
}

#[derive(Default)]
struct O<T> {
    store: std::rc::Rc<std::cell::RefCell<Option<T>>>,
}

impl<T> O<T> {
    fn none() -> Self {
        return Self {
            store: std::rc::Rc::new(std::cell::RefCell::new(None)),
        };
    }

    fn read(&self) -> std::cell::Ref<'_, Option<T>> {
        return self.store.borrow();
    }

    fn write(
        &self,
        t: T,
    ) {
        *self.store.borrow_mut() = Some(t);
    }

    fn clear(&self) {
        self.store.borrow_mut().take();
    }

    fn is_present(&self) -> bool {
        return self.read().is_some();
    }
}

#[clippy::format_args]
macro_rules! log {
    ($whom:ident@info $fmt:literal $($arg:tt)*) => {{ log!([INFO, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident@warn $fmt:literal $($arg:tt)*) => {{ log!([WARN, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident@fail $fmt:literal $($arg:tt)*) => {{ log!([FAIL, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident@trac $fmt:literal $($arg:tt)*) => {{
        if $crate::cfg::TRACE.load(::std::sync::atomic::Ordering::Relaxed) {
            log!([TRAC, $whom, $fmt], [$($arg)*]);
        }
    }};
    ($whom:ident $fmt:literal $($arg:tt)*) => {{
        if $crate::cfg::DEBUG.load(::std::sync::atomic::Ordering::Relaxed) {
            log!([DBUG, $whom, $fmt], [$($arg)*]);
        }
    }};
    ([$level:ident, $whom:ident, $fmt:literal], [$($arg:tt)*]) => {{
         let d = ::std::time::SystemTime::now()
            .duration_since(::std::time::UNIX_EPOCH)
            .unwrap_or_default();
        eprintln!(
            concat!(
                "[{:012}.{:03}] [{}::",
                stringify!($level),
                "::",
                stringify!($whom),
                "] ",
                $fmt,
            ),
            d.as_secs(),
            d.subsec_millis(),
            $crate::NAMESPACE
            $($arg)*
        );
    }};
}

//noinspection SpellCheckingInspection
mod dragons {
    use libc::pollfd;
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    extern "C" fn child_exited(_: libc::c_int) {}

    pub(crate) fn sigaction() -> io::Result<()> {
        log!(libc@trac "sigaction...");

        // SAFETY: Linux sigaction's integer/mask fields accept zero, and its
        // optional restorer becomes None. The callback uses the required C ABI.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = child_exited as *const () as usize;

        // SAFETY: sa_mask is writable, aligned, and live for the entire call.
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: SIGCHLD is catchable, action is initialized, and a null old
        // action is permitted. The handler performs no non-signal-safe work.
        if unsafe {
            libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut())
        } != 0
        {
            return Err(io::Error::last_os_error());
        }

        log!(libc@trac "sigaction ok");
        return Ok(());
    }

    pub(crate) fn pgid(pid: libc::pid_t) -> io::Result<libc::pid_t> {
        if pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process lookup requires a positive PID",
            ));
        }
        log!(libc@trac "getting pgid of: {}", pid);

        // SAFETY: getpgid takes no pointers. A positive PID selects one process.
        let group = unsafe { libc::getpgid(pid) };
        if group == -1 {
            return Err(io::Error::last_os_error());
        }
        return Ok(group);
    }

    /// Return true on timeout; preserve EINTR so the event loop can reap children.
    /// POLLNVAL on any descriptor is reported as EBADF, not as readable activity.
    pub(crate) fn poll(
        fds: &mut [pollfd],
        timeout: i32,
    ) -> io::Result<bool> {
        let count = libc::nfds_t::try_from(fds.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many poll descriptors",
            )
        })?;
        log!(libc@trac "polling fds: timeout={}, count={}", timeout, fds.len());

        // SAFETY: the exclusive slice provides count initialized pollfd entries;
        // its storage cannot move during this call. A zero count permits an empty slice.
        let polled = unsafe { libc::poll(fds.as_mut_ptr(), count, timeout) };
        if polled < 0 {
            return Err(io::Error::last_os_error());
        }
        if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }

        log!(libc@trac "poll ended: {}", polled);
        return Ok(polled == 0);
    }

    pub(crate) fn kill_group(gid: libc::pid_t) -> io::Result<()> {
        if gid <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process-group termination requires a PGID greater than one",
            ));
        }
        log!(libc@trac "killing process group: {}", gid);

        // SAFETY: gid > 1 makes negation safe and excludes kill's special 0/-1
        // selectors. SIGTERM is valid; permission and existence errors are checked.
        if unsafe { libc::kill(-gid, libc::SIGTERM) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }

    pub(crate) fn try_kill_group(gid: libc::pid_t) {
        if let Err(err) = kill_group(gid)
            && err.raw_os_error() != Some(libc::ESRCH)
        {
            log!(libc@warn "could not kill process group: gid={}, error={}", gid, err);
        }
    }

    /// Arm SIGTERM on parent-thread death when Command::spawn runs the hook.
    /// Capture the expected parent before fork; hook failures return through spawn.
    /// Linux clears this setting for credential-changing execs; see
    /// <https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html>.
    pub(crate) fn pre_exec(it: &mut Command) {
        let parent = std::process::id();

        // SAFETY: the hook captures only a PID and uses libc calls and raw OS
        // errors after fork: no allocation, locks, logging, or unwinding. prctl's
        // variadic arguments have unsigned-long width, including unused zeros.
        unsafe {
            it.pre_exec(move || {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGTERM as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                if u32::try_from(libc::getppid()).ok() != Some(parent) {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                return Ok(());
            });
        }
    }
}

mod errors {
    use std::io;

    use x11rb::errors::{
        ConnectError,
        ConnectionError,
        ReplyError,
        ReplyOrIdError,
    };

    #[derive(thiserror::Error, Debug)]
    pub(crate) enum MyError {
        #[error("x11 connect error")]
        X11Connect(#[from] ConnectError),

        #[error("x11 connection error")]
        X11Connection(#[from] ConnectionError),

        #[error("x11 reply error")]
        X11Reply(#[from] ReplyError),

        #[error("x11 reply or id error")]
        X11ReplyOrIdError(#[from] ReplyOrIdError),

        #[error("dbus failure")]
        DBus(#[from] dbus::Error),

        #[error("dbus signal error: {0}")]
        DBusSignal(String),

        #[error("dbus signal send failure")]
        DBusSend,

        #[error("no X11 screen")]
        NoScreen,

        #[error("io error: {0}")]
        Io(#[from] io::Error),

        #[error("no command to run")]
        NoCommand,
    }

    pub(crate) type Z<T = ()> = Result<T, MyError>;

    pub(crate) trait ErrorLogger: Sized {
        fn and_log(self) -> Self;
    }

    impl<T> ErrorLogger for Result<T, MyError> {
        fn and_log(self) -> Self {
            match self {
                Ok(it) => Ok(it),
                Err(err) => {
                    log!(unexpected@fail "unexpected error: {}", err);
                    return Err(err);
                }
            }
        }
    }
}

mod cfg {
    use std::{
        path::PathBuf,
        sync::atomic::AtomicBool,
    };

    use clap::Parser;

    pub(crate) static TRACE: AtomicBool = AtomicBool::new(false);
    pub(crate) static DEBUG: AtomicBool = AtomicBool::new(false);

    #[derive(Debug, Copy, Clone, Eq, PartialEq, clap::ValueEnum)]
    pub(crate) enum Behavior {
        Hide,
        Appear,
        None,
    }

    #[derive(clap::Parser, Debug)]
    #[command(author, version, about)]
    pub(crate) struct Args {
        /// Send D-Bus signal to request visibility toggle and exit.
        #[arg(short, long, conflicts_with_all = ["command"])]
        pub(crate) signal: bool,

        #[arg(short, long, action = clap::ArgAction::Count)]
        pub(crate) verbose: u8,

        /// The XORG app and its arguments to host.
        #[arg(
            allow_hyphen_values = true,
            conflicts_with_all = ["signal"],
            required_unless_present_any = ["signal"],
            num_args = 1..
        )]
        pub(crate) command: Vec<String>,

        #[arg(short, long)]
        pub(crate) working_dir: Option<PathBuf>,

        #[arg(long, default_value_t = crate::NAMESPACE.to_string())]
        pub(crate) dbus_interface: String,

        #[arg(long, default_value_t = crate::NAMESPACE.split('.').last().unwrap().to_string())]
        pub(crate) dbus_member: String,

        #[arg(long, default_value_t = { let mut value = crate::NAMESPACE.replace('.', "/"); value.insert(0, '/'); value }
        )]
        pub(crate) dbus_path: String,

        #[arg(long, default_value = "appear")]
        pub(crate) on_start: Behavior,

        #[arg(long, default_value = "main")]
        pub(crate) title: String,

        #[arg(short, default_value_t = 0)]
        pub(crate) x: u32,

        #[arg(short, default_value_t = 0)]
        pub(crate) y: u32,

        #[arg(long, default_value = "100%")]
        pub(crate) width: String,

        #[arg(long, default_value = "100%")]
        pub(crate) height: String,
    }

    #[derive(Debug, Copy, Clone)]
    pub(crate) struct Area {
        pub(crate) x: i16,
        pub(crate) y: i16,
        pub(crate) width: u16,
        pub(crate) height: u16,
    }

    pub(crate) fn args_or_exit() -> Args {
        return Args::parse();
    }
}

mod x11 {
    use std::{
        collections::{
            HashSet,
            VecDeque,
        },
        os::fd::{
            AsFd,
            BorrowedFd,
        },
    };
    use x11rb::{
        connection::Connection as _,
        protocol::{
            Event,
            xproto::{
                Atom,
                AtomEnum,
                ChangeWindowAttributesAux,
                ConfigureWindowAux,
                ConnectionExt as _,
                CreateWindowAux,
                EventMask,
                InputFocus,
                MapState,
                PropMode,
                StackMode,
                Window,
                WindowClass,
            },
        },
        wrapper::{
            ConnectionExt,
            GrabServer,
        },
        xcb_ffi::XCBConnection,
    };

    use crate::{
        O,
        R,
        cfg::Area,
        dragons,
        errors::{
            MyError,
            Z,
        },
    };

    pub(crate) struct X11Host {
        net_wm_pid: Atom,
        conn: XCBConnection,
        root_win: Window,
    }

    impl X11Host {
        pub(crate) fn find_mapped_window(
            &self,
            gid: libc::pid_t,
        ) -> Z<Option<Window>> {
            log!(x11@trac "finding window...");

            let mut seen = HashSet::from([self.root_win]);
            let mut pending = VecDeque::from([self.root_win]);
            while let Some(parent) = pending.pop_front() {
                for window in self.conn.query_tree(parent)?.reply()?.children {
                    if !seen.insert(window) {
                        continue;
                    }

                    pending.push_back(window);
                    if self.matches_process_group(window, gid)?
                        && self.is_viewable(window)?
                    {
                        return Ok(Some(window));
                    }
                }
            }

            Ok(None)
        }

        pub(crate) fn watch_window(
            &self,
            window: Window,
        ) -> Z {
            self.conn
                .change_window_attributes(
                    window,
                    &ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE),
                )?
                .check()?;
            self.conn.flush()?;
            return Ok(());
        }

        pub(crate) fn is_mapped_window(
            &self,
            window: Window,
            gid: libc::pid_t,
        ) -> Z<bool> {
            return Ok(self.matches_process_group(window, gid)?
                && self.is_viewable(window)?);
        }

        pub(crate) fn is_window_pid_property(
            &self,
            atom: Atom,
        ) -> bool {
            return atom == self.net_wm_pid;
        }

        fn matches_process_group(
            &self,
            window: Window,
            group: libc::pid_t,
        ) -> Z<bool> {
            let pid = self
                .conn
                .get_property(
                    false,
                    window,
                    self.net_wm_pid,
                    AtomEnum::CARDINAL,
                    0,
                    1,
                )
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .and_then(|reply| {
                    reply.value32().and_then(|mut values| values.next())
                })
                .and_then(|pid| libc::pid_t::try_from(pid).ok())
                .filter(|pid| *pid > 0);
            let Some(pid) = pid
            else {
                return Ok(false);
            };

            match dragons::pgid(pid) {
                Ok(actual) => Ok(actual == group),
                Err(err) if err.raw_os_error() == Some(libc::ESRCH) => {
                    Ok(false)
                }
                Err(err) => Err(err.into()),
            }
        }

        fn is_viewable(
            &self,
            window: Window,
        ) -> Z<bool> {
            let it =
                self.conn.get_window_attributes(window)?.reply()?.map_state;
            Ok(it == MapState::VIEWABLE)
        }

        pub(crate) fn conn_poll_fd(&self) -> BorrowedFd<'_> {
            return self.conn.as_fd();
        }

        pub(crate) fn poll(&self) -> Z<Option<Event>> {
            return self.conn.poll_for_event().map_err(MyError::X11Connection);
        }

        pub(crate) fn grab_server(&self) -> Z<GrabServer<'_, XCBConnection>> {
            return Ok(GrabServer::grab(&self.conn)?);
        }

        pub(crate) fn flush(&self) -> Z {
            self.conn.flush()?;
            return Ok(());
        }

        pub(crate) fn root_is(
            &self,
            window: Window,
        ) -> bool {
            self.root_win == window
        }
    }

    pub(crate) struct EmbeddedWindowMan {
        x11: R<X11Host>,
        ready: R<bool>,
        embedded_win: O<Window>,
    }

    impl EmbeddedWindowMan {
        pub(crate) fn map_window(&self) -> Z {
            let Some(window) = *self.embedded_win.read()
            else {
                log!(embedded "no window, cannot map");
                return Ok(());
            };

            let conn = &self.x11.get().conn;
            conn.map_window(window)?.check()?;
            conn.flush()?;
            Ok(())
        }

        pub(crate) fn hide(&self) -> Z {
            let Some(window) = *self.embedded_win.read()
            else {
                return Ok(());
            };

            let conn = &self.x11.get().conn;
            conn.unmap_window(window)?.check()?;
            conn.flush()?;
            return Ok(());
        }

        pub(crate) fn write(
            &self,
            window: Window,
        ) {
            self.embedded_win.write(window);
        }

        pub(crate) fn is(
            &self,
            win: Window,
        ) -> bool {
            return self.embedded_win.read().is_some_and(|it| it == win);
        }

        pub(crate) fn is_present(&self) -> bool {
            return self.embedded_win.is_present();
        }

        pub(crate) fn is_viewable(&self) -> Z<bool> {
            let Some(window) = *self.embedded_win.read()
            else {
                return Ok(false);
            };

            return self.x11.get().is_viewable(window);
        }

        pub(crate) fn resize(
            &self,
            area: Area,
        ) -> Z {
            let Some(window) = *self.embedded_win.read()
            else {
                log!(embedded "no window, cannot resize");
                return Ok(());
            };

            log!(embedded "resizing");
            self.x11
                .get()
                .conn
                .configure_window(
                    window,
                    &ConfigureWindowAux::new()
                        .x(0)
                        .y(0)
                        .width(u32::from(area.width.max(1)))
                        .height(u32::from(area.height.max(1)))
                        .border_width(0),
                )?
                .check()?;
            self.x11.get().conn.flush()?;
            return Ok(());
        }

        /// Arm server-side recovery even when the client already has focus.
        ///
        /// RevertToParent becomes RevertToNone after reverting to the host. If
        /// the host then disappears, X discards keyboard input. PointerRoot
        /// avoids this cascade without either process having to run cleanup.
        pub(crate) fn focus(&self) -> Z<bool> {
            let Some(window) = *self.embedded_win.read()
            else {
                log!(embedded@warn "no embedded window, cannot focus");
                return Ok(false);
            };

            if !self.is_viewable()? {
                log!(embedded "embedded window not viewable, marking as not ready and ignoring focus");
                self.ready.write(false);
                return Ok(false);
            }

            let focus = self.x11.get().conn.get_input_focus()?.reply()?;
            if self.is(focus.focus)
                && focus.revert_to == InputFocus::POINTER_ROOT
            {
                log!(embedded "already focused, not doing anything further");
                return Ok(true);
            }

            self.x11
                .get()
                .conn
                .set_input_focus(
                    InputFocus::POINTER_ROOT,
                    window,
                    x11rb::CURRENT_TIME,
                )?
                .check()?;

            self.x11.get().conn.flush()?;

            Ok(true)
        }

        pub(crate) fn clear(&self) {
            self.embedded_win.clear();
        }
    }

    pub(crate) struct HostWindowMan {
        x11: R<X11Host>,
        area: Area,
        window: Window,
        ready: R<bool>,
    }

    impl HostWindowMan {
        pub(crate) fn set_host_area(
            &mut self,
            area: Area,
        ) {
            self.area = area;
        }

        fn raise(&self) -> Z {
            self.x11
                .get()
                .conn
                .configure_window(
                    self.window,
                    &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                )?
                .check()?;

            return Ok(());
        }

        pub(crate) fn hide(&self) -> Z {
            self.x11.get().conn.unmap_window(self.window)?.check()?;
            self.x11.get().conn.flush()?;
            return Ok(());
        }

        /// Keep PointerRoot reversion if xpop exits before a client is attached.
        pub(crate) fn focus(&self) -> Z {
            self.x11
                .get()
                .conn
                .set_input_focus(
                    InputFocus::POINTER_ROOT,
                    self.window,
                    x11rb::CURRENT_TIME,
                )?
                .check()?;
            self.x11.get().conn.flush()?;
            return Ok(());
        }

        pub(crate) fn is_focused(&self) -> Z<bool> {
            let focus = self.x11.get().conn.get_input_focus()?.reply()?.focus;
            return Ok(self.is(focus));
        }

        pub(crate) fn destroy(&self) -> Z {
            let conn = &self.x11.get().conn;
            conn.destroy_window(self.window)?.check()?;
            conn.flush()?;
            return Ok(());
        }

        pub(crate) fn show(&self) -> Z {
            self.x11.get().conn.map_window(self.window)?.check()?;
            return self.raise();
        }

        pub(crate) fn area(&self) -> &Area {
            return &self.area;
        }

        pub(crate) fn embed(
            &self,
            window: Window,
        ) -> Z {
            let conn = &self.x11.get().conn;

            // ReparentWindow automatically remaps an already mapped client.
            // Unmap first so the explicit map after resize can generate Expose.
            conn.unmap_window(window)?.check()?;

            conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new()
                    .override_redirect(1u32)
                    .event_mask(EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;

            conn.reparent_window(window, self.window, 0, 0)?.check()?;

            self.raise()?;

            self.ready.write(false);

            Ok(())
        }

        pub(crate) fn is_parent_of(
            &self,
            window: Window,
        ) -> Z<bool> {
            let parent =
                self.x11.get().conn.query_tree(window)?.reply()?.parent;
            return Ok(self.is(parent));
        }

        pub(crate) fn is(
            &self,
            window: Window,
        ) -> bool {
            self.window == window
        }
    }

    fn create_host_window(
        x11: R<X11Host>,
        area: Area,
        title: &str,
    ) -> Z<Window> {
        let x11 = x11.get();

        log!(x11_window@trac "generating window id...");
        let window = x11.conn.generate_id()?;

        log!(x11_window@trac "creating window");
        x11.conn
            .create_window(
                x11rb::COPY_FROM_PARENT as u8,
                window,
                x11.root_win,
                0,
                0,
                area.width,
                area.height,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .background_pixel(0)
                    .override_redirect(1u32)
                    .event_mask(
                        EventMask::STRUCTURE_NOTIFY
                            | EventMask::FOCUS_CHANGE
                            | EventMask::EXPOSURE,
                    ),
            )?
            .check()?;

        log!(x11_window@trac "configuring window size");
        x11.conn
            .configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(i32::from(area.x))
                    .y(i32::from(area.y))
                    .width(u32::from(area.width))
                    .height(u32::from(area.height)),
            )?
            .check()?;

        log!(x11_window@trac "setting window properties");
        x11.conn
            .change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_NAME,
                x11.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom,
                title.as_bytes(),
            )?
            .check()?;

        x11.conn.flush()?;

        log!(x11_window@trac "X11 host window created");
        return Ok(window);
    }

    pub(crate) fn open_x11() -> Z<(R<X11Host>, Area)> {
        log!(x11@trac "acquiring xcb connection...");
        let (conn, screen_index) = XCBConnection::connect(None)?;

        log!(x11@trac "acquiring xcb screen...");
        let screen = conn
            .setup()
            .roots
            .get(screen_index)
            .ok_or(MyError::NoScreen)?;

        log!(x11@trac "getting screen dimensions");
        let area = Area {
            x: 0,
            y: 0,
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
        };

        log!(x11@trac "set screen root window attributes");
        conn.change_window_attributes(
            screen.root,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()?;

        let net_wm_pid = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;

        conn.flush()?;

        let this = X11Host {
            root_win: screen.root,
            net_wm_pid,
            conn,
        };
        let this = R::of(this);

        return Ok((this, area));
    }

    pub(crate) fn open_host_man(
        x11: R<X11Host>,
        area: Area,
        title: &str,
        ready: R<bool>,
    ) -> Z<HostWindowMan> {
        log!(x11@trac "creating raw window...");

        let this = HostWindowMan {
            window: create_host_window(x11.clone(), area, title)?,
            x11,
            area,
            ready,
        };

        return Ok(this);
    }

    pub(crate) fn open_embed_man(
        x11: R<X11Host>,
        ready: R<bool>,
    ) -> EmbeddedWindowMan {
        let this = EmbeddedWindowMan {
            x11,
            ready,
            embedded_win: O::none(),
        };
        return this;
    }
}

mod app {
    use std::{
        cmp::min,
        io,
        os::{
            fd::AsRawFd,
            unix::process::CommandExt as _,
        },
        process::Command,
        time::{
            Duration,
            Instant,
            SystemTime,
        },
    };

    use dbus::{
        ffidisp::{
            Connection as DBusConn,
            ConnectionItem,
            WatchEvent,
        },
        message::MatchRule,
    };
    use x11rb::protocol::Event;

    use crate::x11::{
        EmbeddedWindowMan,
        HostWindowMan,
        open_embed_man,
        open_host_man,
    };
    use crate::{
        R,
        cfg::{
            Area,
            Args,
            Behavior,
        },
        dragons,
        errors::{
            MyError,
            Z,
        },
        x11::{
            X11Host,
            open_x11,
        },
    };

    struct Discovery {
        deadline: Instant,
        delay: Duration,
    }

    struct EmbeddedProcess {
        child: std::process::Child,
        gid: libc::pid_t,
    }

    pub(crate) struct Ctx {
        ready: R<bool>,
        process_group: Option<libc::pid_t>,
        args: Args,
        x11: R<X11Host>,
        dbus: DBusConn,
        signal_rule: MatchRule<'static>,
        showing: bool,
        focus_pending: bool,
        closed: bool,
        last_toggle: SystemTime,
        window_discovery: Option<Discovery>,
        process: Option<EmbeddedProcess>,

        host: HostWindowMan,
        embedded: EmbeddedWindowMan,
    }

    impl Ctx {
        const TOGGLE_COOL_DOWN_MILLIS: u128 = 40;
        const DISCOVERY_INITIAL_DELAY: Duration = Duration::from_millis(100);
        const DISCOVERY_MAX_DELAY: Duration = Duration::from_secs(1);

        pub(crate) fn looper(&mut self) -> Z {
            while !self.closed {
                self.ekran()?;
            }

            Ok(())
        }

        /// Process buffered XCB events before blocking on file descriptors.
        ///
        /// Synchronous request checks and replies can consume socket data while
        /// leaving events queued in XCB, so POLLIN alone cannot drive dispatch.
        pub(crate) fn ekran(&mut self) -> Z {
            log!(ekran "reap hosted");
            self.reap_hosted()?;
            if self.closed {
                return Ok(());
            }

            self.process_x11_events()?;

            log!(ekran "watch fds");
            let watches = self.dbus.watch_fds();
            let mut fds = Vec::with_capacity(watches.len() + 1);
            fds.push(libc::pollfd {
                fd: self.x11.get().conn_poll_fd().as_raw_fd(),
                events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
                revents: 0,
            });
            fds.extend(watches.iter().map(|watch| watch.to_pollfd()));

            log!(ekran "poll...");
            let timeout = self.discovery_timeout();
            let timed_out = match dragons::poll(&mut fds, timeout) {
                Ok(timed_out) => timed_out,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    return Ok(());
                }
                Err(err) => return Err(err.into()),
            };
            if timed_out {
                log!(ekran "retry discovery on poll timeout");
                self.retry_discovery()?;
                return Ok(());
            }

            let mask = libc::POLLERR | libc::POLLHUP;
            if fds[0].revents & mask != 0 {
                log!(ekran@info "X11 connection closed");
                self.quit()?;
                return Ok(());
            }

            if (fds[0].revents & libc::POLLIN) != 0 {
                self.process_x11_events()?;
            }

            for (watch, pollfd) in watches.iter().zip(&fds[1..]) {
                if pollfd.revents != 0
                    && let Err(err) =
                        self.process_dbus_watch(watch.fd(), pollfd.revents)
                {
                    log!(ekran@warn "error processing dbus watch: {}", err);
                }
            }

            return Ok(());
        }

        fn process_x11_events(&mut self) -> Z {
            loop {
                let event = self.x11.get().poll()?;
                let Some(event) = event
                else {
                    return Ok(());
                };

                if let Err(err) = self.process_x11_event(event) {
                    log!(ekran@warn "error processing X11 events: {}", err);
                }
            }
        }

        fn process_x11_event(
            &mut self,
            event: Event,
        ) -> Z {
            log!(ekran "x11 event: {:?}", event);

            match event {
                Event::MapNotify(event)
                    if self.x11.get().root_is(event.event)
                        && !self.host.is(event.window)
                        && !self.embedded.is_present() =>
                {
                    self.attach_window(event.window)?;
                }

                Event::PropertyNotify(event)
                    if !self.embedded.is_present()
                        && self
                            .x11
                            .get()
                            .is_window_pid_property(event.atom) =>
                {
                    self.attach_window(event.window)?;
                }

                Event::ReparentNotify(event)
                    if self.embedded.is(event.window)
                        && !self.host.is(event.parent)
                        && !self.host.is_parent_of(event.window)? =>
                {
                    log!(
                        ctx "embedded window was reparented away: window={:#x}, parent={:#x}",
                        event.window,
                        event.parent
                    );
                    self.ready.write(false);
                    self.embedded.clear();
                    self.schedule_discovery();
                }

                Event::MapNotify(event) if self.host.is(event.window) => {
                    self.embedded.resize(*self.host.area())?;
                    self.embedded.map_window()?;
                    self.update_readiness()?;
                }

                Event::UnmapNotify(event) if self.host.is(event.window) => {
                    self.ready.write(false);
                }

                Event::FocusIn(event) if self.host.is(event.event) => {
                    if self.showing
                        && event.mode
                            == x11rb::protocol::xproto::NotifyMode::NORMAL
                        && self.host.is_focused()?
                    {
                        self.focus_pending = true;
                        self.update_readiness()?;
                    }
                }

                Event::ConfigureNotify(event) if self.host.is(event.window) => {
                    self.host.set_host_area(Area {
                        x: event.x,
                        y: event.y,
                        width: event.width,
                        height: event.height,
                    });
                    self.embedded.resize(*self.host.area())?;
                }

                Event::MapNotify(event) if self.embedded.is(event.window) => {
                    self.embedded.resize(*self.host.area())?;
                    self.update_readiness()?;
                }

                Event::ConfigureNotify(event)
                    if self.embedded.is(event.window) =>
                {
                    if event.width != self.host.area().width
                        || event.height != self.host.area().height
                    {
                        self.embedded.resize(*self.host.area())?;
                    }
                }

                Event::UnmapNotify(event) if self.embedded.is(event.window) => {
                    self.ready.write(false);
                    if self.showing {
                        self.embedded.map_window()?;
                        self.update_readiness()?;
                    }
                }

                Event::DestroyNotify(event)
                    if self.embedded.is(event.window) =>
                {
                    log!(ctx@info
                        "embedded X11 window was destroyed: {:#x}",
                        event.window
                    );
                    self.ready.write(false);
                    self.embedded.clear();
                    self.schedule_discovery();
                }

                _ => {}
            }

            return Ok(());
        }

        fn start(&mut self) -> Z {
            self.dbus.add_match(&self.signal_rule.match_str())?;

            if self.showing {
                self.host.show()?;
                self.host.focus()?;
            }

            let mut it = Command::new(
                self.args.command.first().ok_or(MyError::NoCommand)?,
            );
            it.args(&self.args.command[1..]).process_group(0);
            dragons::pre_exec(&mut it);

            if let Some(dir) = self.args.working_dir.as_ref() {
                it.current_dir(dir);
            }

            let child = it.spawn()?;
            // Linux Child::id originates as a positive pid_t. process_group(0)
            // makes that PID the group ID as well.
            let pid = child.id() as libc::pid_t;

            self.process_group = Some(pid);
            self.process = Some(EmbeddedProcess { child, gid: pid });

            self.schedule_discovery();

            Ok(())
        }

        fn reap_hosted(&mut self) -> Z {
            let Some(hosted) = self.process.as_mut()
            else {
                return Ok(());
            };

            if let Some(status) = hosted.child.try_wait()? {
                self.on_child_exit(status)?;
            }

            return Ok(());
        }

        fn process_dbus_watch(
            &mut self,
            fd: libc::c_int,
            revents: libc::c_short,
        ) -> Z {
            let mut should_toggle = false;
            for item in self
                .dbus
                .watch_handle(fd, WatchEvent::from_revents(revents))
            {
                if let ConnectionItem::Signal(message) = item
                    && self.signal_rule.matches(&message)
                {
                    should_toggle = true;
                }
            }

            if should_toggle {
                self.toggle()?;
            }

            return Ok(());
        }

        fn on_child_exit(
            &mut self,
            status: std::process::ExitStatus,
        ) -> Z {
            if let Some(hosted) = self.process.take() {
                log!(ctx@info
                    "hosted process exited status, pid={}, status={}",
                    hosted.child.id(),
                    status
                );
                dragons::try_kill_group(hosted.gid);
            }
            return self.quit();
        }

        fn schedule_discovery(&mut self) {
            if !self.embedded.is_present() && self.window_discovery.is_none() {
                self.window_discovery = Some(Discovery {
                    deadline: Instant::now() + Self::DISCOVERY_INITIAL_DELAY,
                    delay: Self::DISCOVERY_INITIAL_DELAY,
                });
            }
        }

        fn discovery_timeout(&self) -> i32 {
            let Some(discovery) = self.window_discovery.as_ref()
            else {
                return -1;
            };
            let millis = discovery
                .deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1);
            return i32::try_from(millis).unwrap_or(i32::MAX);
        }

        fn retry_discovery(&mut self) -> Z {
            let Some(discovery) = self.window_discovery.take()
            else {
                return Ok(());
            };

            self.attach()?;

            if !self.embedded.is_present() {
                let delay = min(
                    discovery.delay.saturating_mul(2),
                    Self::DISCOVERY_MAX_DELAY,
                );
                self.window_discovery = Some(Discovery {
                    deadline: Instant::now() + delay,
                    delay,
                });
            }

            return Ok(());
        }

        fn attach(&mut self) -> Z {
            log!(ctx "attaching window");

            let Some(gid) = self.process_group
            else {
                log!(ctx@fail "attach called on missing gid");
                return Ok(());
            };

            if let Some(window) =
                self.x11.clone().get().find_mapped_window(gid)?
            {
                self.embed(window)?;
            }

            return Ok(());
        }

        fn attach_window(
            &mut self,
            window: x11rb::protocol::xproto::Window,
        ) -> Z {
            let Some(gid) = self.process_group
            else {
                log!(ctx@fail "attach_window called on missing gid");
                return Ok(());
            };

            self.x11.get().watch_window(window)?;
            if self.x11.get().is_mapped_window(window, gid)? {
                self.embed(window)?;
            }
            return Ok(());
        }

        fn embed(
            &mut self,
            window: x11rb::protocol::xproto::Window,
        ) -> Z {
            let x11 = self.x11.get();
            let server_grab = x11.grab_server()?;
            self.host.embed(window)?;
            self.embedded.write(window);
            self.embedded.resize(*self.host.area())?;
            self.embedded.map_window()?;

            drop(server_grab);
            x11.flush()?;
            drop(x11);

            self.window_discovery = None;
            self.focus_pending = self.showing;
            return self.update_readiness();
        }

        /// Clear focus intent only after a successful or already-satisfied request.
        /// Never focus the host here: its FocusIn would trigger another handoff.
        fn update_readiness(&mut self) -> Z {
            self.ready.write(self.embedded.is_viewable()?);

            if self.ready.read()
                && self.showing
                && self.focus_pending
                && self.embedded.focus()?
            {
                self.focus_pending = false;
            }

            return Ok(());
        }

        fn toggle(&mut self) -> Z {
            let now = SystemTime::now();
            let elapsed = now
                .duration_since(self.last_toggle)
                .unwrap_or(Duration::ZERO)
                .as_millis();
            if elapsed < Self::TOGGLE_COOL_DOWN_MILLIS {
                return Ok(());
            }
            self.last_toggle = now;

            self.showing = !self.showing;
            self.focus_pending = self.showing;

            if !self.showing {
                self.ready.write(false);
                self.embedded.hide()?;
                self.host.hide()
            }
            else {
                self.host.show()?;
                if !self.embedded.is_present() {
                    self.attach()?;
                }
                if self.embedded.is_present() {
                    self.embedded.map_window()?;
                    self.update_readiness()
                }
                else {
                    self.host.focus()
                }
            }
        }

        pub(crate) fn quit(&mut self) -> Z {
            if self.closed {
                return Ok(());
            }
            self.closed = true;

            if let Some(hosted) = self.process.take() {
                dragons::try_kill_group(hosted.gid);
            }

            self.host.destroy()?;

            return Ok(());
        }
    }

    fn calc_area(
        args: &Args,
        screen_area: Area,
    ) -> Area {
        fn dimension(
            value: &str,
            limit: u16,
        ) -> u16 {
            let limit = u32::from(limit).max(1);
            let raw = if let Some(percent) = value.strip_suffix('%') {
                percent
                    .parse::<u32>()
                    .ok()
                    .filter(|it| (1..=100).contains(it))
                    .map(|it| limit * it / 100)
                    .unwrap_or(limit)
            }
            else {
                value.parse::<u32>().unwrap_or(limit)
            };
            min(raw, limit).max(1) as u16
        }

        return Area {
            x: args.x.min(i16::MAX as u32) as i16,
            y: args.y.min(i16::MAX as u32) as i16,
            width: dimension(&args.width, screen_area.width),
            height: dimension(&args.height, screen_area.height),
        };
    }

    pub(crate) fn open_context(args: Args) -> Z<Ctx> {
        let ready = R::default();

        log!(x11@trac "opening x11 connection");
        let (x11, area) = open_x11()?;

        let mut this = Ctx {
            dbus: DBusConn::new_session()?,

            signal_rule: MatchRule::new_signal(
                &args.dbus_interface,
                &args.dbus_member,
            )
            .with_path(&args.dbus_path)
            .static_clone(),

            host: open_host_man(
                x11.clone(),
                calc_area(&args, area),
                args.title.as_ref(),
                ready.clone(),
            )?,

            embedded: open_embed_man(x11.clone(), ready.clone()),

            showing: args.on_start == Behavior::Appear,
            focus_pending: args.on_start == Behavior::Appear,

            last_toggle: SystemTime::UNIX_EPOCH,
            process_group: None,
            window_discovery: None,
            process: None,
            closed: false,

            ready,
            x11,
            args,
        };

        this.start()?;

        return Ok(this);
    }
}

use crate::errors::ErrorLogger;
use crate::{
    app::open_context,
    cfg::Args,
    errors::{
        MyError,
        Z,
    },
};

fn main() -> Z {
    let it: Args = cfg::args_or_exit();
    if it.verbose > 0 {
        cfg::DEBUG.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    if it.verbose > 1 {
        cfg::TRACE.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    log!(main "BEGIN");

    if it.signal {
        log!(main "doing signal and quit");

        return DBusConn::new_session()?
            .send(
                Message::new_signal(
                    &it.dbus_path,
                    &it.dbus_interface,
                    &it.dbus_member,
                )
                .map_err(MyError::DBusSignal)?,
            )
            .map(|_| ())
            .map_err(|_| MyError::DBusSend);
    }

    dragons::sigaction()?;

    log!(main@trac "opening context...");
    let mut ctx = open_context(it)?;

    log!(main "main loop reached");
    let result = ctx.looper().and_log();

    log!(main "will quit");
    ctx.quit()?;

    log!(main "fin");
    return result;
}
