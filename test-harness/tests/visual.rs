//! Display-free visual selection and host lifecycle using production methods.

#[path = "visual/model.rs"]
mod model;

use model::*;
use x11rb::protocol::xproto::{
    AtomEnum,
    ConfigureWindowAux,
    CreateWindowAux,
    EventMask,
    PropMode,
    Window,
    WindowClass,
};

macro_rules! log {
    ($scope:ident $( @ $level:ident )? $fmt:literal $($args:tt)*) => {
        eprintln!($fmt $($args)*);
    };
}

#[derive(Debug, PartialEq, Eq)]
enum MyError {
    NoScreen,
    NoArgbVisual,
    Model(&'static str),
}
impl std::fmt::Display for MyError {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
type Z<T = ()> = Result<T, MyError>;

include!(concat!(env!("OUT_DIR"), "/visual_methods.rs"));

const FIRST_ROOT: Window = 0x10;
const HOST_ROOT: Window = 0x20;
const FIRST_ARGB: xproto::Visualid = 0x101;
const RGB32: xproto::Visualid = 0x201;
const DIRECT_COLOR: xproto::Visualid = 0x202;
const HOST_ARGB: xproto::Visualid = 0x203;
const RGB_FORMAT: render::Pictformat = 1;
const ARGB_FORMAT: render::Pictformat = 2;

fn visual(
    visual_id: xproto::Visualid,
    class: xproto::VisualClass,
) -> xproto::Visualtype {
    xproto::Visualtype {
        visual_id,
        class,
        bits_per_rgb_value: 8,
        colormap_entries: 256,
        red_mask: 0xff0000,
        green_mask: 0xff00,
        blue_mask: 0xff,
    }
}

fn fixture() -> (xproto::Setup, render::QueryPictFormatsReply) {
    let setup = xproto::Setup {
        roots: vec![
            xproto::Screen {
                root: FIRST_ROOT,
                allowed_depths: vec![xproto::Depth {
                    depth: 32,
                    visuals: vec![visual(
                        FIRST_ARGB,
                        xproto::VisualClass::TRUE_COLOR,
                    )],
                }],
                ..Default::default()
            },
            xproto::Screen {
                root: HOST_ROOT,
                allowed_depths: vec![xproto::Depth {
                    depth: 32,
                    visuals: vec![
                        visual(RGB32, xproto::VisualClass::TRUE_COLOR),
                        visual(DIRECT_COLOR, xproto::VisualClass::DIRECT_COLOR),
                        visual(HOST_ARGB, xproto::VisualClass::TRUE_COLOR),
                    ],
                }],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let rgb = render::Directformat {
        red_shift: 16,
        red_mask: 0xff,
        green_shift: 8,
        green_mask: 0xff,
        blue_shift: 0,
        blue_mask: 0xff,
        ..Default::default()
    };
    let formats = render::QueryPictFormatsReply {
        formats: vec![
            render::Pictforminfo {
                id: RGB_FORMAT,
                type_: render::PictType::DIRECT,
                depth: 32,
                direct: rgb,
                ..Default::default()
            },
            render::Pictforminfo {
                id: ARGB_FORMAT,
                type_: render::PictType::DIRECT,
                depth: 32,
                direct: render::Directformat {
                    alpha_shift: 24,
                    alpha_mask: 0xff,
                    ..rgb
                },
                ..Default::default()
            },
        ],
        screens: vec![
            render::Pictscreen {
                depths: vec![render::Pictdepth {
                    depth: 32,
                    visuals: vec![render::Pictvisual {
                        visual: FIRST_ARGB,
                        format: ARGB_FORMAT,
                    }],
                }],
                ..Default::default()
            },
            render::Pictscreen {
                depths: vec![render::Pictdepth {
                    depth: 32,
                    visuals: vec![
                        render::Pictvisual {
                            visual: RGB32,
                            format: RGB_FORMAT,
                        },
                        render::Pictvisual {
                            visual: DIRECT_COLOR,
                            format: ARGB_FORMAT,
                        },
                        render::Pictvisual {
                            visual: HOST_ARGB,
                            format: ARGB_FORMAT,
                        },
                    ],
                }],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    (setup, formats)
}

#[test]
fn selects_truecolor_alpha_on_the_hosts_screen() {
    let (setup, formats) = fixture();
    assert_eq!(
        find_argb_visual(&setup, FIRST_ROOT, &formats),
        Ok(FIRST_ARGB)
    );
    assert_eq!(find_argb_visual(&setup, HOST_ROOT, &formats), Ok(HOST_ARGB));
}

#[test]
fn cannot_substitute_opaque_or_directcolor_visuals() {
    let (mut setup, formats) = fixture();
    setup.roots[1].allowed_depths[0]
        .visuals
        .retain(|visual| visual.visual_id != HOST_ARGB);
    assert_eq!(
        find_argb_visual(&setup, HOST_ROOT, &formats),
        Err(MyError::NoArgbVisual)
    );
}

#[test]
fn alpha_pixmap_format_requires_a_matching_window_visual() {
    let (setup, mut formats) = fixture();
    formats.screens[1].depths[0]
        .visuals
        .retain(|visual| visual.visual != HOST_ARGB);
    assert_eq!(
        find_argb_visual(&setup, HOST_ROOT, &formats),
        Err(MyError::NoArgbVisual)
    );
}

#[test]
fn rgb_only_screen_does_not_use_another_screens_alpha_visual() {
    let (mut setup, mut formats) = fixture();
    setup.roots[1].allowed_depths = vec![xproto::Depth {
        depth: 24,
        visuals: vec![visual(RGB32, xproto::VisualClass::TRUE_COLOR)],
    }];
    formats.screens[1].depths.clear();
    assert_eq!(
        find_argb_visual(&setup, FIRST_ROOT, &formats),
        Ok(FIRST_ARGB)
    );
    assert_eq!(
        find_argb_visual(&setup, HOST_ROOT, &formats),
        Err(MyError::NoArgbVisual)
    );
}

fn host_fixture(
    alpha: bool,
    failure: Failure,
) -> R<X11Host> {
    let (mut setup, formats) = fixture();
    let screen = &mut setup.roots[1];
    screen.root_depth = 24;
    screen.root_visual = RGB32;
    screen.default_colormap = 0x30;
    if !alpha {
        screen.allowed_depths.clear();
    }
    screen.allowed_depths.push(xproto::Depth {
        depth: 24,
        visuals: vec![visual(RGB32, xproto::VisualClass::TRUE_COLOR)],
    });
    let mut state = State::default();
    state.colormaps.insert(screen.default_colormap, RGB32);
    R::of(X11Host {
        conn: Conn {
            setup,
            formats,
            failure,
            state: std::cell::RefCell::new(state),
        },
        root_win: HOST_ROOT,
    })
}

fn create_host(x11: R<X11Host>) -> Z<HostWindowMan> {
    let (window, colormap) = create_host_window(
        x11.clone(),
        Area {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        },
        "fallback regression",
    )?;
    Ok(HostWindowMan {
        x11,
        window,
        colormap,
    })
}

fn verify_default_visual(
    alpha: bool,
    failure: Failure,
) {
    let x11 = host_fixture(alpha, failure);
    let host = create_host(x11.clone()).expect("default visual must work");
    let window = x11.get().conn.state.borrow().windows[&host.window];
    assert_eq!(
        (window.depth, window.visual, window.colormap),
        (24, RGB32, 0x30)
    );
    assert_eq!(host.colormap, None);
    host.destroy().unwrap();
    let state = x11.get().conn.state.borrow();
    assert!(state.windows.is_empty());
    assert_eq!(state.colormaps, [(0x30, RGB32)].into());
}

#[test]
fn missing_alpha_visual_still_creates_and_destroys_the_host() {
    verify_default_visual(false, Failure::None);
}

#[test]
fn transparency_setup_errors_use_the_inherited_visual_and_colormap() {
    for failure in [Failure::Render, Failure::Formats, Failure::Colormap] {
        verify_default_visual(true, failure);
    }
}

#[test]
fn supported_alpha_visual_retains_its_colormap_until_host_destruction() {
    let x11 = host_fixture(true, Failure::None);
    let host = create_host(x11.clone()).unwrap();
    let window = x11.get().conn.state.borrow().windows[&host.window];
    assert_eq!((window.depth, window.visual), (32, HOST_ARGB));
    assert_eq!((window.background, window.border), (Some(0), Some(0)));
    let colormap = host.colormap.expect("alpha needs its own colormap");
    assert_eq!(window.colormap, colormap);
    assert_eq!(
        x11.get().conn.state.borrow().colormaps[&colormap],
        HOST_ARGB
    );
    host.destroy().unwrap();
    let state = x11.get().conn.state.borrow();
    assert!(state.windows.is_empty());
    assert_eq!(state.colormaps, [(0x30, RGB32)].into());
}

#[test]
fn unrelated_window_errors_remain_fatal_without_leaking_alpha_colormaps() {
    for alpha in [false, true] {
        let x11 = host_fixture(alpha, Failure::Window);
        assert!(matches!(
            create_host(x11.clone()),
            Err(MyError::Model("window rejected"))
        ));
        let state = x11.get().conn.state.borrow();
        assert!(state.windows.is_empty());
        assert_eq!(state.colormaps, [(0x30, RGB32)].into());
    }
}
