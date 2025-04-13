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

The managing of windows on a sliding strip is inspired by [Niri] and [PaperWM.spoon].

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

Drop following configuration lines into your `~/.paneru` file:

```
# syntax=toml
#
# Example configuration for Paneru.
#
[options]
# Enables focus follows mouse
focus_follows_mouse = true

[bindings]
# Moves the focus between windows.
window_focus_west = "cmd - h"
window_focus_east = "cmd - l"

# Swaps windows in chosen direction.
window_swap_west = "alt - h"
window_swap_east = "alt - l"

# Jump to the left most or right most windows.
window_focus_first = "cmd + shift - h"
window_focus_last = "cmd + shift - l"

# Move the current window into the left most or right most positions.
window_swap_first = "alt + shift - h"
window_swap_last = "alt + shift - l"

# Centers the current window on screen.
window_center = "alt - c"

# Shuffles between predefined window sizes: 25%, 33%, 50%, 66% and 75%.
window_resize = "alt - r"

# Toggles the window for management. If unmanaged, the window will be "floating".
window_manage = "ctrl + alt - t"

# Quits the window manager.
quit = "ctrl + alt - q"
```

The settings can be changed while Paneru is running - they will be automatically reloaded.
Very useful for tweaking keyboard bindings.

Start the main binary without any parameters:

```shell
$ cargo run paneru
```

You can change the default `info` log level to more verbose levels (`debug`, `trace`) with:

```shell
$ RUST_LOG=debug cargo run paneru
```

## TODO

- More commands for manipulating windows: fullscreen, finegrained size adjustments, etc.
- Multiple windows stacked into the same column.
- Scriptability. A nice feature would be to use Lua for configuration and simple scripting,
  like triggering and positioning specific windows or applications.

## Architecture Overview

The overall architecture is layered, with a platform interaction layer at the base.
This bottom layer, primarily within `platform.rs`, interfaces directly with the macOS operating system via Objective-C and Core Graphics APIs.
It runs the main RunLoop in the main thread, receiving OS-level events and acting as the bridge between the operating system and the application's logic.
Events captured by this layer, such as window events, application state changes, and mouse events, are then pushed into a multiple-producer single-consumer (MPSC) queue.

Higher layers of the application consume events from this queue.
These layers include the `WindowManager`, `ProcessManager`, and various event handlers.
The `WindowManager` is responsible for tracking and manipulating window states, while the `ProcessManager` handles the lifecycle of applications.
Event handlers in modules like `events.rs` interpret the raw events and orchestrate the appropriate responses within the application.
This design promotes a decoupled architecture, allowing modules to operate independently while reacting to system-level changes.

## Tile Scrollably Elsewhere

Here are some other projects which implement a similar workflow:

- [Niri]: a scrollable tiling Wayland compositor.
- [PaperWM]: scrollable tiling on top of GNOME Shell.
- [karousel]: scrollable tiling on top of KDE.
- [papersway]: scrollable tiling on top of sway/i3.
- [hyprscroller] and [hyprslidr]: scrollable tiling on top of Hyprland.
- [PaperWM.spoon]: scrollable tiling on top of MacOS.

[Yabai]: https://github.com/koekeishiya/yabai
[Niri]: https://github.com/YaLTeR/niri
[PaperWM]: https://github.com/paperwm/PaperWM
[karousel]: https://github.com/peterfajdiga/karousel
[papersway]: https://spwhitton.name/tech/code/papersway/
[hyprscroller]: https://github.com/dawsers/hyprscroller
[hyprslidr]: https://gitlab.com/magus/hyprslidr
[PaperWM.spoon]: https://github.com/mogenson/PaperWM.spoon
[The future is Niri]: https://ersei.net/en/blog/niri
