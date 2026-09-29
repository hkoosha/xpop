#![allow(clippy::needless_return)]

use dbus::{
    Message,
    ffidisp::Connection as DBusConn,
};

pub const NAMESPACE: &str = "io.koosha.xpop";

#[clippy::format_args]
macro_rules! log {
    ($whom:ident@info $fmt:literal $($arg:tt)*) => {{ log!([INFO, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident@warn $fmt:literal $($arg:tt)*) => {{ log!([WARN, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident@fail $fmt:literal $($arg:tt)*) => {{ log!([FAIL, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident $fmt:literal $($arg:tt)*) => {{
        if $crate::cfg::DEBUG.load(::std::sync::atomic::Ordering::SeqCst) {
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

    use crate::cfg::Z;

    extern "C" fn child_exited(_: libc::c_int) {}

    pub(crate) fn sigaction() -> Z {
        log!(libc "sigaction...");

        let ok = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = child_exited as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut())
        };

        if ok != 0 {
            let err = io::Error::last_os_error();
            log!(libc@fail "sigaction failure: {}", err);
            Err(err)?;
        };

        log!(libc "sigaction ok");
        return Ok(());
    }

    pub(crate) fn pgid(pid: libc::pid_t) -> Z<libc::pid_t> {
        log!(libc "getting pgid of: {}", pid);

        return Ok(unsafe { libc::getpgid(pid) });
    }

    pub(crate) fn waitpid(pid: libc::pid_t) -> Z<(libc::pid_t, libc::c_int)> {
        log!(libc "waiting gid: {}", pid);

        let mut status = 0;
        let wait = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        return Ok((wait, status));
    }

    pub(crate) fn poll(
        fds: &mut Vec<pollfd>,
        timeout: i32,
        do_while: impl Fn() -> bool,
    ) -> Z<bool> {
        log!(libc "polling fds: timeout={}, count={}", timeout, fds.len());

        let polled = loop {
            let ok = unsafe {
                libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout)
            };

            if ok < 0 {
                let err = io::Error::last_os_error();
                if do_while() && err.raw_os_error() == Some(libc::EINTR) {
                    log!(libc "interrupted poll, will retry: count={}", fds.len());
                    continue;
                }

                log!(libc "poll failed: {}", err);
                Err(err)?;
            }

            log!(libc "poll ended: {}", ok);
            break ok;
        };

        return Ok(polled == 0);
    }

    pub(crate) fn kill(it: libc::pid_t) -> Z {
        log!(libc "killing pid: {}", it);

        if unsafe { libc::kill(it, libc::SIGTERM) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                log!(libc "killing pid failed: pid={}, err={}", it, err);
                Err(err)?;
            }
        }

        return Ok(());
    }

    pub(crate) fn try_kill(it: libc::pid_t) {
        if let Err(err) = kill(it) {
            log!(libc@warn "could not kill: pid={}, error={}", it, err);
        }
    }

    pub(crate) fn pre_exec(it: &mut Command) {
        unsafe {
            it.pre_exec(|| {
                let parent = libc::getppid();

                return if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM)
                    != 0
                {
                    Err(io::Error::last_os_error())
                }
                else if libc::getppid() != parent {
                    Err(io::Error::from_raw_os_error(libc::ESRCH))
                }
                else {
                    Ok(())
                };
            });
        }
    }
}

mod cfg {
    use std::{
        io,
        path::PathBuf,
        sync::atomic::AtomicBool,
    };

    use clap::Parser;
    use x11rb::errors::{
        ConnectError,
        ConnectionError,
        ReplyError,
        ReplyOrIdError,
    };

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

        #[arg(short, long)]
        pub(crate) verbose: bool,

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

        #[error("io error")]
        Io(#[from] io::Error),

        #[error("no command to run")]
        NoCommand,
    }

    pub(crate) type Z<T = ()> = Result<T, MyError>;

    pub(crate) fn args_or_exit() -> Args {
        return Args::parse();
    }
}

mod x11 {
    use crate::{
        cfg::{
            Area,
            MyError,
            Z,
        },
        dragons,
    };
    use std::collections::{
        HashSet,
        VecDeque,
    };
    use std::os::fd::{
        AsFd,
        BorrowedFd,
    };
    use x11rb::protocol::Event;
    use x11rb::{
        connection::Connection as _,
        protocol::xproto::{
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
            Window,
            WindowClass,
        },
        wrapper::ConnectionExt,
        xcb_ffi::XCBConnection,
    };

    pub(crate) struct EmbeddedProcess {
        pub(crate) pid: libc::pid_t,
        pub(crate) gid: libc::pid_t,
    }

    struct RawWindow {
        area: Area,
        window: Window,
    }

    impl RawWindow {
        fn create(
            conn: &XCBConnection,
            root: Window,
            area: Area,
            title: &str,
        ) -> Z<Self> {
            log!(x11_window "generating window id...");
            let this = Self {
                window: conn.generate_id()?,
                area,
            };

            log!(x11_window "creating window");
            conn.create_window(
                x11rb::COPY_FROM_PARENT as u8,
                this.window,
                root,
                0,
                0,
                this.area.width,
                this.area.height,
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

            log!(x11_window "configuring window size");
            conn.configure_window(
                this.window,
                &ConfigureWindowAux::new()
                    .x(i32::from(this.area.x))
                    .y(i32::from(this.area.y))
                    .width(u32::from(this.area.width))
                    .height(u32::from(this.area.height)),
            )?
            .check()?;

            log!(x11_window "setting window properties");
            conn.change_property8(
                PropMode::REPLACE,
                this.window,
                AtomEnum::WM_NAME,
                conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom,
                title.as_bytes(),
            )?
            .check()?;

            conn.flush()?;

            log!(x11_window "X11 window created");
            return Ok(this);
        }

        fn show(
            &self,
            conn: &XCBConnection,
        ) -> Z {
            conn.map_window(self.window)?.check()?;
            return conn.flush().map_err(|it| it.into());
        }

        fn hide(
            &self,
            conn: &XCBConnection,
        ) -> Z {
            conn.unmap_window(self.window)?.check()?;
            return conn.flush().map_err(|it| it.into());
        }

        fn owns(
            &self,
            window: Window,
        ) -> bool {
            self.window == window
        }

        fn focus(
            &self,
            conn: &XCBConnection,
        ) -> Z {
            conn.set_input_focus(
                InputFocus::PARENT,
                self.window,
                x11rb::CURRENT_TIME,
            )?
            .check()?;

            return conn.flush().map_err(|it| it.into());
        }

        fn destroy(
            &self,
            conn: &XCBConnection,
        ) -> Z {
            conn.destroy_window(self.window)?.check()?;

            return conn.flush().map_err(|it| it.into());
        }
    }

    pub(crate) struct X11Host {
        net_wm_pid: Atom,
        conn: XCBConnection,
        root: Window,
        raw: RawWindow,
        pub(crate) process_group: Option<libc::pid_t>,

        pub(crate) ready: bool,
        pub(crate) window: Option<Window>,
    }

    impl X11Host {
        pub(crate) fn open(
            title: &str,
            get_area: impl FnOnce(u16, u16) -> Area,
        ) -> Z<Self> {
            log!(x11 "acquiring xcb connection...");
            let (conn, screen_index) = XCBConnection::connect(None)?;

            log!(x11 "acquiring xcb screen...");
            let screen = conn
                .setup()
                .roots
                .get(screen_index)
                .ok_or(MyError::NoScreen)?;

            log!(x11 "getting screen dimensions");
            let area =
                get_area(screen.width_in_pixels, screen.height_in_pixels);

            log!(x11 "set root window attributes");
            conn.change_window_attributes(
                screen.root,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
            )?
            .check()?;

            log!(x11 "creating raw window...");
            let raw = RawWindow::create(&conn, screen.root, area, title)?;

            let net_wm_pid =
                conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;

            conn.flush()?;

            let this = Self {
                root: screen.root,
                net_wm_pid,
                conn,
                raw,
                process_group: None,
                window: None,
                ready: false,
            };

            return Ok(this);
        }

        pub(crate) fn find_mapped_window(&self) -> Z<Option<Window>> {
            log!(x11 "finding window...");

            let group = if let Some(it) = self.process_group {
                it
            }
            else {
                log!(x11 "missing pid");
                return Ok(None);
            };

            let mut seen = HashSet::from([self.root]);
            let mut pending = VecDeque::from([self.root]);
            while let Some(parent) = pending.pop_front() {
                for window in self.conn.query_tree(parent)?.reply()?.children {
                    if !seen.insert(window) {
                        continue;
                    }

                    pending.push_back(window);
                    if self.matches_process_group(window, group)
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
            return self.conn.flush().map_err(|it| it.into());
        }

        pub(crate) fn is_mapped_window(
            &self,
            window: Window,
        ) -> Z<bool> {
            let Some(group) = self.process_group
            else {
                return Ok(false);
            };
            return Ok(self.matches_process_group(window, group)
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
        ) -> bool {
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
                .map(|pid| pid as libc::pid_t);

            pid.is_some_and(|pid| {
                dragons::pgid(pid).is_ok_and(|pgid| pgid == group)
            })
        }

        pub(crate) fn is_viewable(
            &self,
            window: Window,
        ) -> Z<bool> {
            Ok(self.conn.get_window_attributes(window)?.reply()?.map_state
                == MapState::VIEWABLE)
        }

        pub(crate) fn embed(
            &mut self,
            window: Window,
        ) -> Z {
            self.conn
                .change_window_attributes(
                    window,
                    &ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::STRUCTURE_NOTIFY),
                )?
                .check()?;
            self.conn
                .reparent_window(window, self.raw.window, 0, 0)?
                .check()?;
            self.ready = false;
            self.window = Some(window);
            self.resize(self.raw.area.width, self.raw.area.height)?;
            self.map_window()?;
            Ok(())
        }

        pub(crate) fn map_window(&self) -> Z {
            let Some(window) = self.window
            else {
                return Ok(());
            };

            self.conn.map_window(window)?.check()?;
            self.conn.flush()?;

            Ok(())
        }

        pub(crate) fn request_redraw(&self) -> Z {
            let Some(window) = self.window
            else {
                return Ok(());
            };

            self.conn.clear_area(true, window, 0, 0, 0, 0)?.check()?;
            return self.conn.flush().map_err(|it| it.into());
        }

        pub(crate) fn resize(
            &mut self,
            width: u16,
            height: u16,
        ) -> Z {
            let Some(window) = self.window
            else {
                return Ok(());
            };

            self.conn
                .configure_window(
                    window,
                    &ConfigureWindowAux::new()
                        .x(0)
                        .y(0)
                        .width(u32::from(width.max(1)))
                        .height(u32::from(height.max(1)))
                        .border_width(0),
                )?
                .check()?;
            self.conn.flush()?;
            Ok(())
        }

        pub(crate) fn focus_window(&mut self) -> Z<bool> {
            if !self.ready {
                return Ok(false);
            }

            let Some(window) = self.window
            else {
                return Ok(false);
            };

            if !self.is_viewable(window)? {
                self.ready = false;
                return Ok(false);
            }

            if self.conn.get_input_focus()?.reply()?.focus == window {
                return Ok(true);
            }

            self.conn
                .set_input_focus(
                    InputFocus::PARENT,
                    window,
                    x11rb::CURRENT_TIME,
                )?
                .check()?;

            self.conn.flush()?;

            Ok(true)
        }

        pub(crate) fn window(&self) -> X11Window<'_> {
            return X11Window {
                x11: self,
                window: &self.raw,
            };
        }

        pub(crate) fn owns(
            &self,
            window: Window,
        ) -> bool {
            self.root == window
        }

        pub(crate) fn conn_poll_fd(&self) -> BorrowedFd<'_> {
            return self.conn.as_fd();
        }

        pub(crate) fn poll(&self) -> Z<Option<Event>> {
            return self.conn.poll_for_event().map_err(MyError::X11Connection);
        }
    }

    pub(crate) struct X11Window<'a> {
        x11: &'a X11Host,
        window: &'a RawWindow,
    }

    impl X11Window<'_> {
        pub(crate) fn show(&self) -> Z {
            return self.window.show(&self.x11.conn);
        }

        pub(crate) fn hide(&self) -> Z {
            return self.window.hide(&self.x11.conn);
        }

        pub(crate) fn focus(&self) -> Z {
            return self.window.focus(&self.x11.conn);
        }

        pub(crate) fn destroy(&self) -> Z {
            return self.window.destroy(&self.x11.conn);
        }

        pub(crate) fn area(&self) -> Area {
            return self.window.area;
        }

        pub(crate) fn owns(
            &self,
            window: Window,
        ) -> bool {
            return self.window.owns(window);
        }
    }
}

mod app {
    use dbus::{
        ffidisp::{
            Connection as DBusConn,
            ConnectionItem,
            WatchEvent,
        },
        message::MatchRule,
    };
    use std::os::fd::AsRawFd;
    use std::{
        cmp::min,
        io,
        os::unix::process::CommandExt as _,
        process::Command,
        time::{
            Duration,
            Instant,
            SystemTime,
        },
    };
    use x11rb::protocol::Event;

    use crate::{
        cfg::{
            Area,
            Args,
            Behavior,
            MyError,
            Z,
        },
        dragons,
        x11::{
            EmbeddedProcess,
            X11Host,
        },
    };

    struct Discovery {
        deadline: Instant,
        delay: Duration,
    }

    pub(crate) struct Ctx {
        args: Args,
        x11: X11Host,
        dbus: DBusConn,
        signal_rule: MatchRule<'static>,
        showing: bool,
        focus_pending: bool,
        last_toggle: SystemTime,
        window_discovery: Option<Discovery>,
        embedded: Option<EmbeddedProcess>,
        closed: bool,
    }

    impl Ctx {
        const TOGGLE_COOL_DOWN_MILLIS: u128 = 40;
        const DISCOVERY_INITIAL_DELAY: Duration = Duration::from_millis(100);
        const DISCOVERY_MAX_DELAY: Duration = Duration::from_secs(1);

        pub(crate) fn open(args: Args) -> Z<Self> {
            log!(x11 "opening dbus connection");
            let dbus = DBusConn::new_session()?;

            log!(x11 "opening x11 connection");
            let x11 = X11Host::open(&args.title, |width, height| {
                Self::calc_area(&args, width, height)
            })?;

            let mut this = Self {
                signal_rule: MatchRule::new_signal(
                    &args.dbus_interface,
                    &args.dbus_member,
                )
                .with_path(&args.dbus_path)
                .static_clone(),

                showing: args.on_start == Behavior::Appear,
                focus_pending: args.on_start == Behavior::Appear,

                last_toggle: SystemTime::UNIX_EPOCH,
                window_discovery: None,
                embedded: None,
                closed: false,

                dbus,
                x11,
                args,
            };

            this.start()?;

            return Ok(this);
        }

        fn calc_area(
            args: &Args,
            screen_width: u16,
            screen_height: u16,
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
                width: dimension(&args.width, screen_width),
                height: dimension(&args.height, screen_height),
            };
        }

        fn start(&mut self) -> Z {
            self.dbus.add_match(&self.signal_rule.match_str())?;

            if self.showing {
                self.x11.window().show()?;
                self.x11.window().focus()?;
            }

            let mut it = Command::new(
                self.args.command.first().ok_or(MyError::NoCommand)?,
            );
            it.args(&self.args.command[1..]).process_group(0);
            dragons::pre_exec(&mut it);

            if let Some(dir) = self.args.working_dir.as_ref() {
                it.current_dir(dir);
            }

            let pid = it.spawn()?.id() as libc::pid_t;

            self.x11.process_group = Some(pid);
            self.embedded = Some(EmbeddedProcess { pid, gid: pid });

            self.schedule_discovery();

            Ok(())
        }

        fn reap_hosted(&mut self) -> Z {
            let Some(hosted) = self.embedded.as_ref()
            else {
                return Ok(());
            };

            let (pid, status) = dragons::waitpid(hosted.pid)?;

            if pid == hosted.pid {
                self.on_child_exit(status)?;
            }
            else if pid < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ECHILD) {
                    Err(err)?;
                }
            }

            return Ok(());
        }

        fn process_dbus_watch(
            &mut self,
            fd: libc::c_int,
            revents: libc::c_short,
        ) -> Z {
            let mut toggled = false;
            for item in self
                .dbus
                .watch_handle(fd, WatchEvent::from_revents(revents))
            {
                if let ConnectionItem::Signal(message) = item
                    && self.signal_rule.matches(&message)
                {
                    toggled = true;
                }
            }

            if toggled {
                self.toggle()?;
            }

            return Ok(());
        }

        pub(crate) fn run(&mut self) -> Z {
            while !self.closed {
                self.reap_hosted()?;
                if self.closed {
                    continue;
                }

                let watches = self.dbus.watch_fds();
                let mut fds = Vec::with_capacity(watches.len() + 1);
                fds.push(libc::pollfd {
                    fd: self.x11.conn_poll_fd().as_raw_fd(),
                    events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
                    revents: 0,
                });
                fds.extend(watches.iter().map(|watch| watch.to_pollfd()));

                let timeout = self.discovery_timeout();
                let timed_out =
                    dragons::poll(&mut fds, timeout, || !self.closed)?;
                if timed_out {
                    self.retry_discovery()?;
                    continue;
                }

                let x11_events = fds[0].revents;
                if x11_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
                    != 0
                {
                    log!(ctx@info "X11 connection closed");
                    self.quit()?;
                    continue;
                }

                if x11_events & libc::POLLIN != 0
                    && let Err(err) = self.process_x11_events()
                {
                    log!(ctx@fail "error processing X11 events: {}", err);
                }

                for (watch, pollfd) in watches.iter().zip(&fds[1..]) {
                    if pollfd.revents != 0
                        && let Err(err) =
                            self.process_dbus_watch(watch.fd(), pollfd.revents)
                    {
                        log!(ctx@warn "error processing dbus watch: {}", err);
                    }
                }
            }
            Ok(())
        }

        fn on_child_exit(
            &mut self,
            status: i32,
        ) -> Z {
            if let Some(hosted) = self.embedded.take() {
                log!(ctx@info
                    "hosted process exited status, pid={}, status={}",
                    hosted.pid,
                    status
                );
                dragons::try_kill(-hosted.gid);
            }
            return self.quit();
        }

        fn process_x11_events(&mut self) -> Z {
            while let Some(event) = self.x11.poll()? {
                self.process_x11_event(event)?;
            }
            return Ok(());
        }

        fn process_x11_event(
            &mut self,
            event: Event,
        ) -> Z {
            let window = self.x11.window;

            match event {
                Event::MapNotify(event)
                    if self.x11.owns(event.event)
                        && !self.x11.window().owns(event.window)
                        && window.is_none() =>
                {
                    self.attach_window(event.window)?;
                }

                Event::PropertyNotify(event)
                    if window.is_none()
                        && self.x11.is_window_pid_property(event.atom) =>
                {
                    self.attach_window(event.window)?;
                }

                Event::ReparentNotify(event)
                    if window == Some(event.window)
                        && !self.x11.window().owns(event.parent) =>
                {
                    log!(
                        ctx@warn "embedded window was reparented away: window={:#x}, parent={:#x}",
                        event.window,
                        event.parent
                    );
                    self.x11.window = None;
                    self.x11.ready = false;
                    self.focus_pending = self.showing;
                    self.schedule_discovery();
                }

                Event::MapNotify(event)
                    if self.x11.window().owns(event.window) =>
                {
                    self.sync_geometry()?;
                    self.x11.map_window()?;
                    self.update_readiness()?;
                }

                Event::UnmapNotify(event)
                    if self.x11.window().owns(event.window) =>
                {
                    self.focus_pending = false;
                }

                Event::FocusIn(event)
                    if self.x11.window().owns(event.event) =>
                {
                    if self.showing {
                        self.focus_pending = true;
                        self.update_readiness()?;
                    }
                }

                Event::ConfigureNotify(event)
                    if self.x11.window().owns(event.window) =>
                {
                    self.x11.window().area().width = event.width;
                    self.x11.window().area().height = event.height;
                    self.sync_geometry()?;
                }

                Event::MapNotify(event) if window == Some(event.window) => {
                    self.sync_geometry()?;
                    self.update_readiness()?;
                }

                Event::ConfigureNotify(event)
                    if window == Some(event.window) =>
                {
                    if event.width != self.x11.window().area().width
                        || event.height != self.x11.window().area().height
                    {
                        self.sync_geometry()?;
                    }
                }

                Event::UnmapNotify(event) if window == Some(event.window) => {
                    self.x11.ready = false;
                    if self.showing {
                        self.x11.map_window()?;
                        self.update_readiness()?;
                    }
                }

                Event::DestroyNotify(event) if window == Some(event.window) => {
                    log!(ctx@info
                        "embedded X11 window was destroyed: {:#x}",
                        event.window
                    );
                    self.x11.window = None;
                    self.schedule_discovery();
                }

                _ => {}
            }

            return Ok(());
        }

        fn sync_geometry(&mut self) -> Z {
            return self.x11.resize(
                self.x11.window().area().width,
                self.x11.window().area().height,
            );
        }

        fn schedule_discovery(&mut self) {
            if self.x11.window.is_none() && self.window_discovery.is_none() {
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
            if self.x11.window.is_none() {
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

            if let Some(window) = self.x11.find_mapped_window()? {
                self.embed(window)?;
            }

            return Ok(());
        }

        fn attach_window(
            &mut self,
            window: x11rb::protocol::xproto::Window,
        ) -> Z {
            self.x11.watch_window(window)?;
            if self.x11.is_mapped_window(window)? {
                self.embed(window)?;
            }
            return Ok(());
        }

        fn embed(
            &mut self,
            window: x11rb::protocol::xproto::Window,
        ) -> Z {
            self.x11.embed(window)?;
            self.window_discovery = None;
            if self.showing {
                self.x11.request_redraw()?;
            }
            return self.update_readiness();
        }

        fn update_readiness(&mut self) -> Z {
            let ready = self
                .x11
                .window
                .map(|window| self.x11.is_viewable(window))
                .transpose()?;

            self.x11.ready = match ready {
                None => return Ok(()),
                Some(it) => it,
            };

            if self.x11.ready {
                if !self.showing || !self.focus_pending {
                    log!(
                        ctx "ready but focus is not requested: showing={}, focus_pending={}",
                        self.showing,
                        self.focus_pending
                    );
                }
                else if self.x11.focus_window()? {
                    self.focus_pending = false;
                }
            }

            return Ok(());
        }

        fn toggle(&mut self) -> Z {
            if !self.mark_toggle_timestamp() {
                return Ok(());
            }

            self.showing = !self.showing;
            self.focus_pending = self.showing;

            if !self.showing {
                self.x11.window().hide()
            }
            else {
                self.x11.window().show()?;
                if self.x11.window.is_none() {
                    self.attach()?;
                }
                self.x11.map_window()?;
                self.x11.request_redraw()?;
                self.x11.window().focus()
            }
        }

        fn mark_toggle_timestamp(&mut self) -> bool {
            let now = SystemTime::now();
            let elapsed = now
                .duration_since(self.last_toggle)
                .unwrap_or(Duration::ZERO)
                .as_millis();

            if elapsed < Self::TOGGLE_COOL_DOWN_MILLIS {
                return false;
            }

            self.last_toggle = now;
            return true;
        }

        pub(crate) fn quit(&mut self) -> Z {
            if self.closed {
                return Ok(());
            }
            self.closed = true;

            if let Some(hosted) = self.embedded.take() {
                dragons::try_kill(-hosted.gid);
            }

            self.x11.window().destroy()?;

            return Ok(());
        }
    }
}

use crate::{
    app::Ctx,
    cfg::{
        Args,
        MyError,
        Z,
    },
};

fn main() -> Z {
    let it: Args = cfg::args_or_exit();
    cfg::DEBUG.store(it.verbose, std::sync::atomic::Ordering::SeqCst);

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

    log!(main "opening context...");
    let mut ctx = Ctx::open(it)?;

    log!(main "main loop reached");
    let result = ctx.run();

    log!(main "will quit");
    ctx.quit()?;

    log!(main "fin");
    match result.as_ref() {
        Ok(_) => log!(main "END: ok"),
        Err(err) => log!(main "END: failed: {}", err),
    }

    return result;
}
