//! In-memory focus/ownership replay using current production methods.
//! No X server, rendering, wire protocol, window-manager scheduling, or D-Bus daemon.
use std::{
    cell::RefCell,
    collections::VecDeque,
    io,
    rc::Rc,
    time::{
        Duration,
        SystemTime,
    },
};
use x11rb::protocol::{
    Event,
    xproto::*,
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
    host_mapped: bool,
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
struct FocusReply {
    focus: Window,
    revert_to: InputFocus,
}
struct TreeReply {
    parent: Window,
}
struct Conn {
    state: Rc<RefCell<State>>,
}
impl Conn {
    fn get_input_focus(&self) -> Z<Reply<FocusReply>> {
        Ok(Reply(FocusReply {
            focus: self.state.borrow().focus,
            revert_to: self.state.borrow().revert_to,
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
            else {
                ROOT
            },
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
                || (window == HOST && s.focus == CLIENT && s.parent == HOST))
        {
            s.revert_focus();
        }
    }
}
struct X11Host {
    conn: Conn,
}
struct GrabGuard<'a> {
    _host: &'a X11Host,
}
impl X11Host {
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
        let x11 = self.x11.get();
        let s = x11.conn.state.borrow();
        Ok(self.is_present()
            && s.client_mapped
            && (s.parent != HOST || s.host_mapped))
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
    fn hide(&self) -> Z {
        self.x11.get().conn.set_mapped(HOST, false);
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
struct Ctx {
    x11: R<X11Host>,
    host: HostWindowMan,
    embedded: EmbeddedWindowMan,
    ready: R<bool>,
    showing: bool,
    focus_pending: bool,
    last_toggle: SystemTime,
    window_discovery: Option<()>,
}
impl Ctx {
    const TOGGLE_COOL_DOWN_MILLIS: u128 = 40;
    fn schedule_discovery(&mut self) {
        if !self.embedded.is_present() {
            self.window_discovery = Some(());
        }
    }
    fn attach_window(
        &mut self,
        window: Window,
    ) -> Z {
        self.embed(window)
    }
    fn attach(&mut self) -> Z {
        self.embed(CLIENT)
    }
}
fn make_ctx() -> (Ctx, Rc<RefCell<State>>) {
    let state = Rc::new(RefCell::new(State {
        focus: OUTSIDE,
        revert_to: InputFocus::NONE,
        parent: HOST,
        client_mapped: true,
        host_mapped: true,
        events: VecDeque::new(),
        focus_requests: vec![],
        delivered: 0,
        fail_focus: false,
        detached_resizes: 0,
    }));
    let x11 = R::of(X11Host {
        conn: Conn {
            state: state.clone(),
        },
    });
    let ready = R::of(false);
    let ctx = Ctx {
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
        focus_pending: true,
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
                && !ctx.focus_pending)
        },
    );
    scenario(
        "a completed focus request is not replayed by map notifications",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = false;
            state.borrow_mut().focus = CLIENT;
            state.borrow_mut().events.push_back(mapped(CLIENT, HOST));
            ctx.process_x11_events()?;
            Ok(state.borrow().focus == CLIENT
                && state.borrow().focus_requests.is_empty()
                && !ctx.focus_pending)
        },
    );
    scenario(
        "an actual host focus is forwarded to the client once",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = false;
            state.borrow_mut().focus = HOST;
            state
                .borrow_mut()
                .events
                .push_back(focus_in(NotifyMode::NORMAL));
            ctx.process_x11_events()?;
            Ok(state.borrow().focus == CLIENT
                && state.borrow().focus_requests == [CLIENT]
                && !ctx.focus_pending)
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
            ctx.focus_pending = false;
            state.borrow_mut().focus = focus;
            state.borrow_mut().events.push_back(focus_in(mode));
            ctx.process_x11_events()?;
            Ok(state.borrow().focus == focus
                && state.borrow().focus_requests.is_empty()
                && !ctx.focus_pending)
        });
    }
    scenario(
        "hide/show rearms focus despite queued old unmap notifications",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = false;
            state.borrow_mut().focus = CLIENT;
            ctx.toggle()?;
            ctx.last_toggle = SystemTime::UNIX_EPOCH;
            ctx.toggle()?;
            ctx.process_x11_events()?;
            Ok(ctx.showing
                && ctx.ready.read()
                && !ctx.focus_pending
                && state.borrow().focus == CLIENT
                && state.borrow().focus_requests == [CLIENT])
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
                || !ctx.focus_pending
                || !state.borrow().focus_requests.is_empty()
            {
                return Ok(false);
            }
            ctx.embedded.map_window()?;
            ctx.process_x11_events()?;
            Ok(ctx.ready.read()
                && !ctx.focus_pending
                && state.borrow().focus == CLIENT)
        },
    );
    scenario(
        "focus failure does not consume the outstanding focus request",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            state.borrow_mut().fail_focus = true;
            if ctx.update_readiness().is_ok() || !ctx.focus_pending {
                return Ok(false);
            }
            state.borrow_mut().fail_focus = false;
            ctx.update_readiness()?;
            ctx.process_x11_events()?;
            Ok(!ctx.focus_pending && state.borrow().focus == CLIENT)
        },
    );
    scenario(
        "confirmed detach invalidates ownership and existing discovery can reattach",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = false;
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
                && !ctx.focus_pending
                && s.parent == HOST
                && s.focus == CLIENT
                && s.detached_resizes == 0)
        },
    );
    scenario(
        "stale detach notification cannot discard the current embedding",
        &mut failures,
        || {
            let (mut ctx, state) = make_ctx();
            ctx.focus_pending = false;
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
        assert!(ctx.embedded.focus()?);
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
    assert!(ctx.embedded.focus()?);
    ctx.x11.get().conn.set_mapped(CLIENT, false);
    ctx.x11.get().conn.set_mapped(HOST, false);
    assert_eq!(state.borrow().focus, u32::from(InputFocus::POINTER_ROOT));
    Ok(())
}

#[test]
fn host_loss_before_embedding_cannot_discard_keyboard_focus() -> Z {
    let (ctx, state) = make_ctx();
    ctx.embedded.clear();
    ctx.x11.get().conn.set_mapped(CLIENT, false);
    ctx.host.focus()?;
    ctx.x11.get().conn.set_mapped(HOST, false);
    assert_eq!(state.borrow().focus, u32::from(InputFocus::POINTER_ROOT));
    Ok(())
}

#[test]
fn window_loss_does_not_steal_another_clients_focus() -> Z {
    for order in [[CLIENT, HOST], [HOST, CLIENT]] {
        let (ctx, state) = make_ctx();
        assert!(ctx.embedded.focus()?);
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

include!(concat!(env!("OUT_DIR"), "/focus_methods.rs"));
