//! Models depth/visual compatibility and colormap ownership, not rendering.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
};

use super::{
    MyError,
    Z,
};
use x11rb::protocol::xproto::{
    Atom,
    AtomEnum,
    ConfigureWindowAux,
    CreateWindowAux,
    PropMode,
    Window,
    WindowClass,
};

pub mod render {
    pub use x11rb::protocol::render::*;

    use super::*;

    pub fn query_version(
        conn: &Conn,
        _: u32,
        _: u32,
    ) -> Z<Reply<()>> {
        if conn.failure == Failure::Render {
            return Err(MyError::Model("Render unavailable"));
        }
        Ok(Reply(Ok(())))
    }

    pub fn query_pict_formats(conn: &Conn) -> Z<Reply<QueryPictFormatsReply>> {
        Ok(Reply(if conn.failure == Failure::Formats {
            Err(MyError::Model("Render formats unavailable"))
        }
        else {
            Ok(conn.formats.clone())
        }))
    }
}

pub mod xproto {
    pub use x11rb::protocol::xproto::*;

    use super::*;

    pub struct ColormapWrapper<'a> {
        conn: &'a Conn,
        id: Colormap,
        owned: bool,
    }

    impl<'a> ColormapWrapper<'a> {
        pub fn create_colormap_and_get_cookie(
            conn: &'a Conn,
            _: ColormapAlloc,
            _: Window,
            visual: Visualid,
        ) -> Z<(Self, Checked)> {
            let id = 0x40;
            let result = if conn.failure == Failure::Colormap {
                Err(MyError::Model("colormap rejected"))
            }
            else {
                conn.state.borrow_mut().colormaps.insert(id, visual);
                Ok(())
            };
            Ok((
                Self {
                    conn,
                    id,
                    owned: true,
                },
                Checked(result),
            ))
        }

        pub fn colormap(&self) -> Colormap {
            self.id
        }

        pub fn into_colormap(mut self) -> Colormap {
            self.owned = false;
            self.id
        }
    }

    impl Drop for ColormapWrapper<'_> {
        fn drop(&mut self) {
            if self.owned {
                self.conn.state.borrow_mut().colormaps.remove(&self.id);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    None,
    Render,
    Formats,
    Colormap,
    Window,
}

pub struct Reply<T>(pub Z<T>);
impl<T> Reply<T> {
    pub fn reply(self) -> Z<T> {
        self.0
    }
}

pub struct Checked(pub Z);
impl Checked {
    pub fn check(self) -> Z {
        self.0
    }
}

#[derive(Clone, Copy)]
pub struct WindowState {
    pub depth: u8,
    pub visual: xproto::Visualid,
    pub colormap: xproto::Colormap,
    pub background: Option<u32>,
    pub border: Option<u32>,
}

#[derive(Default)]
pub struct State {
    pub windows: HashMap<Window, WindowState>,
    pub colormaps: HashMap<xproto::Colormap, xproto::Visualid>,
}

pub struct Conn {
    pub setup: xproto::Setup,
    pub formats: render::QueryPictFormatsReply,
    pub failure: Failure,
    pub state: RefCell<State>,
}

impl Conn {
    pub fn setup(&self) -> &xproto::Setup {
        &self.setup
    }

    pub fn generate_id(&self) -> Z<Window> {
        Ok(0x50)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_window(
        &self,
        depth: u8,
        window: Window,
        parent: Window,
        _: i16,
        _: i16,
        _: u16,
        _: u16,
        _: u16,
        _: WindowClass,
        visual: xproto::Visualid,
        attributes: &CreateWindowAux,
    ) -> Z<Checked> {
        if self.failure == Failure::Window {
            return Ok(Checked(Err(MyError::Model("window rejected"))));
        }
        let screen =
            self.setup.roots.iter().find(|s| s.root == parent).unwrap();
        let depth = if depth == x11rb::COPY_DEPTH_FROM_PARENT {
            screen.root_depth
        }
        else {
            depth
        };
        let visual = if visual == x11rb::COPY_FROM_PARENT {
            screen.root_visual
        }
        else {
            visual
        };
        let colormap = attributes.colormap.unwrap_or(screen.default_colormap);
        let mut state = self.state.borrow_mut();
        let compatible = screen.allowed_depths.iter().any(|d| {
            d.depth == depth && d.visuals.iter().any(|v| v.visual_id == visual)
        }) && state.colormaps.get(&colormap) == Some(&visual);
        if !compatible {
            return Ok(Checked(Err(MyError::Model("BadMatch"))));
        }
        state.windows.insert(
            window,
            WindowState {
                depth,
                visual,
                colormap,
                background: attributes.background_pixel,
                border: attributes.border_pixel,
            },
        );
        Ok(Checked(Ok(())))
    }

    pub fn configure_window(
        &self,
        window: Window,
        _: &ConfigureWindowAux,
    ) -> Z<Checked> {
        Ok(Checked(self.require_window(window)))
    }

    pub fn intern_atom(
        &self,
        _: bool,
        _: &[u8],
    ) -> Z<Reply<AtomReply>> {
        Ok(Reply(Ok(AtomReply { atom: 0x60 })))
    }

    pub fn change_property8(
        &self,
        _: PropMode,
        window: Window,
        _: AtomEnum,
        _: Atom,
        _: &[u8],
    ) -> Z<Checked> {
        Ok(Checked(self.require_window(window)))
    }

    pub fn destroy_window(
        &self,
        window: Window,
    ) -> Z<Checked> {
        let result = self.require_window(window);
        self.state.borrow_mut().windows.remove(&window);
        Ok(Checked(result))
    }

    pub fn free_colormap(
        &self,
        colormap: xproto::Colormap,
    ) -> Z<Checked> {
        if self
            .setup
            .roots
            .iter()
            .any(|s| s.default_colormap == colormap)
        {
            return Err(MyError::Model("cannot free the inherited colormap"));
        }
        let mut state = self.state.borrow_mut();
        if state.windows.values().any(|w| w.colormap == colormap) {
            return Err(MyError::Model("colormap still in use"));
        }
        let result = state
            .colormaps
            .remove(&colormap)
            .ok_or(MyError::Model("unknown colormap"))
            .map(|_| ());
        Ok(Checked(result))
    }

    pub fn flush(&self) -> Z {
        Ok(())
    }

    fn require_window(
        &self,
        window: Window,
    ) -> Z {
        if self.state.borrow().windows.contains_key(&window) {
            Ok(())
        }
        else {
            Err(MyError::Model("unknown window"))
        }
    }
}

pub struct AtomReply {
    pub atom: Atom,
}

pub struct R<T>(pub Rc<T>);
impl<T> Clone for R<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> R<T> {
    pub fn of(value: T) -> Self {
        Self(Rc::new(value))
    }

    pub fn get(&self) -> &T {
        &self.0
    }
}

pub struct X11Host {
    pub conn: Conn,
    pub root_win: Window,
}

pub struct HostWindowMan {
    pub x11: R<X11Host>,
    pub window: Window,
    pub colormap: Option<xproto::Colormap>,
}

pub struct Area {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}
