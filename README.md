# Paneru

A sliding, tiling window manager for MacOS.


## About

Windows are arranged on an infinite strip going to the right.
Opening a new window never causes existing windows to resize.

Every monitor has its own separate window strip.
Windows can never "overflow" onto an adjacent monitor.


## Why

- Standard tiling window managers are sub-optimal for large displays.
  You are either looking at huge maximised windows or a bunch of very small windows.
- They are also bad for small displays (e.g. a laptop).
  One ends up maximising all of the windows, becausing tiling them makes the windows too small to be usable.
- Wanted to emulate [Niri] behavior on MacOS.
- Learn stuff about MacOS API and Objective C.
- Good way to practice Rust.


## Inspiration

The code is heavily inspired by another excellent MacOS window manager - [Yabai].
Reading its source allows for a deep understanding on how to manage windows on MacOS,
especially the undocumented functions!

The managing of windows on a sliding strip  is inspired by [Niri] and [PaperWM.spoon].


## Installation

The window manager is working in a basic state.

### Building

Due to pecularity of some MacOS internal libraries, standard Rust linker is unable to
link to some of them - it gets a message saying "you are not allowed to link to this library".
To avoid this a simple Zig wrapper is made around the Private Frameworks, which is then linked into Rust.

The first step builds the Zig wrapper and the second builds the Rust binary itself:

```shell
$ zig build
$ cargo build --target release
$ cp target/release/paneru ~/bin/
```


### Configuration

Currently the window manager lacks a configuration file. For keyboard bindings it listens on a socket
(`/tmp/paneru.socket`) for commands.
These are provided by an external helper - [simple hotkey daemon]. The `skhd` is easy to configure and
it hot reloads the configuration.

```
# Example configuration for Paneru.
# Binary 'paneru' should be in your PATH, or provide a full path.
#
# Moves the focus between windows.
cmd - h : paneru window focus west
cmd - l : paneru window focus east
# Swaps windows in chosen direction.
alt - h : paneru window swap west
alt - l : paneru window swap east
# Centers the current window on screen.
alt - c : paneru window center
# Shuffles between predefined window sizes: 25%, 33%, 50%, 66% and 75%.
alt - r : paneru window resize
# Toggles the window for management. If unmanaged, the window will be "floating".
ctrl + alt - t : paneru window manage
# Quits the window manager.
ctrl + alt - q : paneru quit
```

###

Run the `skhd` daemon after confguring it and then start the main binary without any parameters:
```shell
$ paneru
```

You can change the default `info` log level to more verbose levels (`debug`, `trace`) with:

```shell
$ RUST_LOG=debug paneru
```


## TODO

- Configuration file.
- Reading keyboard events directly. Currently they keys are grabbed by a [simple hotkey daemon]
  and written to a pipe as an event the window manager can react to.
- Scriptability. A nice feature would be to use Lua for configuration and simple scripting,
  like triggering and positioning specific windows or applications.


## Tile Scrollably Elsewhere

Here are some other projects which implement a similar workflow:


- [Niri]: a scrollable tiling Wayland compositor.
- [PaperWM]: scrollable tiling on top of GNOME Shell.
- [karousel]: scrollable tiling on top of KDE.
- [papersway]: scrollable tiling on top of sway/i3.
- [hyprscroller] and [hyprslidr]: scrollable tiling on top of Hyprland.
- [PaperWM.spoon]: scrollable tiling on top of MacOS.

[simple hotkey daemon]: https://github.com/koekeishiya/skhd
[Yabai]: https://github.com/koekeishiya/yabai
[Niri]: https://github.com/YaLTeR/niri
[PaperWM]: https://github.com/paperwm/PaperWM
[karousel]: https://github.com/peterfajdiga/karousel
[papersway]: https://spwhitton.name/tech/code/papersway/
[hyprscroller]: https://github.com/dawsers/hyprscroller
[hyprslidr]: https://gitlab.com/magus/hyprslidr
[PaperWM.spoon]: https://github.com/mogenson/PaperWM.spoon
[The future is Niri]: https://ersei.net/en/blog/niri
