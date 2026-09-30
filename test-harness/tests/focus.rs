//! In-memory focus/ownership and exit-policy replay using current production methods.
//! Child processes are native; X11 state is modeled without an X server or D-Bus daemon.
use std::{
    cell::RefCell,
    cmp::min,
    collections::{
        HashSet,
        VecDeque,
    },
    fs,
    io,
    os::unix::process::CommandExt as _,
    path::PathBuf,
    process::Command,
    rc::Rc,
    time::{
        Duration,
        Instant,
        SystemTime,
    },
};
use x11rb::protocol::{
    Event,
    xproto::{
        self,
        *,
    },
};
type Z<T = ()> = Result<T, Box<dyn std::error::Error>>;
macro_rules! log {
    ($scope:ident $( @ $level:ident )? $fmt:literal $($args:tt)*) => {
        eprintln!($fmt $($args)*);
    };
}
const ROOT: Window = 0x10;
const HOST: Window = 0x100;
const CLIENT: Window = 0x200;
const OUTSIDE: Window = 0x300;
struct R<T>(Rc<RefCell<T>>);
impl<T> Clone for R<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> R<T> {
    fn of(v: T) -> Self {
        Self(Rc::new(RefCell::new(v)))
    }
    fn get(&self) -> std::cell::Ref<'_, T> {
        self.0.borrow()
    }
    fn write(
        &self,
        v: T,
    ) {
        *self.0.borrow_mut() = v;
    }
}
impl<T: Copy> R<T> {
    fn read(&self) -> T {
        *self.0.borrow()
    }
}
struct O<T>(RefCell<Option<T>>);
impl<T> O<T> {
    fn read(&self) -> std::cell::Ref<'_, Option<T>> {
        self.0.borrow()
    }
    fn write(
        &self,
        v: T,
    ) {
        *self.0.borrow_mut() = Some(v);
    }
    fn clear(&self) {
        *self.0.borrow_mut() = None;
    }
    fn is_present(&self) -> bool {
        self.0.borrow().is_some()
    }
}
#[derive(Clone, Copy)]
#[allow(dead_code)] // Coordinate fields are required by the real ConfigureNotify handler.
struct Area {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}
fn focus_in(mode: NotifyMode) -> Event {
    Event::FocusIn(FocusInEvent {
        response_type: FOCUS_IN_EVENT,
        event: HOST,
        mode,
        detail: NotifyDetail::NONLINEAR,
        ..Default::default()
    })
}
fn mapped(
    window: Window,
    parent: Window,
) -> Event {
    Event::MapNotify(MapNotifyEvent {
        response_type: MAP_NOTIFY_EVENT,
        event: parent,
        window,
        ..Default::default()
    })
}
fn reparented(parent: Window) -> Event {
    Event::ReparentNotify(ReparentNotifyEvent {
        response_type: REPARENT_NOTIFY_EVENT,
        event: CLIENT,
        window: CLIENT,
        parent,
        ..Default::default()
    })
}
// In-memory event/ownership model; no X connection, X server, or D-Bus daemon.
struct State {
    focus: Window,
    revert_to: InputFocus,
    parent: Window,
    client_mapped: bool,
    client_available: bool,
    client_group: libc::pid_t,
    host_mapped: bool,
    host_destroyed: bool,
    events: VecDeque<Event>,
    focus_requests: Vec<Window>,
    delivered: usize,
    fail_focus: bool,
    detached_resizes: usize,
}
impl State {
    // XSetInputFocus: losing a viewable focus window applies revert_to.
    // RevertToParent also resets the next reversion to RevertToNone.
    // https://www.x.org/archive/current/doc/man/man3/XSetInputFocus.3.xhtml
    fn revert_focus(&mut self) {
        self.focus = match self.revert_to {
            InputFocus::PARENT => {
                self.revert_to = InputFocus::NONE;
                if self.focus == CLIENT
                    && self.parent == HOST
                    && self.host_mapped
                {
                    HOST
                }
                else {
                    ROOT
                }
            }
            InputFocus::POINTER_ROOT => u32::from(InputFocus::POINTER_ROOT),
            InputFocus::NONE => x11rb::NONE,
            _ => panic!("unsupported core focus reversion"),
        };
        if self.focus == HOST {
            self.events.push_back(focus_in(NotifyMode::NORMAL));
        }
    }
}
struct Reply<T>(T);
impl<T> Reply<T> {
    fn reply(self) -> Z<T> {
        Ok(self.0)
    }
}
struct Checked(bool);
impl Checked {
    fn check(self) -> Z {
        if self.0 {
            Err(io::Error::other("focus rejected").into())
        }
        else {
            Ok(())
        }
    }
}
type FocusReply = GetInputFocusReply;
struct TreeReply {
    parent: Window,
    children: Vec<Window>,
}
struct Conn {
    state: Rc<RefCell<State>>,
}
impl Conn {
    fn get_input_focus(&self) -> Z<Reply<FocusReply>> {
        Ok(Reply(FocusReply {
            focus: self.state.borrow().focus,
            revert_to: self.state.borrow().revert_to,
            ..Default::default()
        }))
    }
    fn query_tree(
        &self,
        window: Window,
    ) -> Z<Reply<TreeReply>> {
        Ok(Reply(TreeReply {
            parent: if window == CLIENT {
                self.state.borrow().parent
            }
            else if window == CLIENT + 1 {
                CLIENT
            }
            else {
                ROOT
            },
            children: {
                let s = self.state.borrow();
                let mut children = Vec::new();
                if window == ROOT {
                    children.push(HOST);
                }
                if window == s.parent && s.client_available {
                    children.push(CLIENT);
                }
                children
            },
        }))
    }
    fn get_window_attributes(
        &self,
        window: Window,
    ) -> Z<Reply<GetWindowAttributesReply>> {
        let s = self.state.borrow();
        let map_state = if window == CLIENT {
            if !s.client_available {
                return Err(io::Error::other("client window is missing").into());
            }
            if !s.client_mapped {
                MapState::UNMAPPED
            }
            else if s.parent == HOST && !s.host_mapped {
                MapState::UNVIEWABLE
            }
            else {
                MapState::VIEWABLE
            }
        }
        else if window == HOST && !s.host_mapped {
            MapState::UNMAPPED
        }
        else {
            MapState::VIEWABLE
        };
        Ok(Reply(GetWindowAttributesReply {
            map_state,
            ..Default::default()
        }))
    }
    fn set_input_focus(
        &self,
        revert_to: InputFocus,
        window: Window,
        _: u32,
    ) -> Z<Checked> {
        let mut s = self.state.borrow_mut();
        if s.fail_focus {
            return Ok(Checked(true));
        }
        s.focus_requests.push(window);
        s.revert_to = revert_to;
        let old = s.focus;
        if old == window {
            return Ok(Checked(false));
        }
        s.focus = window;
        if old == HOST {
            s.events.push_back(Event::FocusOut(FocusInEvent {
                response_type: FOCUS_OUT_EVENT,
                event: HOST,
                mode: NotifyMode::NORMAL,
                detail: NotifyDetail::INFERIOR,
                ..Default::default()
            }));
        }
        if window == HOST
            || (window == CLIENT && s.parent == HOST && old != HOST)
        {
            s.events.push_back(focus_in(NotifyMode::NORMAL));
        }
        Ok(Checked(false))
    }
    fn flush(&self) -> Z {
        Ok(())
    }
    fn unmap_window(
        &self,
        window: Window,
    ) -> Z<Checked> {
        self.set_mapped(window, false);
        Ok(Checked(false))
    }
    fn set_mapped(
        &self,
        window: Window,
        value: bool,
    ) {
        let mut s = self.state.borrow_mut();
        let old = if window == HOST {
            s.host_mapped
        }
        else {
            s.client_mapped
        };
        if old == value {
            return;
        }
        if window == HOST {
            s.host_mapped = value;
        }
        else {
            s.client_mapped = value;
        }
        let parent = if window == HOST { ROOT } else { s.parent };
        let event = if value {
            mapped(window, parent)
        }
        else {
            Event::UnmapNotify(UnmapNotifyEvent {
                response_type: UNMAP_NOTIFY_EVENT,
                event: window,
                window,
                ..Default::default()
            })
        };
        s.events.push_back(event);
        if !value
            && (s.focus == window
                || (window == CLIENT && s.focus == CLIENT + 1)
                || (window == HOST
                    && (s.focus == CLIENT || s.focus == CLIENT + 1)
                    && s.parent == HOST))
        {
            s.revert_focus();
        }
    }
}
struct X11Host {
    conn: Conn,
    root_win: Window,
}
struct GrabGuard<'a> {
    _host: &'a X11Host,
}
impl X11Host {
    fn matches_process_group(
        &self,
        window: Window,
        group: libc::pid_t,
    ) -> Z<bool> {
        let s = self.conn.state.borrow();
        Ok(window == CLIENT && s.client_available && s.client_group == group)
    }
    fn watch_window(
        &self,
        _: Window,
    ) -> Z {
        Ok(())
    }
    fn poll(&self) -> Z<Option<Event>> {
        let mut s = self.conn.state.borrow_mut();
        if s.events.is_empty() {
            return Ok(None);
        }
        s.delivered += 1;
        // Replay safety bound only: production event draining has no cap.
        if s.delivered > 64 {
            return Err(io::Error::other(
                "self-generated event stream did not quiesce within 64 events",
            )
            .into());
        }
        Ok(s.events.pop_front())
    }
    fn root_is(
        &self,
        window: Window,
    ) -> bool {
        window == ROOT
    }
    fn is_window_pid_property(
        &self,
        _: Atom,
    ) -> bool {
        false
    }
    fn grab_server(&self) -> Z<GrabGuard<'_>> {
        Ok(GrabGuard { _host: self })
    }
    fn flush(&self) -> Z {
        self.conn.flush()
    }
}
struct EmbeddedWindowMan {
    x11: R<X11Host>,
    ready: R<bool>,
    embedded_win: O<Window>,
}
impl EmbeddedWindowMan {
    fn is(
        &self,
        window: Window,
    ) -> bool {
        self.embedded_win.read().is_some_and(|it| it == window)
    }
    fn is_present(&self) -> bool {
        self.embedded_win.is_present()
    }
    fn is_viewable(&self) -> Z<bool> {
        if !self.is_present() {
            return Ok(false);
        }
        self.x11.get().is_viewable(CLIENT)
    }
    fn write(
        &self,
        window: Window,
    ) {
        self.embedded_win.write(window);
    }
    fn clear(&self) {
        self.embedded_win.clear();
    }
    fn resize(
        &self,
        _: Area,
    ) -> Z {
        if self.is_present() {
            let x11 = self.x11.get();
            let mut s = x11.conn.state.borrow_mut();
            if s.parent != HOST {
                s.detached_resizes += 1;
            }
        }
        Ok(())
    }
    fn map_window(&self) -> Z {
        if self.is_present() {
            self.x11.get().conn.set_mapped(CLIENT, true);
        }
        Ok(())
    }
    fn hide(&self) -> Z {
        if self.is_present() {
            self.x11.get().conn.set_mapped(CLIENT, false);
        }
        Ok(())
    }
}
struct HostWindowMan {
    x11: R<X11Host>,
    window: Window,
    area: Area,
    ready: R<bool>,
}
impl HostWindowMan {
    fn is(
        &self,
        window: Window,
    ) -> bool {
        self.window == window
    }
    fn area(&self) -> &Area {
        &self.area
    }
    fn set_host_area(
        &mut self,
        area: Area,
    ) {
        self.area = area;
    }
    fn show(&self) -> Z {
        self.x11.get().conn.set_mapped(HOST, true);
        Ok(())
    }
    fn destroy(&self) -> Z {
        self.hide()?;
        self.x11.get().conn.state.borrow_mut().host_destroyed = true;
        Ok(())
    }
    fn embed(
        &self,
        _: Window,
    ) -> Z {
        let x11 = self.x11.get();
        x11.conn.set_mapped(CLIENT, false);
        let mut s = x11.conn.state.borrow_mut();
        s.parent = HOST;
        s.events.push_back(reparented(HOST));
        self.ready.write(false);
        Ok(())
    }
}
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Behavior {
    Hide,
    Appear,
    None,
}
struct Args {
    command: Vec<String>,
    working_dir: Option<PathBuf>,
    on_exit: Behavior,
}
#[derive(Debug)]
enum MyError {
    NoCommand,
}
impl std::fmt::Display for MyError {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.write_str("no command")
    }
}
impl std::error::Error for MyError {}
struct Ctx {
    args: Args,
    process_group: Option<libc::pid_t>,
    process: Option<EmbeddedProcess>,
    relaunch_at: Option<Instant>,
    closed: bool,
    x11: R<X11Host>,
    host: HostWindowMan,
    embedded: EmbeddedWindowMan,
    ready: R<bool>,
    showing: bool,
    focus_pending: Option<Window>,
    last_toggle: SystemTime,
    window_discovery: Option<Discovery>,
}
impl Drop for Ctx {
    fn drop(&mut self) {
        if let Some(hosted) = self.process.as_mut() {
            let _ = hosted.child.kill();
            let _ = hosted.child.wait();
        }
    }
}
fn make_ctx() -> (Ctx, Rc<RefCell<State>>) {
    let state = Rc::new(RefCell::new(State {
        focus: OUTSIDE,
        revert_to: InputFocus::NONE,
        parent: HOST,
        client_mapped: true,
        client_available: true,
        client_group: 42,
        host_mapped: true,
        host_destroyed: false,
        events: VecDeque::new(),
        focus_requests: vec![],
        delivered: 0,
        fail_focus: false,
        detached_resizes: 0,
    }));
    let x11 = R::of(X11Host {
        root_win: ROOT,
        conn: Conn {
            state: state.clone(),
        },
    });
    let ready = R::of(false);
    let ctx = Ctx {
        args: Args {
            command: vec![],
            working_dir: None,
            on_exit: Behavior::None,
        },
        process_group: Some(42),
        process: None,
        relaunch_at: None,
        closed: false,
        host: HostWindowMan {
            x11: x11.clone(),
            window: HOST,
            area: Area {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
            ready: ready.clone(),
        },
        embedded: EmbeddedWindowMan {
            x11: x11.clone(),
            ready: ready.clone(),
            embedded_win: O(RefCell::new(Some(CLIENT))),
        },
        x11,
        ready,
        showing: true,
        focus_pending: Some(OUTSIDE),
        last_toggle: SystemTime::UNIX_EPOCH,
        window_discovery: None,
    };
    (ctx, state)
}
fn scenario(
    name: &str,
    failures: &mut usize,
    run: impl FnOnce() -> Z<bool>,
) {
    match run() {
        Ok(true) => println!("PASS: {name}"),
        Ok(false) => {
            *failures += 1;
            println!("FAIL: {name}: wrong focus/ownership state");
        }
        Err(e) => {
            *failures += 1;
            println!("FAIL: {name}: {e}");
        }
    }
}
#[test]
fn focus_events_converge_and_preserve_user_focus() -> Z {
    let mut failures = 0;
    scenario(
        "initial handoff focuses the client once and the generated events quiesce",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.update_readiness()?;
            ctx.process_x11_events()?;
            let s = state.borrow();
            Ok(s.focus == CLIENT
                && s.focus_requests == [CLIENT]
                && s.events.is_empty()
                && ctx.focus_pending.is_none())
        },
    );
    scenario(
        "a completed focus request is not replayed by map notifications",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = None;
            state.borrow_mut().focus = CLIENT;
            state.borrow_mut().events.push_back(mapped(CLIENT, HOST));
            ctx.process_x11_events()?;
            Ok(state.borrow().focus == CLIENT
                && state.borrow().focus_requests.is_empty()
                && ctx.focus_pending.is_none())
        },
    );
    scenario(
        "an actual host focus is forwarded to the client once",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = None;
            state.borrow_mut().focus = HOST;
            state
                .borrow_mut()
                .events
                .push_back(focus_in(NotifyMode::NORMAL));
            ctx.process_x11_events()?;
            Ok(state.borrow().focus == CLIENT
                && state.borrow().focus_requests == [CLIENT]
                && ctx.focus_pending.is_none())
        },
    );
    for (name, focus, mode) in [
        (
            "stale host notification does not steal focus from another application",
            OUTSIDE,
            NotifyMode::NORMAL,
        ),
        (
            "ancestor notification preserves a focused descendant",
            CLIENT + 1,
            NotifyMode::NORMAL,
        ),
        (
            "keyboard-grab notification does not retarget focus",
            HOST,
            NotifyMode::GRAB,
        ),
    ] {
        scenario(name, &mut failures, || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = None;
            state.borrow_mut().focus = focus;
            state.borrow_mut().events.push_back(focus_in(mode));
            ctx.process_x11_events()?;
            Ok(state.borrow().focus == focus
                && state.borrow().focus_requests.is_empty()
                && ctx.focus_pending.is_none())
        });
    }
    scenario(
        "hide/show rearms focus despite queued old unmap notifications",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = None;
            state.borrow_mut().focus = CLIENT;
            ctx.toggle()?;
            ctx.last_toggle = SystemTime::UNIX_EPOCH;
            ctx.toggle()?;
            ctx.process_x11_events()?;
            Ok(ctx.showing
                && ctx.ready.read()
                && ctx.focus_pending.is_none()
                && state.borrow().focus == CLIENT)
        },
    );
    scenario(
        "unviewable client retains focus intent until mapping completes",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            state.borrow_mut().client_mapped = false;
            ctx.update_readiness()?;
            if ctx.ready.read()
                || ctx.focus_pending.is_none()
                || !state.borrow().focus_requests.is_empty()
            {
                return Ok(false);
            }
            ctx.embedded.map_window()?;
            ctx.process_x11_events()?;
            Ok(ctx.ready.read()
                && ctx.focus_pending.is_none()
                && state.borrow().focus == CLIENT)
        },
    );
    scenario(
        "focus failure does not consume the outstanding focus request",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            state.borrow_mut().fail_focus = true;
            if ctx.update_readiness().is_ok() || ctx.focus_pending.is_none() {
                return Ok(false);
            }
            state.borrow_mut().fail_focus = false;
            ctx.update_readiness()?;
            ctx.process_x11_events()?;
            Ok(ctx.focus_pending.is_none() && state.borrow().focus == CLIENT)
        },
    );
    scenario(
        "confirmed detach invalidates ownership and existing discovery can reattach",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = None;
            ctx.ready.write(true);
            state.borrow_mut().parent = ROOT;
            ctx.process_x11_event(reparented(ROOT))?;
            if ctx.embedded.is_present()
                || ctx.ready.read()
                || ctx.window_discovery.is_none()
            {
                return Ok(false);
            }
            ctx.attach()?;
            ctx.process_x11_events()?;
            let s = state.borrow();
            Ok(ctx.embedded.is_present()
                && ctx.ready.read()
                && ctx.window_discovery.is_none()
                && ctx.focus_pending.is_none()
                && s.parent == HOST
                && s.detached_resizes == 0)
        },
    );
    scenario(
        "stale detach notification cannot discard the current embedding",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = None;
            ctx.ready.write(true);
            ctx.process_x11_event(reparented(ROOT))?;
            Ok(ctx.embedded.is_present()
                && ctx.ready.read()
                && ctx.window_discovery.is_none()
                && state.borrow().focus_requests.is_empty())
        },
    );
    if failures != 0 {
        return Err(io::Error::other(format!(
            "{failures} focus replay scenario(s) failed"
        ))
        .into());
    }
    Ok(())
}

#[test]
fn client_and_host_loss_cannot_discard_keyboard_focus() -> Z {
    for order in [[CLIENT, HOST], [HOST, CLIENT]] {
        let (ctx, state) = make_ctx();
        assert!(ctx.embedded.focus(ctx.x11.get().input_focus()?)?);
        let mut focus_after_loss = [0; 2];
        for (index, window) in order.into_iter().enumerate() {
            // Destruction/disconnection makes the window and its descendants
            // unviewable. Do not dispatch events or run any xpop cleanup.
            ctx.x11.get().conn.set_mapped(window, false);
            focus_after_loss[index] = state.borrow().focus;
        }
        assert_eq!(
            focus_after_loss,
            [u32::from(InputFocus::POINTER_ROOT); 2],
            "window loss order {order:?} must leave keyboard input enabled"
        );
    }
    Ok(())
}

#[test]
fn already_focused_client_still_arms_safe_reversion() -> Z {
    let (ctx, state) = make_ctx();
    {
        let mut s = state.borrow_mut();
        s.focus = CLIENT;
        s.revert_to = InputFocus::PARENT;
    }
    assert!(ctx.embedded.focus(ctx.x11.get().input_focus()?)?);
    ctx.x11.get().conn.set_mapped(CLIENT, false);
    ctx.x11.get().conn.set_mapped(HOST, false);
    assert_eq!(state.borrow().focus, u32::from(InputFocus::POINTER_ROOT));
    Ok(())
}

#[test]
fn quitting_an_empty_host_with_unsafe_reversion_keeps_keyboard_input() -> Z {
    let (mut ctx, state) = make_ctx();
    ctx.embedded.clear();
    {
        let mut s = state.borrow_mut();
        s.focus = HOST;
        s.revert_to = InputFocus::NONE;
    }
    ctx.quit()?;
    assert_eq!(state.borrow().focus, u32::from(InputFocus::POINTER_ROOT));
    assert!(state.borrow().host_destroyed);
    Ok(())
}

#[test]
fn window_loss_does_not_steal_another_clients_focus() -> Z {
    for order in [[CLIENT, HOST], [HOST, CLIENT]] {
        let (ctx, state) = make_ctx();
        assert!(ctx.embedded.focus(ctx.x11.get().input_focus()?)?);
        let x11 = ctx.x11.get();
        x11.conn
            .set_input_focus(InputFocus::PARENT, OUTSIDE, x11rb::CURRENT_TIME)?
            .check()?;
        for window in order {
            x11.conn.set_mapped(window, false);
            assert_eq!(state.borrow().focus, OUTSIDE);
        }
    }
    Ok(())
}

fn make_hosted_ctx(
    on_exit: Behavior,
    showing: bool,
) -> Z<(Ctx, Rc<RefCell<State>>)> {
    let (mut ctx, state) = make_ctx();
    ctx.args.command = vec!["sleep".into(), "60".into()];
    ctx.args.working_dir = Some(std::env::temp_dir().canonicalize()?);
    ctx.args.on_exit = on_exit;
    ctx.showing = showing;
    ctx.focus_pending = showing.then_some(OUTSIDE);
    state.borrow_mut().host_mapped = showing;
    ctx.start()?;
    state.borrow_mut().client_group = ctx.process_group.unwrap();
    ctx.update_readiness()?;
    ctx.process_x11_events()?;
    Ok((ctx, state))
}

fn terminate_hosted(ctx: &mut Ctx) -> Z<u32> {
    let hosted = ctx.process.as_mut().expect("hosted child is running");
    let pid = hosted.child.id();
    hosted.child.kill()?;
    hosted.child.wait()?;
    ctx.reap_hosted()?;
    Ok(pid)
}

fn finish_pending_relaunch(ctx: &mut Ctx) -> Z {
    while ctx.process.is_none() {
        let timeout = ctx.poll_timeout();
        assert!((1..=500).contains(&timeout));
        dragons::poll(&mut [], timeout)?;
        ctx.reap_hosted()?;
    }
    ctx.x11.get().conn.state.borrow_mut().client_group =
        ctx.process_group.unwrap();
    Ok(())
}

#[test]
fn exit_none_quits_without_relaunch() -> Z {
    for showing in [false, true] {
        let (mut ctx, state) = make_hosted_ctx(Behavior::None, showing)?;
        terminate_hosted(&mut ctx)?;
        assert!(ctx.closed);
        assert!(ctx.process.is_none());
        assert!(state.borrow().host_destroyed);
        assert!(!state.borrow().host_mapped);
    }
    Ok(())
}

#[test]
fn exit_policies_relaunch_and_restore_visibility() -> Z {
    for on_exit in [Behavior::Hide, Behavior::Appear] {
        for showing in [false, true] {
            let (mut ctx, state) = make_hosted_ctx(on_exit, showing)?;
            let presenting = on_exit == Behavior::Appear;
            for attempt in 0..2 {
                if attempt == 1 {
                    // Also exit before a window is found, after discovery backs off.
                    ctx.embedded.clear();
                    ctx.ready.write(false);
                    ctx.window_discovery = Some(Discovery {
                        deadline: Instant::now() + Ctx::DISCOVERY_MAX_DELAY,
                        delay: Ctx::DISCOVERY_MAX_DELAY,
                    });
                }
                // A pending toggle cooldown must not suppress the exit policy
                // or the first user toggle after the replacement is launched.
                ctx.last_toggle = SystemTime::now() + Duration::from_secs(60);
                let previous_pid = terminate_hosted(&mut ctx)?;
                if on_exit == Behavior::Appear {
                    assert!(ctx.process.is_none());
                    finish_pending_relaunch(&mut ctx)?;
                }
                let hosted =
                    ctx.process.as_mut().expect("child was relaunched");
                let pid = hosted.child.id();
                assert_ne!(pid, previous_pid);
                assert!(hosted.child.try_wait()?.is_none());
                assert_eq!(
                    dragons::pgid(pid as libc::pid_t)?,
                    pid as libc::pid_t
                );
                assert_eq!(ctx.process_group, Some(pid as libc::pid_t));
                assert_eq!(
                    fs::read_link(format!("/proc/{pid}/cwd"))?,
                    *ctx.args.working_dir.as_ref().unwrap()
                );
                assert_eq!(
                    fs::read(format!("/proc/{pid}/cmdline"))?.as_slice(),
                    b"sleep\x0060\x00"
                );
                assert!(!ctx.closed);
                assert!(!state.borrow().host_destroyed);
                assert!(!state.borrow().host_mapped);
                assert_eq!(ctx.showing, presenting);
                assert!(ctx.focus_pending.is_none());
                assert!(!ctx.ready.read());
                assert!(!ctx.embedded.is_present());
                assert!(ctx.discovery_timeout() > 0);
                assert!(
                    ctx.discovery_timeout()
                        <= Ctx::DISCOVERY_INITIAL_DELAY.as_millis() as i32
                );

                // The replacement publishes a newly mapped window belonging
                // to its new group, not the prior child's hidden window.
                {
                    let mut s = state.borrow_mut();
                    s.client_group = pid as libc::pid_t;
                    s.client_mapped = true;
                }
                ctx.retry_discovery()?;
                ctx.process_x11_events()?;
                assert_eq!(ctx.ready.read(), presenting);
                assert_eq!(state.borrow().host_mapped, presenting);
                assert_ne!(state.borrow().focus, CLIENT);

                ctx.toggle()?;
                ctx.process_x11_events()?;
                assert_eq!(state.borrow().host_mapped, !presenting);
                assert_eq!(ctx.ready.read(), !presenting);
            }
        }
    }
    Ok(())
}

#[test]
fn exit_appear_waits_half_a_second_before_relaunch() -> Z {
    let (mut ctx, state) = make_hosted_ctx(Behavior::Appear, true)?;
    let before_exit = Instant::now();
    let previous_pid = terminate_hosted(&mut ctx)?;
    assert!(
        ctx.process.is_none(),
        "appear must not relaunch immediately"
    );
    assert!(ctx.process_group.is_none());
    assert!(ctx.window_discovery.is_none());
    assert!(!ctx.embedded.is_present());

    // A visibility toggle must still work while the replacement is pending.
    ctx.toggle()?;
    assert!(!ctx.showing);
    assert!(!state.borrow().host_mapped);
    ctx.reap_hosted()?;
    assert!(ctx.process.is_none());

    finish_pending_relaunch(&mut ctx)?;
    let hosted = ctx.process.as_mut().expect("child was relaunched");
    assert_ne!(hosted.child.id(), previous_pid);
    assert!(hosted.child.try_wait()?.is_none());
    assert!(before_exit.elapsed() >= Duration::from_millis(500));
    assert!(!ctx.showing);
    assert!(!state.borrow().host_mapped);
    Ok(())
}

#[test]
fn quit_cancels_a_pending_appear_relaunch() -> Z {
    let (mut ctx, state) = make_hosted_ctx(Behavior::Appear, true)?;
    terminate_hosted(&mut ctx)?;
    assert!(ctx.process.is_none());
    ctx.quit()?;
    std::thread::sleep(Duration::from_millis(500));
    ctx.reap_hosted()?;
    assert!(ctx.closed);
    assert!(ctx.process.is_none());
    assert!(state.borrow().host_destroyed);
    Ok(())
}

#[test]
fn clean_and_failed_child_exits_obey_policy() -> Z {
    for on_exit in [Behavior::None, Behavior::Hide, Behavior::Appear] {
        for code in [0, 7] {
            let (mut ctx, state) = make_ctx();
            ctx.args.on_exit = on_exit;
            ctx.args.command = vec![
                "sh".into(),
                "-c".into(),
                "exit \"$1\"".into(),
                "hosted-app".into(),
                code.to_string(),
            ];
            ctx.start()?;
            let hosted = ctx.process.as_mut().unwrap();
            let pid = hosted.child.id();
            assert_eq!(hosted.child.wait()?.code(), Some(code));
            ctx.reap_hosted()?;
            if on_exit == Behavior::None {
                assert!(ctx.closed);
                assert!(ctx.process.is_none());
                assert!(state.borrow().host_destroyed);
            }
            else {
                finish_pending_relaunch(&mut ctx)?;
                let hosted =
                    ctx.process.as_mut().expect("child was relaunched");
                assert_ne!(hosted.child.id(), pid);
                assert_eq!(hosted.child.wait()?.code(), Some(code));
                assert!(!ctx.closed);
                assert!(!state.borrow().host_destroyed);
            }
        }
    }
    Ok(())
}

#[test]
fn automatic_relaunch_preserves_another_hosts_keyboard_focus() -> Z {
    let (mut ctx, state) = make_hosted_ctx(Behavior::Appear, true)?;
    // OUTSIDE can be another xpop, not just a conventional WM-managed app.
    state.borrow_mut().focus = OUTSIDE;
    terminate_hosted(&mut ctx)?;
    finish_pending_relaunch(&mut ctx)?;
    assert_eq!(state.borrow().focus, OUTSIDE);
    assert!(!state.borrow().host_mapped);
    ctx.retry_discovery()?;
    ctx.process_x11_events()?;
    assert!(state.borrow().host_mapped);
    assert_eq!(state.borrow().focus, OUTSIDE);

    let replacement = ctx.process.as_ref().unwrap().child.id();
    for _ in 0..8 {
        ctx.reap_hosted()?;
        ctx.process_x11_events()?;
        assert_eq!(ctx.process.as_ref().unwrap().child.id(), replacement);
        assert_eq!(state.borrow().focus, OUTSIDE);
    }
    Ok(())
}

#[test]
fn child_exit_releases_focus_that_reverted_to_an_empty_host() -> Z {
    let (mut ctx, state) = make_hosted_ctx(Behavior::Appear, true)?;
    // An application can replace xpop's reversion policy with RevertToParent.
    // Its destruction then focuses the host with the next reversion set to None.
    {
        let mut s = state.borrow_mut();
        s.focus = HOST;
        s.revert_to = InputFocus::NONE;
    }
    terminate_hosted(&mut ctx)?;
    assert_eq!(state.borrow().focus, u32::from(InputFocus::POINTER_ROOT));
    assert!(!state.borrow().host_mapped);
    Ok(())
}

#[test]
fn late_readiness_does_not_steal_focus_after_the_user_switches_apps() -> Z {
    let (mut ctx, state) = make_ctx();
    state.borrow_mut().client_mapped = false;
    ctx.update_readiness()?;
    state.borrow_mut().focus = OUTSIDE + 1;
    ctx.embedded.map_window()?;
    ctx.process_x11_events()?;
    assert_eq!(state.borrow().focus, OUTSIDE + 1);
    Ok(())
}

#[test]
fn initial_launch_and_explicit_show_wait_for_a_real_client() -> Z {
    for initial_show in [false, true] {
        let (mut ctx, state) = make_ctx();
        ctx.args.command = vec!["sleep".into(), "60".into()];
        ctx.embedded.clear();
        ctx.showing = initial_show;
        ctx.focus_pending = initial_show.then_some(OUTSIDE);
        {
            let mut s = state.borrow_mut();
            s.client_available = false;
            s.host_mapped = false;
        }
        ctx.start()?;
        state.borrow_mut().client_group = ctx.process_group.unwrap();
        if !initial_show {
            ctx.toggle()?;
        }
        assert!(ctx.showing);
        assert!(!state.borrow().host_mapped);
        assert_eq!(state.borrow().focus, OUTSIDE);

        state.borrow_mut().client_available = true;
        ctx.retry_discovery()?;
        ctx.process_x11_events()?;
        assert!(state.borrow().host_mapped);
        assert_eq!(state.borrow().focus, CLIENT);
    }
    Ok(())
}

#[test]
fn hiding_releases_only_focus_owned_by_this_hosts_window_tree() -> Z {
    for focused in [HOST, CLIENT, CLIENT + 1, OUTSIDE] {
        let (ctx, state) = make_ctx();
        {
            let mut s = state.borrow_mut();
            s.focus = focused;
            s.revert_to = InputFocus::NONE;
        }
        ctx.host.hide()?;
        let expected = if focused == OUTSIDE {
            OUTSIDE
        }
        else {
            u32::from(InputFocus::POINTER_ROOT)
        };
        assert_eq!(state.borrow().focus, expected);
        assert!(!state.borrow().host_mapped);
    }
    Ok(())
}

#[test]
fn focus_handoff_to_an_empty_host_does_not_trap_keyboard_input() -> Z {
    let (mut ctx, state) = make_ctx();
    ctx.embedded.clear();
    {
        let mut s = state.borrow_mut();
        s.focus = HOST;
        s.revert_to = InputFocus::NONE;
    }
    ctx.process_x11_event(focus_in(NotifyMode::NORMAL))?;
    assert_eq!(state.borrow().focus, u32::from(InputFocus::POINTER_ROOT));
    assert!(!state.borrow().host_mapped);
    Ok(())
}

#[test]
fn embedding_preserves_the_focused_client_or_its_descendant() -> Z {
    for focused in [CLIENT, CLIENT + 1] {
        let (mut ctx, state) = make_ctx();
        ctx.embedded.clear();
        {
            let mut s = state.borrow_mut();
            s.parent = ROOT;
            s.focus = focused;
            s.revert_to = InputFocus::POINTER_ROOT;
            s.host_mapped = false;
        }
        ctx.embed(CLIENT)?;
        ctx.process_x11_events()?;
        assert!(ctx.ready.read());
        assert_eq!(state.borrow().focus, focused);
        assert_eq!(state.borrow().revert_to, InputFocus::POINTER_ROOT);
    }
    Ok(())
}

#[test]
fn relaunch_discovers_a_mapped_client_beneath_the_hidden_host() -> Z {
    for event_driven in [false, true] {
        let (mut ctx, state) = make_hosted_ctx(Behavior::Appear, true)?;
        terminate_hosted(&mut ctx)?;
        finish_pending_relaunch(&mut ctx)?;
        assert!(!state.borrow().host_mapped);
        assert!(!ctx.embedded.is_present());
        assert!(state.borrow().client_mapped);
        let focus = state.borrow().focus;
        if event_driven {
            ctx.attach_window(CLIENT)?;
        }
        else {
            ctx.retry_discovery()?;
        }
        ctx.process_x11_events()?;
        assert!(ctx.embedded.is_present());
        assert!(state.borrow().host_mapped);
        assert!(ctx.ready.read());
        assert!(ctx.window_discovery.is_none());
        assert_eq!(state.borrow().focus, focus);

        ctx.toggle()?;
        ctx.last_toggle = SystemTime::UNIX_EPOCH;
        ctx.toggle()?;
        ctx.process_x11_events()?;
        assert!(state.borrow().host_mapped);
        assert!(ctx.ready.read());
        assert_eq!(state.borrow().focus, CLIENT);
    }
    Ok(())
}

#[test]
fn discovery_rejects_unmapped_clients_and_other_process_groups() -> Z {
    let (mut ctx, state) = make_ctx();
    ctx.embedded.clear();
    ctx.schedule_discovery();
    state.borrow_mut().client_mapped = false;
    ctx.retry_discovery()?;
    assert!(!ctx.embedded.is_present());
    assert!(ctx.window_discovery.is_some());
    {
        let mut s = state.borrow_mut();
        s.client_mapped = true;
        s.client_group = 43;
    }
    ctx.retry_discovery()?;
    assert!(!ctx.embedded.is_present());
    assert!(ctx.window_discovery.is_some());
    state.borrow_mut().client_group = 42;
    ctx.retry_discovery()?;
    ctx.process_x11_events()?;
    assert!(ctx.embedded.is_present());
    assert!(ctx.ready.read());
    Ok(())
}

include!(concat!(env!("OUT_DIR"), "/focus_methods.rs"));
include!(concat!(env!("OUT_DIR"), "/syscall_methods.rs"));
