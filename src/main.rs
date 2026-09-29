#![allow(clippy::needless_return)]

use clap::Parser;

use dbus::{
    Message,
    ffidisp::{
        Connection as DBusConn,
        ConnectionItem,
        WatchEvent,
    },
    message::MatchRule,
};
use std::{
    cmp::min,
    collections::{
        HashSet,
        VecDeque,
    },
    io,
    os::{
        fd::AsRawFd as _,
        unix::process::CommandExt as _,
    },
    path::PathBuf,
    process::Command,
    sync::atomic::{
        AtomicBool,
        Ordering,
    },
    time::{
        Duration,
        SystemTime,
    },
};
use thiserror::Error;
use x11rb::errors::ReplyOrIdError;
use x11rb::protocol::xproto::{
    ConfigureWindowAux,
    CreateWindowAux,
    PropMode,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt;
use x11rb::{
    connection::Connection as _,
    errors::{
        ConnectError,
        ConnectionError,
        ReplyError,
    },
    protocol::{
        Event,
        xproto::{
            Atom,
            AtomEnum,
            ChangeWindowAttributesAux,
            ConnectionExt as _,
            EventMask,
            InputFocus,
            MapState,
            Window,
        },
    },
    xcb_ffi::XCBConnection,
};

const TOGGLE_COOL_DOWN_MILLIS: u128 = 40;
const CLIENT_DISCOVERY_INTERVAL_MILLIS: i32 = 25;
const APP: &str = "io.koosha.xpop";

static DEBUG: AtomicBool = AtomicBool::new(false);

extern "C" fn child_exited(_: libc::c_int) {}

fn install_child_exit_handler() -> Z {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = child_exited as *const () as usize;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[clippy::format_args]
macro_rules! log {
    (info, $fmt:literal $($arg:tt)*) => {{ log!(@, ["INFO"], [$fmt], [$($arg)*]); }};
    (warn, $fmt:literal $($arg:tt)*) => {{ log!(@, ["WARN"], [$fmt], [$($arg)*]); }};
    (fail, $fmt:literal $($arg:tt)*) => {{ log!(@, ["FAIL"], [$fmt], [$($arg)*]); }};
    ($fmt:literal $($arg:tt)*) => {{
        if DEBUG.load(Ordering::Relaxed) { log!(@, ["DEBG"], [$fmt], [$($arg)*]); }
    }};
    (@, [$level:literal], [$fmt:literal], [$($arg:tt)*]) => {{
        eprintln!(concat!("[", $level, "] ", $fmt) $($arg)*);
    }};
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, clap::ValueEnum)]
enum Behavior {
    Hide,
    Appear,
    None,
}

#[derive(clap::Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Send D-Bus signal to request visibility toggle and exit.
    #[arg(short, long, conflicts_with_all = ["command", "list_monitors"])]
    signal: bool,

    /// Print the X11 root screen size and exit.
    #[arg(long, conflicts_with_all = ["command", "signal"])]
    list_monitors: bool,

    #[arg(short, long)]
    verbose: bool,

    /// The XORG app and its arguments to host.
    #[arg(allow_hyphen_values = true, conflicts_with_all = ["list_monitors", "signal"], required_unless_present_any = ["signal", "list_monitors"], num_args = 1..)]
    command: Vec<String>,

    #[arg(short, long)]
    working_dir: Option<PathBuf>,

    #[arg(long, default_value_t = APP.to_string())]
    dbus_interface: String,

    #[arg(long, default_value_t = APP.split('.').last().unwrap().to_string())]
    dbus_member: String,

    #[arg(long, default_value_t = { let mut value = APP.replace('.', "/"); value.insert(0, '/'); value })]
    dbus_path: String,

    #[arg(long, default_value = "appear")]
    on_start: Behavior,

    #[arg(long, default_value = "main")]
    title: String,
    #[arg(short, default_value_t = 0)]
    x: u32,
    #[arg(short, default_value_t = 0)]
    y: u32,
    #[arg(long, default_value = "100%")]
    width: String,
    #[arg(long, default_value = "100%")]
    height: String,
}

#[derive(Debug, Copy, Clone)]
struct Area {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

impl Args {
    fn calc_area(
        &self,
        screen: &x11rb::protocol::xproto::Screen,
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

        let width = dimension(&self.width, screen.width_in_pixels);
        let height = dimension(&self.height, screen.height_in_pixels);
        Area {
            x: self.x.min(i16::MAX as u32) as i16,
            y: self.y.min(i16::MAX as u32) as i16,
            width,
            height,
        }
    }
}

#[derive(Error, Debug)]
enum MyError {
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

    #[error("no X11 screen")]
    NoScreen,

    #[error("io error")]
    Io(#[from] io::Error),
}

type Z<T = ()> = Result<T, MyError>;

struct HostedApp {
    pid: libc::pid_t,
    process_group: libc::pid_t,
}

struct EmbeddedClient {
    window: Window,
    ready: bool,
}

struct X11Host {
    conn: XCBConnection,
    root: Window,
    host: RawHostWindow,
    net_wm_pid: Atom,
    process_group: Option<libc::pid_t>,
    client: Option<EmbeddedClient>,
}

impl X11Host {
    fn new(
        area: Area,
        title: &str,
    ) -> Z<Self> {
        let (conn, screen_index) = XCBConnection::connect(None)?;
        let screen = conn
            .setup()
            .roots
            .get(screen_index)
            .ok_or(MyError::NoScreen)?;
        let root = screen.root;
        let mut host =
            RawHostWindow::create(&conn, root, area.width, area.height)?;
        host.configure(&conn, area.x, area.y, area.width, area.height)?;
        let utf8_string =
            conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        host.set_title(&conn, utf8_string, title)?;
        let net_wm_pid = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()?;
        conn.flush()?;

        Ok(Self {
            conn,
            root,
            host,
            net_wm_pid,
            process_group: None,
            client: None,
        })
    }

    fn find_mapped_client(&self) -> Option<Window> {
        let group = self.process_group?;
        let mut seen = HashSet::from([self.root]);
        let mut pending = VecDeque::from([self.root]);
        while let Some(parent) = pending.pop_front() {
            for window in
                self.conn.query_tree(parent).ok()?.reply().ok()?.children
            {
                if !seen.insert(window) {
                    continue;
                }
                pending.push_back(window);
                if self.matches_process_group(window, group)
                    && self.is_viewable(window).ok()?
                {
                    return Some(window);
                }
            }
        }
        None
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
        pid.is_some_and(|pid| unsafe { libc::getpgid(pid) == group })
    }

    fn is_viewable(
        &self,
        window: Window,
    ) -> Z<bool> {
        Ok(self.conn.get_window_attributes(window)?.reply()?.map_state
            == MapState::VIEWABLE)
    }

    fn embed(
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
            .reparent_window(window, self.host.window, 0, 0)?
            .check()?;
        self.client = Some(EmbeddedClient {
            window,
            ready: false,
        });
        self.resize_client(self.host.width, self.host.height)?;
        self.map_client()?;
        Ok(())
    }

    fn map_client(&self) -> Z {
        let Some(client) = self.client.as_ref()
        else {
            return Ok(());
        };
        self.conn.map_window(client.window)?.check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn resize_client(
        &mut self,
        width: u16,
        height: u16,
    ) -> Z {
        let Some(client) = self.client.as_ref()
        else {
            return Ok(());
        };
        self.conn
            .configure_window(
                client.window,
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

    fn focus_client(&mut self) -> Z<bool> {
        let Some(client) = self.client.as_ref().filter(|client| client.ready)
        else {
            return Ok(false);
        };
        if !self.is_viewable(client.window)? {
            self.client.as_mut().expect("client vanished").ready = false;
            return Ok(false);
        }
        if self.conn.get_input_focus()?.reply()?.focus == client.window {
            return Ok(true);
        }
        self.conn
            .set_input_focus(
                InputFocus::PARENT,
                client.window,
                x11rb::CURRENT_TIME,
            )?
            .check()?;
        self.conn.flush()?;
        Ok(true)
    }
}

struct Ctx {
    cfg: Args,
    x11: X11Host,
    dbus: DBusConn,
    signal_rule: MatchRule<'static>,
    showing: bool,
    focus_pending: bool,
    last_toggle: SystemTime,
    hosted: Option<HostedApp>,
    closed: bool,
}

impl Ctx {
    fn new(
        cfg: Args,
        dbus: DBusConn,
    ) -> Z<Self> {
        let area = {
            let (conn, screen) = XCBConnection::connect(None)?;
            cfg.calc_area(
                conn.setup().roots.get(screen).ok_or(MyError::NoScreen)?,
            )
        };
        let signal_rule =
            MatchRule::new_signal(&cfg.dbus_interface, &cfg.dbus_member)
                .with_path(&cfg.dbus_path)
                .static_clone();
        let x11 = X11Host::new(area, &cfg.title)?;
        Ok(Self {
            showing: cfg.on_start == Behavior::Appear,
            focus_pending: cfg.on_start == Behavior::Appear,
            cfg,
            x11,
            dbus,
            signal_rule,
            last_toggle: SystemTime::UNIX_EPOCH,
            hosted: None,
            closed: false,
        })
    }

    fn start(&mut self) -> Z {
        self.dbus.add_match(&self.signal_rule.match_str())?;

        if self.showing {
            self.x11.host.show(&self.x11.conn)?;
            self.x11.host.focus(&self.x11.conn)?;
        }

        let hosted = Self::launch_hosted(&self.cfg)?;
        self.x11.process_group = Some(hosted.process_group);
        self.hosted = Some(hosted);

        Ok(())
    }

    fn reap_hosted(&mut self) {
        let Some(hosted) = self.hosted.as_ref()
        else {
            return;
        };

        let mut status = 0;
        let pid =
            unsafe { libc::waitpid(hosted.pid, &mut status, libc::WNOHANG) };

        if pid == hosted.pid {
            self.on_child_exit(status);
        }
        else if pid < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ECHILD) {
                log!(
                    warn,
                    "could not reap hosted process {}: {}",
                    hosted.pid,
                    err
                );
            }
        }
    }

    fn process_dbus_watch(
        &mut self,
        fd: libc::c_int,
        revents: libc::c_short,
    ) {
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
            self.toggle();
        }
    }

    fn run(&mut self) -> Z {
        while !self.closed {
            self.reap_hosted();
            if self.closed {
                continue;
            }

            let watches = self.dbus.watch_fds();
            let mut fds = Vec::with_capacity(watches.len() + 1);
            fds.push(libc::pollfd {
                fd: self.x11.conn.as_raw_fd(),
                events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
                revents: 0,
            });
            fds.extend(watches.iter().map(|watch| watch.to_pollfd()));
            let discovery_pending =
                self.hosted.is_some() && self.x11.client.is_none();
            let timeout = if discovery_pending {
                CLIENT_DISCOVERY_INTERVAL_MILLIS
            }
            else {
                -1
            };
            let result = unsafe {
                libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout)
            };
            if result < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(err.into());
            }
            if result == 0 {
                self.attach_client();
                continue;
            }

            let x11_events = fds[0].revents;
            if x11_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
                != 0
            {
                log!(info, "X11 connection closed");
                self.quit();
                continue;
            }
            if x11_events & libc::POLLIN != 0 {
                self.process_x11_events();
            }
            for (watch, pollfd) in watches.iter().zip(&fds[1..]) {
                if pollfd.revents != 0 {
                    self.process_dbus_watch(watch.fd(), pollfd.revents);
                }
            }
        }
        Ok(())
    }

    fn on_child_exit(
        &mut self,
        status: i32,
    ) {
        if let Some(hosted) = self.hosted.take() {
            log!(
                info,
                "hosted process {} exited with status {}",
                hosted.pid,
                status
            );
            self.terminate_group(hosted.process_group);
        }
        self.quit();
    }

    fn process_x11_events(&mut self) {
        loop {
            let event = match self.x11.conn.poll_for_event() {
                Ok(Some(event)) => event,
                Ok(None) => return,
                Err(err) => {
                    log!(warn, "failed to read X11 event: {}", err);
                    self.quit();
                    return;
                }
            };
            self.process_x11_event(event);
        }
    }

    fn process_x11_event(
        &mut self,
        event: Event,
    ) {
        let client = self.x11.client.as_ref().map(|client| client.window);
        match event {
            Event::MapNotify(event)
                if event.event == self.x11.root && client.is_none() =>
            {
                self.attach_client()
            }
            Event::MapNotify(event) if self.x11.host.owns(event.window) => {
                self.on_host_mapped()
            }
            Event::UnmapNotify(event) if self.x11.host.owns(event.window) => {
                self.on_host_unmapped()
            }
            Event::FocusIn(event) if self.x11.host.owns(event.event) => {
                self.on_host_focus()
            }
            Event::ConfigureNotify(event)
                if self.x11.host.owns(event.window) =>
            {
                self.x11.host.width = event.width;
                self.x11.host.height = event.height;
                self.sync_client_geometry();
            }
            Event::MapNotify(event) if client == Some(event.window) => {
                self.sync_client_geometry();
                self.update_readiness()
            }
            Event::ConfigureNotify(event) if client == Some(event.window) => {
                if event.width != self.x11.host.width
                    || event.height != self.x11.host.height
                {
                    self.sync_client_geometry();
                }
            }
            Event::UnmapNotify(event) if client == Some(event.window) => {
                self.x11.client.as_mut().expect("client missing").ready = false;
            }
            Event::DestroyNotify(event) if client == Some(event.window) => {
                log!(
                    warn,
                    "embedded X11 client {:#x} was destroyed",
                    event.window
                );
                self.x11.client = None;
            }
            _ => {}
        }
    }

    fn sync_client_geometry(&mut self) {
        let width = self.x11.host.width;
        let height = self.x11.host.height;
        if let Err(err) = self.x11.resize_client(width, height) {
            log!(warn, "could not resize embedded client: {}", err);
        }
    }

    fn attach_client(&mut self) {
        let Some(client) = self.x11.find_mapped_client()
        else {
            return;
        };
        if let Err(err) = self.x11.embed(client) {
            log!(warn, "could not embed X11 client {:#x}: {}", client, err);
            return;
        }
        self.update_readiness();
    }

    fn on_host_mapped(&mut self) {
        self.sync_client_geometry();
        if let Err(err) = self.x11.map_client() {
            log!(warn, "could not map embedded client: {}", err);
        }
        self.update_readiness();
    }

    fn on_host_unmapped(&mut self) {
        self.focus_pending = false;
    }

    fn on_host_focus(&mut self) {
        if self.showing {
            self.focus_pending = true;
            self.update_readiness();
        }
    }

    fn update_readiness(&mut self) {
        let ready = self
            .x11
            .client
            .as_ref()
            .map(|client| client.window)
            .map(|window| self.x11.is_viewable(window));

        match ready {
            Some(Ok(true)) => {
                self.x11.client.as_mut().expect("client missing").ready = true;
                self.focus_if_ready();
            }
            Some(Ok(false)) => {
                self.x11.client.as_mut().expect("client missing").ready = false
            }
            Some(Err(err)) => {
                log!(warn, "could not inspect embedded client: {}", err)
            }
            None => {}
        }
    }

    fn focus_if_ready(&mut self) {
        if !self.showing || !self.focus_pending {
            log!(
                "not ready yet, cannot focus, showing={}, focus_pending={}",
                self.showing,
                self.focus_pending
            );
            return;
        }

        match self.x11.focus_client() {
            Ok(true) => self.focus_pending = false,
            Ok(false) => {}
            Err(err) => log!(warn, "could not focus embedded client: {}", err),
        }
    }

    fn toggle(&mut self) {
        let now = SystemTime::now();
        let elapsed = now
            .duration_since(self.last_toggle)
            .unwrap_or(Duration::ZERO)
            .as_millis();
        if elapsed < TOGGLE_COOL_DOWN_MILLIS {
            return;
        }
        self.last_toggle = now;

        let result: Z = if self.showing {
            self.showing = false;
            self.focus_pending = false;
            self.x11.host.hide(&self.x11.conn)
        }
        else {
            self.showing = true;
            self.focus_pending = true;
            (|| {
                self.x11.host.show(&self.x11.conn)?;
                self.x11.map_client()?;
                self.x11.host.focus(&self.x11.conn)?;
                Ok(())
            })()
        };

        if let Err(err) = result {
            log!(warn, "could not toggle raw X11 host: {}", err);
        }
    }

    fn terminate_group(
        &self,
        group: libc::pid_t,
    ) {
        if unsafe { libc::kill(-group, libc::SIGTERM) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                log!(
                    warn,
                    "could not terminate hosted process group {}: {}",
                    group,
                    err
                );
            }
        }
    }

    fn quit(&mut self) {
        if self.closed {
            return;
        }

        self.closed = true;

        if let Some(hosted) = self.hosted.take() {
            self.terminate_group(hosted.process_group);
        }

        if let Err(err) = self.x11.host.destroy(&self.x11.conn) {
            log!(warn, "could not destroy raw X11 host: {}", err);
        }
    }

    fn launch_hosted(cfg: &Args) -> Z<HostedApp> {
        let mut command =
            Command::new(cfg.command.first().expect("missing command"));
        command.args(&cfg.command[1..]).process_group(0);
        if let Some(dir) = cfg.working_dir.as_ref() {
            command.current_dir(dir);
        }
        unsafe {
            command.pre_exec(|| {
                let parent = libc::getppid();
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let pid = child.id() as libc::pid_t;
        Ok(HostedApp {
            pid,
            process_group: pid,
        })
    }
}

struct RawHostWindow {
    window: Window,
    width: u16,
    height: u16,
}

impl RawHostWindow {
    fn create(
        conn: &XCBConnection,
        root: Window,
        width: u16,
        height: u16,
    ) -> Z<Self> {
        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_FROM_PARENT as u8,
            window,
            root,
            0,
            0,
            width.max(1),
            height.max(1),
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

        conn.flush()?;

        Ok(Self {
            window,
            width: width.max(1),
            height: height.max(1),
        })
    }

    fn show(
        &self,
        conn: &XCBConnection,
    ) -> Z<()> {
        conn.map_window(self.window)?.check()?;
        return Self::flush(conn);
    }

    fn hide(
        &self,
        conn: &XCBConnection,
    ) -> Z<()> {
        conn.unmap_window(self.window)?.check()?;
        return Self::flush(conn);
    }

    fn configure(
        &mut self,
        conn: &XCBConnection,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> Z<()> {
        self.width = width.max(1);
        self.height = height.max(1);
        conn.configure_window(
            self.window,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(self.width))
                .height(u32::from(self.height)),
        )?
        .check()?;
        return Self::flush(conn);
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
    ) -> Z<()> {
        conn.set_input_focus(
            InputFocus::PARENT,
            self.window,
            x11rb::CURRENT_TIME,
        )?
        .check()?;
        return Self::flush(conn);
    }

    fn set_title(
        &self,
        conn: &XCBConnection,
        utf8_string: Atom,
        title: &str,
    ) -> Z<()> {
        conn.change_property8(
            PropMode::REPLACE,
            self.window,
            AtomEnum::WM_NAME,
            utf8_string,
            title.as_bytes(),
        )?
        .check()?;
        return Self::flush(conn);
    }

    fn destroy(
        &self,
        conn: &XCBConnection,
    ) -> Z<()> {
        conn.destroy_window(self.window)?.check()?;
        return Self::flush(conn);
    }

    fn flush(conn: &XCBConnection) -> Z<()> {
        conn.flush()?;
        return Ok(());
    }
}

fn main() -> Z {
    let args = Args::parse();
    DEBUG.store(args.verbose, Ordering::Relaxed);
    if args.signal {
        return DBusConn::new_session()?
            .send(
                Message::new_signal(
                    &args.dbus_path,
                    &args.dbus_interface,
                    &args.dbus_member,
                )
                .map_err(MyError::DBusSignal)?,
            )
            .map(|_| ())
            .map_err(|_| MyError::DBusSignal("could not send signal".into()));
    }
    if args.list_monitors {
        let (conn, screen) = XCBConnection::connect(None)?;
        let screen = conn.setup().roots.get(screen).ok_or(MyError::NoScreen)?;
        println!("0 - {}x{}", screen.width_in_pixels, screen.height_in_pixels);
        return Ok(());
    }

    install_child_exit_handler()?;
    let dbus = DBusConn::new_session()?;
    let mut ctx = Ctx::new(args, dbus)?;
    ctx.start()?;
    let result = ctx.run();
    ctx.quit();
    result
}
