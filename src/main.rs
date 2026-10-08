#![allow(clippy::needless_return)]

use std::collections::{
    HashMap,
    HashSet,
    VecDeque,
};
use std::fmt::{
    Display,
    Formatter,
};
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{
    AtomicBool,
    Ordering,
};
use std::sync::mpsc;
use std::thread;
use std::time::{
    Duration,
    SystemTime,
};

use clap::{
    Args as ClapArgs,
    Parser,
    Subcommand,
    ValueEnum,
};
use dbus::MethodErr;
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use dbus_crossroads::Crossroads;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    self,
    Atom,
    AtomEnum,
    ConfigureWindowAux,
    ConnectionExt as _,
    MapState,
    Screen,
    Window,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

use crate::poly::*;

const TOGGLE_COOL_DOWN_MILLIS: u128 = 40;
const LAUNCH_WAIT_MAX_MILLIS: u64 = 200;
const LAUNCH_WAIT_MILLIS: u64 = 5;
const LAUNCH_WAIT_RETRIES: u64 = LAUNCH_WAIT_MAX_MILLIS / LAUNCH_WAIT_MILLIS;
const LAUNCH_HIDE_RETRIES: u64 = LAUNCH_WAIT_RETRIES * 2;
const NAMESPACE: &str = "io.koosha.xpop";
const PATH: &str = "/io/koosha/xpop";
const METHOD: &str = "pop";

static LOOP: AtomicBool = AtomicBool::new(true);
static TRACE: AtomicBool = AtomicBool::new(false);
static DEBUG: AtomicBool = AtomicBool::new(false);

mod poly {
    /// polyfill for anything that makes working with rust easier.
    /// Plus a tiny log crate re-implementation.
    ///
    /// It provides 2 one-letter types:
    /// - Z<T> : An alias for Result<T, crate::MyError>
    /// - R<T> : A wrapper around the mouthful Arc<RefCell<T>>
    ///
    /// Additionally, a plain `Z` defaults to `Z<()>`.

    #[derive(Default, Debug)]
    pub struct R<T: Debug> {
        store: std::sync::Arc<std::cell::RefCell<T>>,
    }

    impl<T: Debug> R<T> {
        pub fn copy(&self) -> Self {
            return R {
                store: self.store.clone(),
            };
        }

        pub fn of(it: T) -> Self {
            return Self {
                store: std::sync::Arc::new(std::cell::RefCell::new(it)),
            };
        }

        pub fn get(&self) -> std::cell::Ref<'_, T> {
            return self.store.borrow();
        }
    }

    pub type Z<T = ()> = Result<T, crate::MyError>;

    // Good enough for xpop, no need for extra dependencies.
    #[clippy::format_args]
    macro_rules! log {
    ($whom:ident@info $fmt:literal $($arg:tt)*) => {{ log!([INFO, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident@warn $fmt:literal $($arg:tt)*) => {{ log!([WARN, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident@fail $fmt:literal $($arg:tt)*) => {{ log!([FAIL, $whom, $fmt], [$($arg)*]); }};
    ($whom:ident@trac $fmt:literal $($arg:tt)*) => {{
        if $crate::TRACE.load(::std::sync::atomic::Ordering::Relaxed) {
            log!([TRAC, $whom, $fmt], [$($arg)*]);
        }
    }};
    ($whom:ident $fmt:literal $($arg:tt)*) => {{
        if $crate::DEBUG.load(::std::sync::atomic::Ordering::Relaxed) {
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

    use std::fmt::Debug;

    pub(crate) use log;
}

#[derive(Debug, Copy, Clone, ValueEnum)]
enum Cmd {
    Show,
    Hide,
    Toggle,
}

impl AsRef<str> for Cmd {
    fn as_ref(&self) -> &str {
        return match self {
            Self::Show => "show",
            Self::Hide => "hide",
            Self::Toggle => "toggle",
        };
    }
}

#[derive(thiserror::Error, Debug)]
enum MyError {
    #[error("x11 connect error")]
    X11Connect(#[from] x11rb::errors::ConnectError),

    #[error("x11 connection error")]
    X11Connection(#[from] x11rb::errors::ConnectionError),

    #[error("x11 reply error: {0}")]
    X11Reply(#[from] x11rb::errors::ReplyError),

    #[error("dbus failure")]
    DBus(#[from] dbus::Error),

    #[error("no X11 screen")]
    NoScreen,

    #[error("invalid dbus member")]
    InvalidDBusMember,

    #[error("invalid arg: {0}")]
    InvalidArg(&'static str),

    #[error("missing window: {0}")]
    MissingWindow(String),

    #[error("window not ready to hide: {0}")]
    WindowNotReady(String),

    #[error("app launch error")]
    AppLaunch(#[from] std::io::Error),
}

#[derive(Debug, Copy, Clone)]
struct Area {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

#[derive(Default, Debug, Copy, Clone)]
struct CoordinatedArea {
    x: Coordinates,
    y: Coordinates,
    w: Coordinates,
    h: Coordinates,
}

impl CoordinatedArea {
    fn absolute(
        self,
        max_width: i32,
        max_height: i32,
    ) -> Area {
        return Area {
            x: self.x.absolute(max_width),
            y: self.y.absolute(max_height),
            w: self.w.length(max_width as u32),
            h: self.h.length(max_height as u32),
        };
    }
}

#[derive(Debug, Copy, Clone)]
enum Coordinates {
    Percentage(u8),
    Absolute(i32),
    Minimax,
}

impl Default for Coordinates {
    fn default() -> Self {
        return Self::Minimax;
    }
}

impl FromStr for Coordinates {
    type Err = MyError;

    fn from_str(it: &str) -> Result<Self, Self::Err> {
        if it.is_empty() {
            return Ok(Self::Minimax);
        }

        let this = match it.strip_suffix('%') {
            None => {
                if let Ok(v) = it.parse() {
                    Some(Coordinates::Absolute(v))
                }
                else {
                    None
                }
            }
            Some(it) => {
                if it.is_empty() {
                    Some(Coordinates::Percentage(100))
                }
                else {
                    if let Ok(v) = it.parse() {
                        Some(Coordinates::Percentage(v))
                    }
                    else {
                        None
                    }
                }
            }
        };

        return this.ok_or(MyError::InvalidArg("coordinate"));
    }
}

impl Coordinates {
    fn absolute(
        self,
        max: i32,
    ) -> i32 {
        match self {
            Self::Minimax => 0,
            Self::Percentage(it) => (max / 100) * (it as i32),
            Self::Absolute(it) => it,
        }
    }

    fn length(
        self,
        max: u32,
    ) -> u32 {
        match self {
            Self::Minimax => max,
            Self::Percentage(it) => (max / 100) * (it as u32),
            Self::Absolute(it) => it as u32,
        }
    }
}

impl CoordinatedArea {
    fn _do_parse(it: &[&str]) -> Z<CoordinatedArea> {
        let this = CoordinatedArea {
            x: Coordinates::from_str(it[0])?,
            y: Coordinates::from_str(it[1])?,
            w: Coordinates::from_str(it[2])?,
            h: Coordinates::from_str(it[3])?,
        };

        return Ok(this);
    }

    fn parse(it: &str) -> Z<CoordinatedArea> {
        let it = it.splitn(4, ',').collect::<Vec<_>>();
        if it.len() != 4 {
            return Err(MyError::InvalidArg("area"));
        }

        let area =
            Self::_do_parse(&it).map_err(|_| MyError::InvalidArg("area"))?;

        return Ok(area);
    }
}

#[derive(Debug, Clone)]
struct Atoms {
    motif_wm_hints: Atom,
    net_wm_desktop: Atom,
    net_wm_state: Atom,
    net_wm_state_above: Atom,
    net_wm_state_sticky: Atom,
    net_wm_state_skip_taskbar: Atom,
    net_wm_state_skip_pager: Atom,
    net_wm_name: Atom,
}

#[derive(Debug)]
struct WindowMan {
    conn: R<XCBConnection>,
    window_desc: String,
    area: CoordinatedArea,
    atoms: Atoms,
    last_toggle: SystemTime,
    screen: Screen,
}

impl WindowMan {
    fn apply_dropdown_hints(
        &self,
        x11: &XCBConnection,
        window: Window,
    ) -> Z {
        x11.change_property32(
            xproto::PropMode::REPLACE,
            window,
            self.atoms.motif_wm_hints,
            self.atoms.motif_wm_hints,
            &[
                2, 0, 0, 0, 0,
            ],
        )?
        .check()?;
        x11.change_property32(
            xproto::PropMode::REPLACE,
            window,
            self.atoms.net_wm_desktop,
            AtomEnum::CARDINAL,
            &[u32::MAX],
        )?
        .check()?;
        x11.change_property32(
            xproto::PropMode::REPLACE,
            window,
            self.atoms.net_wm_state,
            AtomEnum::ATOM,
            &[
                self.atoms.net_wm_state_above,
                self.atoms.net_wm_state_sticky,
                self.atoms.net_wm_state_skip_taskbar,
                self.atoms.net_wm_state_skip_pager,
            ],
        )?
        .check()?;
        return Ok(());
    }

    fn show(
        &self,
        window: Window,
    ) -> Z {
        let x11 = self.conn.get();

        self.apply_dropdown_hints(&x11, window)?;

        let area = self.area.absolute(
            self.screen.width_in_pixels as i32,
            self.screen.height_in_pixels as i32,
        );
        x11.configure_window(
            window,
            &ConfigureWindowAux::new()
                .x(area.x)
                .y(area.y)
                .height(area.h)
                .width(area.w),
        )?
        .check()?;

        x11.map_window(window)?.check()?;

        self.apply_dropdown_hints(&x11, window)?;

        x11.flush()?;
        Ok(())
    }

    fn hide(
        &self,
        window: Window,
    ) -> Z {
        let x11 = &self.conn.get();
        self.apply_dropdown_hints(x11, window)?;
        x11.unmap_window(window)?.check()?;
        self.apply_dropdown_hints(x11, window)?;
        x11.flush()?;
        return Ok(());
    }

    fn toggle(
        &mut self,
        window: Window,
    ) -> Z {
        let x11 = &self.conn.get();

        let now = SystemTime::now();
        let elapsed = now
            .duration_since(self.last_toggle)
            .unwrap_or(Duration::ZERO)
            .as_millis();
        if elapsed < TOGGLE_COOL_DOWN_MILLIS {
            log!(man "toggle signal too early, skipping");
            return Ok(());
        }

        self.last_toggle = now;

        return if x11.get_window_attributes(window)?.reply()?.map_state
            == MapState::UNMAPPED
        {
            self.show(window)
        }
        else {
            self.hide(window)
        };
    }

    fn find_window(&mut self) -> Z<Window> {
        let filter = WindowFilter::from_str(&self.window_desc)?;
        let x11 = self.conn.get();

        let mut seen = HashSet::from([self.screen.root]);
        let mut pending = VecDeque::from([self.screen.root]);

        while let Some(parent) = pending.pop_front() {
            let Ok(children) = x11.query_tree(parent)
            else {
                continue;
            };

            let Ok(children) = children.reply()
            else {
                continue;
            };

            for window in children.children {
                if !seen.insert(window) {
                    continue;
                }
                pending.push_back(window);

                if filter.matches_id(window) {
                    log!(
                        man "found window by id: filter={}, window={:#x}",
                        self.window_desc,
                        window
                    );
                    return Ok(window);
                }

                let win_prop = window_properties(
                    &x11,
                    self.atoms.net_wm_name,
                    window,
                    filter.needs_class(),
                    filter.needs_title(),
                )?;

                if let Some(win_prop) = win_prop
                    && filter.matches(window, &win_prop)
                {
                    log!(
                        man "found window: filter={}, window={:#x}",
                        self.window_desc,
                        window
                    );
                    return Ok(window);
                }
            }
        }

        return Err(MyError::MissingWindow(self.window_desc.clone()));
    }

    fn execute(
        &mut self,
        area: Option<CoordinatedArea>,
        mut cmd: Cmd,
        bin: &str,
        bins: &HashMap<String, PathBuf>,
    ) -> Z {
        if let Some(area) = area {
            self.area = area;
        }

        let window = match self.find_window() {
            Ok(it) => it,
            Err(MyError::MissingWindow(err)) => {
                log!(man@trac "checking if fallback is present: {}", err);
                let Some(path) = bins.get(bin)
                else {
                    log!(man@trac "no fallback: {} => {:?} <=> {}", err, bins, bin);
                    return Err(MyError::MissingWindow(err));
                };

                log!(man@info "launching app: bin={}, path={}", bin, path.display());
                let mut child = Command::new(path).spawn()?;

                // TODO store handle
                thread::spawn(move || {
                    let _ = child.wait();
                });

                let mut window = None;
                for _ in 0..LAUNCH_WAIT_RETRIES {
                    thread::sleep(Duration::from_millis(LAUNCH_WAIT_MILLIS));
                    match self.find_window() {
                        Ok(it) => {
                            window = Some(it);
                            break;
                        }
                        Err(MyError::MissingWindow(_)) => {}
                        Err(err) => return Err(err),
                    }
                }
                let window = window
                    .ok_or_else(|| MyError::MissingWindow(err.clone()))?;

                log!(man@trac "hiding newly launched window so its WM hints will properly apply: {}={}", bin, window);
                let x11 = self.conn.get();
                let mut ready_to_hide = false;
                for _ in 0..LAUNCH_HIDE_RETRIES {
                    if x11.get_window_attributes(window)?.reply()?.map_state
                        == MapState::VIEWABLE
                    {
                        ready_to_hide = true;
                        break;
                    }
                }
                if !ready_to_hide {
                    return Err(MyError::WindowNotReady(err));
                }
                self.hide(window)?;

                cmd = Cmd::Show;
                window
            }
            Err(err) => return Err(err),
        };

        return match cmd {
            Cmd::Show => self.show(window),
            Cmd::Hide => self.hide(window),
            Cmd::Toggle => self.toggle(window),
        };
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
enum ListProperty {
    Id,
    Class,
    Title,
}

#[derive(Debug, Clone, Default)]
struct WindowFilter {
    class: Option<String>,
    instance: Option<String>,
    title: Option<String>,
    id: Option<Window>,
    fallback: Option<String>,
}

impl Display for WindowFilter {
    fn fmt(
        &self,
        f: &mut Formatter<'_>,
    ) -> std::fmt::Result {
        let mut fields = Vec::with_capacity(5);
        if let Some(it) = self.id {
            fields.push(format!("id:0x{it:x}"));
        }
        if let Some(it) = self.class.as_deref() {
            fields.push(format!("class:{}", filter_escape(it)));
        }
        if let Some(it) = self.instance.as_deref() {
            fields.push(format!("instance:{}", filter_escape(it)));
        }
        if let Some(it) = self.title.as_deref() {
            fields.push(format!("title:{}", filter_escape(it)));
        }
        if let Some(it) = self.fallback.as_deref() {
            fields.push(format!("fallback:{}", filter_escape(it)));
        }
        write!(f, "{}", fields.join("&"))
    }
}

impl FromStr for WindowFilter {
    type Err = MyError;

    fn from_str(it: &str) -> Z<Self> {
        return match Self::_do_parse(it) {
            Ok(it) => Ok(it),
            Err(it) => {
                log!(win@warn "bad filter: {}", it);
                return Err(it);
            }
        };
    }
}

impl WindowFilter {
    fn _do_parse(it: &str) -> Z<Self> {
        let mut this = Self::default();
        for component in it.split('&') {
            let (key, value) = component
                .split_once(':')
                .ok_or(MyError::InvalidArg("window_filter"))?;
            let value = filter_unescape(value)?;
            let value = (!value.is_empty()).then_some(value);

            match key {
                "class" => this.class = value,
                "instance" => this.instance = value,
                "title" => this.title = value,
                "fallback" => {
                    if let Some(value) = value.as_deref() {
                        Args::parse_bin_name(value).map_err(|_| {
                            MyError::InvalidArg("window_filter_bad_fallback")
                        })?;
                    }
                    this.fallback = value;
                }
                "id" if this.id.is_none() => {
                    if let Some(value) = value {
                        this.id = Some(
                            match value.strip_prefix("0x") {
                                Some(value) => u32::from_str_radix(value, 16),
                                None => value.parse(),
                            }
                            .map_err(|_| {
                                MyError::InvalidArg("window_filter_bad_id")
                            })?,
                        );
                    }
                }
                _ => return Err(MyError::InvalidArg("window_filter_unknown")),
            }
        }

        if this.class.is_none()
            && this.instance.is_none()
            && this.title.is_none()
            && this.id.is_none()
        {
            return Err(MyError::InvalidArg("window_filter_empty"));
        }

        log!(filter@trac "parsed: {:?}", this);
        return Ok(this);
    }

    fn list_line(
        &self,
        properties: &[ListProperty],
    ) -> String {
        const ALL: [ListProperty; 3] = [
            ListProperty::Id,
            ListProperty::Class,
            ListProperty::Title,
        ];
        let properties = if properties.is_empty() {
            &ALL[..]
        }
        else {
            properties
        };

        let mut output = Vec::with_capacity(properties.len());
        for property in properties {
            if output.iter().any(|value: &String| {
                value.starts_with(match property {
                    ListProperty::Id => "id:",
                    ListProperty::Class => "class:",
                    ListProperty::Title => "title:",
                })
            }) {
                continue;
            }

            let (name, value) = match property {
                ListProperty::Id => (
                    "id",
                    self.id.map(|id| format!("0x{id:x}")).unwrap_or_default(),
                ),
                ListProperty::Class => (
                    "class",
                    filter_escape(self.class.as_deref().unwrap_or_default()),
                ),
                ListProperty::Title => (
                    "title",
                    filter_escape(self.title.as_deref().unwrap_or_default()),
                ),
            };
            output.push(format!("{name}:{value}"));
        }

        return output.join("&");
    }

    fn needs_class(&self) -> bool {
        return self.class.is_some() || self.instance.is_some();
    }

    fn needs_title(&self) -> bool {
        return self.title.is_some();
    }

    fn matches_id(
        &self,
        window: Window,
    ) -> bool {
        return self.id == Some(window);
    }

    fn matches(
        &self,
        window: Window,
        properties: &WindowProperties,
    ) -> bool {
        // List output always includes the stable XID. Prefer it when present:
        // titles can change after listing, and an ID is sufficient to select
        // the exact same client regardless of its changing metadata.
        if let Some(id) = self.id {
            return window == id;
        }

        let class_matches = self.class.as_deref().is_none_or(|filter| {
            properties
                .class
                .as_deref()
                .is_some_and(|class| class.eq_ignore_ascii_case(filter))
        });
        let instance_matches = self.instance.as_deref().is_none_or(|filter| {
            properties
                .instance
                .as_deref()
                .is_some_and(|instance| instance.eq_ignore_ascii_case(filter))
        });
        let title_matches = self.title.as_deref().is_none_or(|filter| {
            properties.title.as_deref().is_some_and(|title| {
                title.to_lowercase().contains(&filter.to_lowercase())
            })
        });

        return class_matches && instance_matches && title_matches;
    }
}

#[derive(Debug, Default)]
struct WindowProperties {
    instance: Option<String>,
    class: Option<String>,
    title: Option<String>,
}

fn filter_escape(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            output.push(char::from(byte));
        }
        else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    return output;
}

fn filter_unescape(value: &str) -> Z<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut value = value.bytes();
    while let Some(byte) = value.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }

        let high = value.next().ok_or(MyError::InvalidArg("window_filter"))?;
        let low = value.next().ok_or(MyError::InvalidArg("window_filter"))?;
        let high = char::from(high)
            .to_digit(16)
            .ok_or(MyError::InvalidArg("window_filter"))?;
        let low = char::from(low)
            .to_digit(16)
            .ok_or(MyError::InvalidArg("window_filter"))?;
        bytes.push((high * 16 + low) as u8);
    }

    return String::from_utf8(bytes)
        .map_err(|_| MyError::InvalidArg("window_filter"));
}

fn window_properties(
    x11: &XCBConnection,
    net_wm_name: Atom,
    window: Window,
    with_class: bool,
    with_title: bool,
) -> Z<Option<WindowProperties>> {
    fn property(
        x11: &XCBConnection,
        window: Window,
        atom: Atom,
    ) -> Option<Vec<u8>> {
        return x11
            .get_property(false, window, atom, AtomEnum::ANY, 0, u32::MAX)
            .ok()?
            .reply()
            .ok()
            .map(|reply| reply.value);
    }

    let (instance, class) = if with_class {
        let class = property(x11, window, AtomEnum::WM_CLASS.into())
            .unwrap_or_default();
        let mut class = class
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned());
        (class.next(), class.next())
    }
    else {
        (None, None)
    };

    let title = if with_title {
        property(x11, window, net_wm_name)
            .or_else(|| property(x11, window, AtomEnum::WM_NAME.into()))
            .map(|value| String::from_utf8_lossy(&value).into_owned())
            .filter(|value| !value.is_empty())
    }
    else {
        None
    };

    if instance.is_none() && class.is_none() && title.is_none() {
        return Ok(None);
    }

    return Ok(Some(WindowProperties {
        instance,
        class,
        title,
    }));
}

#[derive(Debug, Parser)]
#[command(author, version, about, subcommand_required = true)]
struct Args {
    #[arg(
        short = 'v',
        long,
        global = true,
        help_heading = "Global options",
        action = clap::ArgAction::Count
    )]
    verbose: u8,

    /// DBus namespace to run under or connect to.
    #[arg(
        short,
        long,
        global = true,
        help_heading = "Global options",
        value_parser = Self::parse_dbus_ns
    )]
    namespace: Option<String>,

    #[command(subcommand)]
    mode: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Run the D-Bus daemon.
    #[command(visible_alias = "d")]
    Daemon(DaemonArgs),

    /// Send a command to the daemon for an X11 window.
    #[command(visible_alias = "s")]
    Signal(SignalArgs),

    /// List reusable filters for X11 client windows.
    #[command(visible_alias = "l")]
    List(ListArgs),
}

#[derive(Debug, ClapArgs, Default)]
struct DaemonArgs {
    /// bin-to-path mapping in the form NAME=PATH.
    #[arg(short, long, value_name = "BIN=PATH")]
    bin: Vec<String>,
}

#[derive(Debug, ClapArgs)]
struct SignalArgs {
    /// Top-left x coordinate, in absolute pixels or percentage of screen width.
    #[arg(short)]
    x: Option<String>,

    /// Top-left y coordinate, in absolute pixels or percentage of screen height.
    #[arg(short)]
    y: Option<String>,

    /// Width, in absolute pixels or percentage of screen width.
    #[arg(short)]
    w: Option<String>,

    /// Height, in absolute pixels or percentage of screen height.
    #[arg(short)]
    r: Option<String>,

    #[command(subcommand)]
    command: SignalCommand,
}

#[derive(Debug, Subcommand)]
enum SignalCommand {
    Show(FilterArgs),
    Hide(FilterArgs),
    Toggle(FilterArgs),
}

#[derive(Debug, ClapArgs)]
struct FilterArgs {
    /// Window filter.
    #[arg(value_name = "FILTER", value_parser = WindowFilter::from_str)]
    filter: WindowFilter,
}

impl SignalCommand {
    fn into_parts(self) -> (Cmd, WindowFilter) {
        return match self {
            Self::Show(args) => (Cmd::Show, args.filter),
            Self::Hide(args) => (Cmd::Hide, args.filter),
            Self::Toggle(args) => (Cmd::Toggle, args.filter),
        };
    }
}

#[derive(Debug, ClapArgs)]
struct ListArgs {
    /// Add a property to each listed filter. May be passed multiple times.
    #[arg(short, long)]
    list: Vec<ListProperty>,
}

impl Args {
    fn parse_dbus_ns(it: &str) -> Z<String> {
        if it.is_empty()
            || it.len() > 255
            || it.as_bytes()[0].is_ascii_digit()
            || !it
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(MyError::InvalidDBusMember);
        }
        return Ok(it.to_owned());
    }

    fn dbus_namespace(&self) -> String {
        return format!(
            "{}.{}",
            NAMESPACE,
            self.namespace.as_deref().unwrap_or("default")
        );
    }

    fn parse_bin_name(it: &str) -> Z<()> {
        let mut chars = it.bytes();
        let Some(first) = chars.next()
        else {
            return Err(MyError::InvalidArg("bin"));
        };
        if !(first.is_ascii_lowercase() || first == b'_')
            || !chars.all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || byte == b'_'
            })
        {
            return Err(MyError::InvalidArg("bin"));
        }
        return Ok(());
    }
}

impl DaemonArgs {
    fn bin_mappings(&self) -> Z<HashMap<String, PathBuf>> {
        let mut bins = HashMap::with_capacity(self.bin.len());
        for bin in &self.bin {
            let (name, path) =
                bin.split_once('=').ok_or(MyError::InvalidArg("bin"))?;
            Args::parse_bin_name(name)?;

            let path = PathBuf::from(path);
            if path.as_os_str().is_empty()
                || !path.exists()
                || bins.insert(name.to_owned(), path).is_some()
            {
                return Err(MyError::InvalidArg("bin"));
            }
        }
        return Ok(bins);
    }
}

fn ekran_list(fields: &[ListProperty]) -> Z {
    let (x11, screen_index) = XCBConnection::connect(None)?;
    let root = x11
        .setup()
        .roots
        .get(screen_index)
        .ok_or(MyError::NoScreen)?
        .root;
    let net_wm_name = x11.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;

    let mut seen = HashSet::from([root]);
    let mut pending = VecDeque::from([root]);
    while let Some(parent) = pending.pop_front() {
        let Ok(children) = x11.query_tree(parent)
        else {
            continue;
        };
        let Ok(children) = children.reply()
        else {
            continue;
        };

        for window in children.children {
            if !seen.insert(window) {
                continue;
            }
            pending.push_back(window);

            let Some(properties) =
                window_properties(&x11, net_wm_name, window, true, true)?
            else {
                continue;
            };
            println!(
                "{}",
                WindowFilter {
                    id: Some(window),
                    class: properties.class,
                    instance: None,
                    title: properties.title,
                    fallback: None,
                }
                .list_line(fields)
            );
        }
    }

    return Ok(());
}

fn ekran_x11(
    rx: mpsc::Receiver<(String, Option<CoordinatedArea>, Cmd, String)>,
    bins: HashMap<String, PathBuf>,
) -> Z {
    log!(x11@trac "opening x11 connection");
    let (x11, screen_index) = XCBConnection::connect(None)?;

    log!(x11@trac "acquiring xcb screen...");
    let screen = x11
        .setup()
        .roots
        .get(screen_index)
        .ok_or(MyError::NoScreen)?
        .clone();

    let atom_intern = |name| -> Z<Atom> {
        return Ok(x11.intern_atom(false, name)?.reply()?.atom);
    };
    let atoms = Atoms {
        motif_wm_hints: atom_intern(b"_MOTIF_WM_HINTS")?,
        net_wm_desktop: atom_intern(b"_NET_WM_DESKTOP")?,
        net_wm_state: atom_intern(b"_NET_WM_STATE")?,
        net_wm_state_above: atom_intern(b"_NET_WM_STATE_ABOVE")?,
        net_wm_state_sticky: atom_intern(b"_NET_WM_STATE_STICKY")?,
        net_wm_state_skip_taskbar: atom_intern(b"_NET_WM_STATE_SKIP_TASKBAR")?,
        net_wm_state_skip_pager: atom_intern(b"_NET_WM_STATE_SKIP_PAGER")?,
        net_wm_name: atom_intern(b"_NET_WM_NAME")?,
    };

    x11.flush()?;
    let x11 = R::of(x11);

    let mut windows = HashMap::with_capacity(1);
    for (window_desc, area, cmd, bin) in rx {
        let man =
            windows
                .entry(window_desc)
                .or_insert_with_key(|window_desc| WindowMan {
                    conn: x11.copy(),
                    screen: screen.clone(),
                    area: area.unwrap_or_default(),
                    atoms: atoms.clone(),
                    window_desc: window_desc.clone(),
                    last_toggle: SystemTime::UNIX_EPOCH,
                });

        if let Err(err) = man.execute(area, cmd, &bin, &bins) {
            log!(x11@fail "failed: {:#?}", err);
        }

        if !LOOP.load(Ordering::Relaxed) {
            break;
        }
    }

    log!(x11@info "exiting...");
    return Ok(());
}

fn ekran_dbus(
    ns: String,
    tx: mpsc::Sender<(String, Option<CoordinatedArea>, Cmd, String)>,
) -> Z {
    log!(x11@trac "opening dbus connection");
    let c = Connection::new_session()?;
    c.request_name(ns.clone(), false, false, true)?;
    log!(
        dbus@info "owning D-Bus endpoint: name={}, path={}, interface={}",
        ns,
        PATH,
        ns
    );

    let mut cr = Crossroads::new();
    let token = cr.register(ns, |it| {
        const ARGS: (&str, &str, &str, &str) = ("window", "cmd", "area", "bin");

        it.method(
            METHOD,
            ARGS,
            (),
            move |_,
                  _,
                  (window, cmd, area, bin): (
                String,
                String,
                String,
                String,
            )| {
                log!(
                    dbus "message: window={}, area={}, cmd={}, bin={}",
                    window,
                    area,
                    cmd,
                    bin
                );

                let cmd = Cmd::from_str(cmd.as_ref(), false).map_err(|_| {
                    log!(dbus@warn "invalid command: {}", cmd);
                    MethodErr::invalid_arg("cmd")
                })?;

                let area = if area.is_empty() {
                    None
                }
                else {
                    Some(CoordinatedArea::parse(&area).map_err(|_| {
                        log!(dbus@warn "invalid area: area={}", area);
                        MethodErr::invalid_arg("area")
                    })?)
                };

                log!(dbus@trac "recv msg: {:?}", (&window, &area, &cmd, &bin));
                if let Err(err) = tx.send((window, area, cmd, bin)) {
                    log!(dbus "channel error: {}", err);
                    LOOP.store(false, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(20));
                    std::process::exit(1);
                }

                Ok(())
            },
        );
    });
    cr.insert(PATH, &[token], ());

    dbus::channel::MatchingReceiver::start_receive(
        &c,
        MatchRule::new_method_call(),
        Box::new(move |msg, conn| {
            cr.handle_message(msg, conn).unwrap();
            true
        }),
    );

    while LOOP.load(Ordering::Relaxed) {
        c.process(Duration::from_millis(1000))?;
    }

    log!(dbus@info "exiting...");
    return Ok(());
}

fn ekran_daemon(
    namespace: String,
    daemon: DaemonArgs,
) -> Z {
    let bins = daemon.bin_mappings()?;
    let (tx, rx) = mpsc::channel();

    let join_x11 = thread::spawn(move || ekran_x11(rx, bins));

    let dbus_result = ekran_dbus(namespace, tx);

    log!(main "signaling end");
    LOOP.store(false, Ordering::SeqCst);

    if let Err(err) = dbus_result {
        log!(main@fail "dbus error: {}", err);
    }
    if let Ok(x11_result) = join_x11.join() {
        if let Err(err) = x11_result {
            log!(main@fail "x11 error: {}", err);
        }
    }
    else {
        log!(main@fail "x11 thread join error");
    }

    return Ok(());
}

fn ekran_client(
    namespace: String,
    signal: SignalArgs,
) -> Z {
    fn normalize<T: FromStr + Display>(
        name: &'static str,
        v: Option<String>,
    ) -> Z<String> {
        let v = match v {
            None => return Ok("".to_string()),
            Some(v) => v,
        };

        return match v.strip_suffix('%').unwrap_or(&v).parse::<T>() {
            Ok(_) => Ok(v),
            Err(_) => Err(MyError::InvalidArg(name)),
        };
    }

    let area = format!(
        "{},{},{},{}",
        normalize::<i16>("x", signal.x)?,
        normalize::<i16>("y", signal.y)?,
        normalize::<u16>("w", signal.w)?,
        normalize::<u16>("h", signal.r)?
    );
    let (command, mut filter) = signal.command.into_parts();
    let command = command.as_ref().to_string();
    let bin = filter.fallback.take().unwrap_or_default();

    let dbus_args = (filter.to_string(), command, area, bin);

    log!(
        dbus@info "calling D-Bus endpoint: name={}, path={}, interface={}, args={:?}",
        namespace,
        PATH,
        namespace,
        dbus_args,
    );
    Connection::new_session()?
        .with_proxy(namespace.as_str(), PATH, Duration::from_secs(10))
        .method_call(namespace.as_str(), METHOD, dbus_args)
        .map_err(MyError::DBus)
}

fn main() -> Result<(), MyError> {
    let args = Args::parse();
    if args.verbose > 0 {
        DEBUG.store(true, Ordering::SeqCst);
    }
    if args.verbose > 1 {
        TRACE.store(true, Ordering::SeqCst);
    }
    log!(main "BEGIN");
    let namespace = args.dbus_namespace();

    return match args.mode {
        Mode::Daemon(daemon) => ekran_daemon(namespace, daemon),
        Mode::Signal(signal) => ekran_client(namespace, signal),
        Mode::List(list) => ekran_list(&list.list),
    };
}
