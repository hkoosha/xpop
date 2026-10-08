# xpop

Toggle an X11 application's pull-down visibility over D-Bus.

xpop leaves the target application as a normal top-level X11 window. It applies
the requested geometry once when the application is discovered, then uses
direct `UnmapWindow` / `MapWindow` calls to hide and restore it. To emulate
Guake-style pull-down behavior, xpop follows the GTK3 approach of requesting
that the window manager leave the target window unmanaged. These are only X11
hints, and individual window managers may disregard them.

xpop works only with Xorg. It definitely does not work on Wayland; use
window-manager-specific mechanisms instead, such as i3 scratchpads. XWayland
has not been tested.

xpop is an alternative to [tdrop](https://github.com/noctuid/tdrop) that uses
the same X11 window-management machinery with addition of WM hints following
the GTK3 approach. Keeping its daemon connected to an active Xorg session,
rather than repeatedly invoking shell binaries, appears to reduce window
flicker.

Successor to https://crates.io/crates/zoha for hosting a pull-down terminal in
Xorg. Happily working with Wezterm.

## Build

Requires `pkg-config`, and D-Bus development libraries. Running xpop requires an
X11 session and a D-Bus connection.

```sh
just run -h
just release
just install
```

## Usage

```sh
# Start the daemon. It may be abbreviated as `d`.
xpop daemon

# Toggle a matching window. `signal` may be abbreviated as `s`.
xpop signal toggle 'class:wezterm'

# Hide a matching window.
xpop s hide 'class:wezterm'

# `list` may be abbreviated as `l`. Repeating -l selects properties.
xpop list -l id -l class
```

## CLI Usage

```sh
An X11 pull-down application toggler

Usage: xpop [OPTIONS] [COMMAND]

Commands:
  daemon  Run the D-Bus daemon [alias: d]
  signal  Send a command to the daemon for an X11 window [alias: s]
  list    List reusable filters for X11 client windows [alias: l]

Options:
  -v, --verbose...               Enable debug logging; may appear before or after subcommands
  -n, --namespace <NAMESPACE>    DBus namespace to run under or connect to
  -h, --help                     Print help
```

