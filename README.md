# xpop

Toggle an X11 application's pull-down visibility over D-Bus.

xpop keeps the application as its own normal top-level X11 window. It applies
the requested geometry once when the application is discovered, then uses
direct `UnmapWindow` / `MapWindow` calls to hide and restore it. It does not
reparent the application or resize it while toggling, preserving the window
manager's ownership and the application's dimensions.

Successor to https://crates.io/crates/zoha for hosting a pull-down terminal in
XOrg.

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
# Launch a half-height terminal.
xpop --height 50% wezterm

# Relaunch and hide the terminal hidden after it exits.
xpop --on-exit hide wezterm

# Toggle visibility from another terminal, or a desktop keyboard shortcut.
xpop --signal

# Launch another app, separated from the one launched above.
xpop --member Terminal_2 xterm
xpop --member Terminal_2 --signal 
```

## CLI Usage

```sh
An X11 pull-down application host

Usage: xpop [OPTIONS] [COMMAND]...

Arguments:
  [COMMAND]...  The XORG app and its arguments to host

Options:
  -s, --signal                     Send D-Bus signal to request visibility toggle and exit
  -v, --verbose...                 
  -w, --working-dir <WORKING_DIR>  
  -m, --member <MEMBER>            D-Bus member used to identify the app to toggle [default: xpop]
      --on-start <ON_START>        [default: appear] [possible values: hide, appear, none]
      --on-exit <ON_EXIT>          [default: appear] [possible values: hide, appear, none]
      --title <TITLE>              [default: main]
  -x <X>                           [default: 0]
  -y <Y>                           [default: 0]
      --width <WIDTH>              [default: 100%]
      --height <HEIGHT>            [default: 100%]
  -h, --help                       Print help
  -V, --version                    Print version
```
