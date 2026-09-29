//! Display-free selection checks using production logic and X11 protocol types.

use x11rb::protocol::{
    render,
    xproto::{
        self,
        Window,
    },
};

#[derive(Debug, PartialEq, Eq)]
enum MyError {
    NoScreen,
    NoArgbVisual,
}
type Z<T> = Result<T, MyError>;

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
