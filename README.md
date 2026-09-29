# xpop

Host an X11 application in a pull-down window and toggle its visibility over
D-Bus.

Successor to https://crates.io/crates/zoha for hosting a pull-down terminal in
XOrg.

## Build

Requires Rust, `pkg-config`, and D-Bus development libraries. Running xpop
requires an X11 session and a D-Bus session bus.

```sh
just release
just install
```

## Usage

```sh
# Launch a half-height terminal.
xpop --height 50% wezterm

# Toggle visibility from another terminal or a desktop shortcut.
xpop --signal
```

## CLI Usage

```sh
$> xpop -h

An X11 pull-down application host

Usage: xpop [OPTIONS] [COMMAND]...

Arguments:
  [COMMAND]...  The XORG app and its arguments to host

Options:
  -s, --signal                           Send D-Bus signal to request visibility toggle and exit
  -v, --verbose...
  -w, --working-dir <WORKING_DIR>
      --dbus-interface <DBUS_INTERFACE>  [default: io.koosha.xpop]
      --dbus-member <DBUS_MEMBER>        [default: xpop]
      --dbus-path <DBUS_PATH>            [default: /io/koosha/xpop]
      --on-start <ON_START>              [default: appear] [possible values: hide, appear, none]
      --title <TITLE>                    [default: main]
  -x <X>                                 [default: 0]
  -y <Y>                                 [default: 0]
      --width <WIDTH>                    [default: 100%]
      --height <HEIGHT>                  [default: 100%]
  -h, --help                             Print help
  -V, --version                          Print version
```
