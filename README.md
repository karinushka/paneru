# Paneru

A sliding, tiling window manager for MacOS.

## About

Paneru is a MacOS window manager that arranges windows on an infinite strip,
extending to the right. A core principle is that opening a new window will
**never** cause existing windows to resize, maintaining your layout stability.

Each monitor operates with its own independent window strip, ensuring that
windows remain confined to their respective displays and do not "overflow" onto
adjacent monitors.

<video src="https://github.com/user-attachments/assets/cbc2e820-635f-408b-923a-6cb47c44704c"></video>

## Why Paneru?

- **Niri-like Behavior on MacOS:** Inspired by the user experience of [Niri],
  Paneru aims to bring a similar scrollable tiling workflow to MacOS.
- **Works with MacOS workspaces:** You can use existing workspaces and switch
  between them with keyboard or touchpad gestures - with a separate window strip
  on each. Drag and dropping windows between them works as well.
- **Virtual Workspaces (Experimental):** Group your windows into tasks by
  stacking multiple horizontal strips (rows) within a single space. Use native
  macOS workspaces for broad segregation (e.g., 'Work', 'Personal') and virtual
  workspaces to stay organized within each context.
- **Focus follows mouse on MacOS:** Very useful for people who would like to
  avoid an extra click.
- **Sliding windows with touchpad:** Using a touchpad is quite natural for
  navigation of the window pane.
- **Native macOS tabs support:** Applications like Ghostty use these, so
  Paneru manages them on the layout strip like other windows.
- **Optimal for Large Displays:** Standard tiling window managers can be
  suboptimal for large displays, often resulting in either huge maximized
  windows or numerous tiny, unusable windows. Paneru addresses this by
  providing a more flexible and practical arrangement.
- **Improved Small Display Usability:** On smaller displays (like laptops),
  traditional tiling can make windows too small to be productive, forcing users
  to constantly maximize. Paneru's sliding strip approach aims to provide a
  better experience without this compromise.

## Inspiration

The fundamental architecture and window management techniques are heavily
inspired by [Yabai], another excellent MacOS window manager. Studying its
source code has provided invaluable insights into managing windows on MacOS,
particularly regarding undocumented functions.

The innovative concept of managing windows on a sliding strip is directly
inspired by [Niri] and [PaperWM.spoon].

## Installation

### Recommended System Options

- Like all non-native window managers for MacOS, Paneru requires accessibility
  access to move windows. Once it runs you may get a dialog window asking for
  permissions. Otherwise check the setting in System Settings under "Privacy &
  Security -> Accessibility".

- Check your System Settings for "Displays have separate spaces" option. It
  should be enabled - this allows Paneru to manage the workspaces independently.

- **Multiple displays**. Paneru is moving the windows off-screen, hiding them
  to the left or right. If you have multiple displays, for example your laptop
  open when docked to an external monitor you may experience weird behavior.
  The issue is that when MacOS notices a window being moved too far off-screen
  it will relocate it to a different display - which confuses Paneru! The
  solution is to change the spatial arrangement of your additional display -
  instead of having it to the left or right, move it above or below your main
  display.
  A [similar situation](https://nikitabobko.github.io/AeroSpace/guide#proper-monitor-arrangement)
  exists with Aerospace window manager.

- **Off-screen window slivers**. Because macOS will forcibly relocate windows
  that are moved fully off-screen, Paneru keeps a thin sliver of each
  off-screen window visible at the screen edge. The `sliver_width` and
  `sliver_height` options control the size of this sliver. This is a
  workaround for a macOS limitation, not a design choice.

### Installing from Crates.io

Paneru is built using Rust's `cargo`. It can be installed directly from
`crates.io` or if you need the latest version, by fetching the source from Github.

```shell
$ cargo install paneru
```

### Installing from Github

```shell
$ git clone https://github.com/karinushka/paneru.git
$ cd paneru
$ cargo build --release
$ cargo install --path .
```

It can run directly from the command line or as a service.
Note, that you will need to grant acessibility priviledge to the binary.

### Installing with Homebrew

If you are using Homebrew, you can install from the formula with:

```shell
$ brew install paneru
```

Or by first adding the tap and then installing by name:

```shell
$ brew tap karinushka/paneru
$ brew install paneru
```

### Installing with Nix

See [`nix/README.md`](/nix/README.md).

### Configuration

Paneru checks for configuration in following locations:

- `$HOME/.paneru`
- `$HOME/.paneru.toml`
- `$XDG_CONFIG_HOME/paneru/paneru.toml`

Additionally it allows overriding the location with `$PANERU_CONFIG` environment variable.

You can use the following basic configuration as a starting point. For a
complete guide to all available options, keybindings, and window rules, see the
**[Configuration Guide](./CONFIGURATION.md)**.

```toml
# basic .paneru.toml
[options]
focus_follows_mouse = true
mouse_follows_focus = true

[bindings]
window_focus_west = "cmd - h"
window_focus_east = "cmd - l"
window_resize = "alt - r"
window_center = "alt - c"
quit = "ctrl + alt - q"
```

### Live reloading

Configuration changes made to your `~/.paneru` file are automatically reloaded
while Paneru is running. This is useful for tweaking keyboard bindings and
other settings without restarting the application.

### Running as a service

```shell
$ paneru install
$ paneru start
```

### Running in the foreground

```shell
$ paneru
```

### Sending Commands

Paneru exposes a `send-cmd` subcommand that lets you control the running
instance from the command line via a Unix socket (`/tmp/paneru.socket`). Any
command that can be bound to a hotkey can also be sent programmatically:

```shell
$ paneru send-cmd <command> [args...]
```

#### Available commands

| Command                    | Description                                      |
| -------------------------- | ------------------------------------------------ |
| `window focus <direction>` | Move focus to a window in the given direction    |
| `window swap <direction>`  | Swap the focused window with a neighbour         |
| `window center`            | Center the focused window on screen              |
| `window resize`            | Cycle through `preset_column_widths`             |
| `window grow`              | Grow to the next preset width                    |
| `window shrink`            | Shrink to the previous preset width              |
| `window fullwidth`         | Toggle full-width mode for the focused window    |
| `window manage`            | Toggle managed/floating state                    |
| `window equalize`          | Distribute equal heights in the focused stack    |
| `window stack`             | Stack the focused window onto its left neighbour |
| `window unstack`           | Unstack the focused window into its own column   |
| `window nextdisplay`       | Move the focused window to the next display      |
| `window nextdisplaysend`   | Move the window to the next display but stay here |
| `window virtual <dir>`     | Switch to the previous/next virtual workspace     |
| `window virtualmove <dir>` | Move the window to a different virtual workspace  |
| `window virtualsend <dir>` | Send the window to a virtual workspace but stay  |
| `window snap`              | Snap the focused window into the visible viewport |
| `mouse nextdisplay`        | Warp the mouse pointer to the next display       |
| `printstate`               | Print the internal ECS state to the debug log    |
| `quit`                     | Quit Paneru                                      |

Where `<direction>` is one of: `west`, `east`, `north`, `south`, `first`, `last`.

#### Examples

```shell
# Move focus one window to the right.
$ paneru send-cmd window focus east

# Swap the current window to the left.
$ paneru send-cmd window swap west

# Center and resize in one shot (two separate calls).
$ paneru send-cmd window center && paneru send-cmd window resize

# Cycle backward through preset widths.
$ paneru send-cmd window shrink

# Jump to the left-most window.
$ paneru send-cmd window focus first
```

#### Scripting ideas

Because `send-cmd` works over a Unix socket, you can drive Paneru from shell
scripts, `cron` jobs, or other automation tools:

- **Launch-and-arrange workflow.** Open an application and immediately position
  it: `open -a Safari && sleep 0.5 && paneru send-cmd window resize`.
- **One-key layout reset.** Bind a script that focuses the first window, resizes
  it, then moves east and resizes the next one — recreating a preferred layout
  after windows get shuffled.
- **Integration with other tools.** Pipe focus events from tools like
  [Hammerspoon](https://www.hammerspoon.org) or
  [skhd](https://github.com/koekeishiya/skhd) into `paneru send-cmd` for
  compound actions that go beyond a single hotkey.
- **Multi-display orchestration.** Move a window to the next display and
  immediately warp the mouse there:
  ```shell
  paneru send-cmd window nextdisplay && paneru send-cmd mouse nextdisplay
  ```


## Future Enhancements

- More commands for manipulating windows: finegrained size adjustments, touchpad resizing, etc.
- Scriptability. For example using Lua for configuration or automation of window handling,
  like triggering and positioning specific windows or applications.

## Communication

There is a public Matrix room
[`#paneru:matrix.org`](https://matrix.to/#/%23paneru%3Amatrix.org). Join and
ask any questions.

## Architecture Overview

For a detailed high-level overview of Paneru's internal design, data flow, and
ECS patterns, please refer to the **[Architecture Guide](./ARCHITECTURE.md)**.

Paneru's architecture is built around the **Bevy ECS (Entity Component
System)**, which manages the window manager's state as a collection of entities
(displays, workspaces, applications, and windows) and components.

The system is decoupled into three primary layers:

1.  **Platform Layer (`src/platform/`)**: Directly interfaces with macOS via `objc2` and Core Graphics. It runs the native Cocoa event loop and pumps OS events into a channel consumed by Bevy.
2.  **Management Layer (`src/manager/`)**: Defines OS-agnostic traits (`WindowManagerApi`, `WindowApi`) that abstract window manipulation. The macOS-specific implementations (`WindowManagerOS`, `WindowOS`) bridge these traits to the Accessibility and SkyLight APIs.
3.  **ECS Layer (`src/ecs/`)**: The "brain" of the application. Bevy systems process incoming events, handle input triggers, and manage animations.

### Repository Structure

- **`main` branch**: Contains the stable, released code.
- **`testing` branch**: Used for experimental features and architectural refactors. This branch is volatile and may be force-pushed.

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
