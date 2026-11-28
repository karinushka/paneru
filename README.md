<div align="center">
  <img src="./images/paneru.png" alt="Paneru" width="600"/>
</div>

A sliding, tiling window manager for macOS.

## About

Paneru is a macOS window manager that arranges windows on an infinite strip,
extending to the right. A core principle is that opening a new window will
**never** cause existing windows to resize, maintaining your layout stability.

Each monitor operates with its own independent window strip, ensuring that
windows remain confined to their respective displays and do not "overflow" onto
adjacent monitors.

https://github.com/user-attachments/assets/cbc2e820-635f-408b-923a-6cb47c44704c

(Video by @emreekici3 - https://github.com/emreekici3/dotfiles)

https://github.com/user-attachments/assets/793e7eaa-7909-4086-8380-1fb7861f8780


## Why Paneru?

- **Niri-like Behavior on macOS:** Inspired by the user experience of [Niri],
  Paneru aims to bring a similar scrollable tiling workflow to macOS.
- **Native macOS Spaces control:** Use existing macOS Spaces with an independent
  window strip on each — switch instantly without slide animations, create,
  destroy, reorder, or swap Spaces, move or pin windows across them, and
  drag-and-drop windows between them.
- **Virtual Workspaces:** Group your windows into tasks by stacking multiple
  horizontal strips (rows) within a single space. Use native macOS workspaces
  for broad segregation (e.g., 'Work', 'Personal') and virtual workspaces to
  stay organized within each context.
- **Workspace Overview:** Zoom out to a live-preview grid of all windows and
  virtual workspaces via a four-finger touchpad swipe or the `overview` command,
  with keyboard navigation and a dedicated shelf for floating windows.
- **Menu bar workspace indicator:** Shows the currently active virtual
  workspace in the macOS menu bar.
- **Startup session restore:** Restores managed window layouts, virtual
  workspaces, and display assignments from the last saved state when Paneru
  starts.
- **Focus follows mouse on macOS:** Very useful for people who would like to
  avoid an extra click.
- **Sliding windows with touchpad:** Using a touchpad is quite natural for
  navigation of the window pane.
- **Title-bar reordering:** Drag a tiled window by its title bar to move its
  column within the strip or to another display. The display under the pointer
  opens a slot in its visible strip. Pressing Escape returns the column to its
  original slot.
- **Stacked & tabbed columns:** Stack windows vertically inside a column, or
  switch any stack to tabbed display with visual tab indicators.
- **Native macOS tabs support:** Applications like Ghostty use these, so
  Paneru manages them on the layout strip like other windows.
- **Full Lua scripting:** Configure and automate window management using the
  embedded Lua runtime (`init.lua`) or the loadable `paneru.so` module from
  external scripts — see the **[Lua Scripting Guide](./SCRIPTING.md)**.
- **JSON queries & event stream:** Query live state or stream status events as
  JSON (`paneru query` / `paneru subscribe`) for menu bars and other macOS
  tools — see the
  **[Query & Subscribe Format](./QUERY_AND_SUBSCRIBE_FORMAT.md)**.
- **Copy Window Rule:** A menu bar entry (and `window copyrule` command) that
  puts a ready-to-paste window rule (TOML or Lua) for the focused window on the
  clipboard, so you never have to guess an app's bundle id.
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
inspired by [Yabai], another excellent macOS window manager. Studying its
source code has provided invaluable insights into managing windows on macOS,
particularly regarding undocumented functions.

The innovative concept of managing windows on a sliding strip is directly
inspired by [Niri] and [PaperWM.spoon].

## Installation

### Recommended System Options

- Like all non-native window managers for macOS, Paneru requires accessibility
  access to move windows. Once it runs you may get a dialog window asking for
  permissions. Otherwise check the setting in System Settings under "Privacy &
  Security -> Accessibility" (or "Privacy & Security -> Device Control and Data
  Access" on macOS 27 and later).

- Check your System Settings for "Displays have separate spaces" option. It
  should be enabled - this allows Paneru to manage the workspaces independently.

- **Native macOS Desktop controls (`desktop_*`) on macOS 27.3+.** Starting with
  macOS 27.3, `WindowManager` restricts bridged Space operations
  (`SLSBridged*Operation`) to Apple-entitled clients, rejecting calls from
  third-party binaries when SIP is enabled. Paneru automatically detects this
  and disables `desktop_*` mutations (and `[windows]` `desktop` placement rules)
  without repeated timeouts; read-only Desktop queries (`paneru query desktops`)
  and Paneru's virtual workspaces (`window_virtual_*`) continue to work normally
  with SIP enabled.

- **Multiple displays**. Paneru is moving the windows off-screen, hiding them
  to the left or right. If you have multiple displays, for example your laptop
  open when docked to an external monitor you may experience weird behavior.
  The issue is that when macOS notices a window being moved too far off-screen
  it will relocate it to a different display - which confuses Paneru! The
  solution is to change the spatial arrangement of your additional display -
  instead of having it to the left or right, move it above or below your main
  display.
  A [similar situation](https://nikitabobko.github.io/AeroSpace/guide#proper-monitor-arrangement)
  exists with Aerospace window manager.
  An option exists (`horizontal_mouse_warp`) which can make a vertical
  arrangement of displays "feel" horizontal.

- **Off-screen window slivers**. Because macOS will forcibly relocate windows
  that are moved fully off-screen, Paneru keeps a thin sliver of each
  off-screen window visible at the screen edge. The `sliver_width` and
  `sliver_height` options control the size of this sliver. This is a
  workaround for a macOS limitation, not a design choice.

### Installing from Crates.io

Paneru is built using Rust's `cargo`. It can be installed directly from
`crates.io` or if you need the latest version, by fetching the source from GitHub.

```shell
$ cargo install paneru
```

### Installing from GitHub

```shell
$ git clone https://github.com/karinushka/paneru.git
$ cd paneru
$ cargo build --release
$ cargo install --path .
```

By default, Paneru builds with the embedded Lua runtime using a vendored LuaJIT compiled from source, requiring no system-wide Lua installation or `pkg-config` setup.

If you prefer to link against a system- or version-manager-installed Lua (e.g. via `mise`, `asdf`, or Homebrew), disable default features and pass `--features lua`, making sure `PKG_CONFIG_PATH` contains the directory with `luajit.pc`:

```shell
# When using mise:
$ PKG_CONFIG_PATH="$(mise where luajit)/lib/pkgconfig:$PKG_CONFIG_PATH" cargo build --release --no-default-features --features lua

# When using Homebrew:
$ cargo build --release --no-default-features --features lua
```

It can run directly from the command line or as a service.
Note that you will need to grant accessibility privileges to the binary.

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

See [`nix/README.md`](./nix/README.md).

### Configuration

Paneru checks for configuration in the following locations:

- `$HOME/.paneru`
- `$HOME/.paneru.toml`
- `$XDG_CONFIG_HOME/paneru/paneru.toml`

Set `$PANERU_CONFIG` to use one explicit TOML path.
If there is no Lua script and the TOML file is missing, Paneru creates it at that path with the built-in defaults.
Without an explicit path, a build with Lua creates `$XDG_CONFIG_HOME/paneru/init.lua` when no configuration exists.
A build without Lua creates `$XDG_CONFIG_HOME/paneru/paneru.toml` instead.

A Lua script (`$PANERU_LUA`, `$HOME/.paneru.lua`, or
`$XDG_CONFIG_HOME/paneru/init.lua`, checked in that order) replaces the TOML
rather than layering on top of it: when one exists, no `paneru.toml` is read,
created, or watched.

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

Alternatively, the embedded Lua runtime can declare the entire configuration
via `paneru.setup{...}`, making the TOML file optional — see the
**[Lua Scripting Guide](./SCRIPTING.md)**.

The `paneru` table is available globally in `init.lua`; `require("paneru")`
also returns it. No separate `paneru.so` is needed for Paneru's own config.

```lua
-- init.lua
paneru.setup {
  options = { focus_follows_mouse = true, mouse_follows_focus = true },
  bindings = {
    ["window focus west"] = "cmd - h",
    ["window focus east"] = "cmd - l",
    ["quit"] = "ctrl + alt - q",
  },
}
```

### Live reloading

Changes made to the active configuration file are automatically reloaded while
Paneru is running. This is useful for tweaking keyboard bindings and other
settings without restarting the application.

### Startup session restore

Paneru saves managed window layout state to the user state directory
(`$XDG_STATE_HOME/paneru/state.json`, usually
`~/.local/state/paneru/state.json`) and loads it when Paneru starts. During the
startup restore window, Paneru matches reopened windows to the saved session and
restores their layout placement, virtual workspace row, and display assignment
where possible.

Restore is startup-only. After the configured startup grace period expires, new
or unmatched windows follow the normal configuration and window-rule behavior.
Saved windows that are not present are ignored by default and the restored
layout is compacted around the windows that were found. The behavior is
configured with `[restore]`; see the
**[Session Restore](./CONFIGURATION.md#session-restore)** section in the
configuration guide.

### Running as a service

```shell
$ paneru install
$ paneru start
```

### Installing an app launcher

To start Paneru from Spotlight, Alfred, Raycast, or another application launcher,
install the lightweight app wrapper:

```shell
$ paneru install-app
```

This creates `$HOME/Applications/Paneru.app`. Opening the app starts the
installed Paneru launch agent and exits immediately. Remove the wrapper with:

```shell
$ paneru uninstall-app
```

### Running in the foreground

```shell
$ paneru
```

### Sending Commands

Paneru exposes a `send-cmd` subcommand that lets you control the running
instance from the command line over a Mach service
(`com.github.karinushka.paneru`). Any
command that can be bound to a hotkey can also be sent programmatically:

```shell
$ paneru send-cmd <command> [args...]
```

#### Available commands

| Command                    | Description                                      |
| -------------------------- | ------------------------------------------------ |
| `window focus <direction\|number\|managed\|unmanaged>` | Move focus by direction, column number, managed or unmanaged. First, last, and numbered jumps restore the last focused window in a split stack |
| `window swap <direction>`  | Swap the focused window with a neighbour         |
| `window center`            | Center the focused window on screen              |
| `window snap`              | Snap the focused window into the visible viewport |
| `window resize`            | Cycle through `preset_column_widths`             |
| `window grow`              | Grow to the next preset width                    |
| `window shrink`            | Shrink to the previous preset width              |
| `window vertical resize`   | Cycle a stacked window through `preset_stack_heights` |
| `window vertical grow`     | Grow a stacked window to the next preset height  |
| `window vertical shrink`   | Shrink a stacked window to the previous preset height |
| `window step [grow\|shrink]` | Change the column width by `window_resize_step` |
| `window vertical step [grow\|shrink]` | Change a stacked window's height by `window_resize_step` |
| `window fullwidth`         | Toggle full-width mode for the focused window    |
| `window manage`            | Toggle managed/floating state                    |
| `window raise floating`    | Raise floating windows above tiled windows       |
| `window togglefloatlayer`  | Toggle the floating layer above or below tiled windows |
| `window equalize`          | Distribute equal heights in the focused stack    |
| `window balance`           | Make all columns match the focused window width  |
| `window stack`             | Stack the focused window onto its left neighbour |
| `window unstack`           | Unstack the focused window into its own column   |
| `window consume`           | Absorb the column to the right into the focused stack |
| `window expel`             | Move the focused window out of its stack into a column to the right |
| `window tabbeddisplay`     | Toggle tabbed display mode for the focused stack |
| `window copyrule`          | Copy a window rule snippet for the focused window to the clipboard |
| `window nextdisplay`       | Move the focused window to the next display      |
| `window nextdisplaysend`   | Move the window to the next display but stay here |
| `window focusdisplay <direction>` | Focus the display to the north, south, west, or east |
| `window movedisplay <direction>`  | Move the focused window to that display and follow it |
| `window displaymove <id>`  | Move the focused window to display `<id>`        |
| `window displaysend <id>`  | Move the window to display `<id>` but stay here  |
| `window virtual <dir\|n>`  | Switch to the previous/next or numbered virtual workspace |
| `window virtualfocus <dir>` | Focus an adjacent stacked window, or switch virtual workspace at the stack edge |
| `window virtualnum <n>`    | Switch directly to numbered virtual workspace    |
| `window virtualadd`        | Create and switch to a new virtual workspace     |
| `window virtualmove <dir\|n>` | Move the window to a virtual workspace and follow it |
| `window virtualmovenum <n>` | Move the window to numbered virtual workspace and follow it |
| `window virtualsend <dir\|n>` | Send the window to a virtual workspace but stay |
| `window virtualsendnum <n>` | Send the window to numbered virtual workspace but stay |
| `desktop focus <dir\|n>`   | Instantly switch native macOS Desktop/Space without slide animations |
| `desktop move <dir\|n>`    | Move focused window (or tab group) to target Desktop and follow it |
| `desktop send <dir\|n>`    | Send focused window (or tab group) to target Desktop without switching |
| `desktop create [focus]`   | Create a new native Desktop on the active display (optionally focusing it) |
| `desktop destroy [dir\|n] [empty]` | Close current or target Desktop (migrating windows unless `empty`) |
| `desktop reorder <dir\|n>` | Move active Desktop to display-local Desktop position |
| `desktop swap <dir\|n>`    | Swap active Desktop with another Desktop on the same display |
| `desktop pin <all\|dir\|n>` | Pin focused window across all or another Desktop (auto-floats) |
| `desktop unpin`            | Collapse a pinned window back onto the active Desktop |
| `desktop assign <all\|this\|none\|n>` | Ephemeral Dock-equivalent app Space assignment for focused app |
| `mouse nextdisplay`        | Warp the mouse pointer to the next display       |
| `mouse display <id>`       | Warp the mouse pointer to display `<id>`         |
| `overview`                 | Toggle the workspace overview                    |
| `showmenu` / `hidemenu`    | Show or hide the Paneru menu bar icon            |
| `printstate`               | Print the internal ECS state to the debug log    |
| `quit`                     | Quit Paneru                                      |
| `restart`                  | Restart the Paneru service                       |

Where `<direction>` is one of: `west`, `east`, `north`, `south`, `first`, `last`.
Window numbers are 1-based and count columns from left to right.

#### Examples

```shell
# Move focus one window to the right.
$ paneru send-cmd window focus east

# Swap the current window to the left.
$ paneru send-cmd window swap west

# Center and resize in one shot (two separate calls).
$ paneru send-cmd window center && paneru send-cmd window resize

# Balance all columns to the focused window's width.
$ paneru send-cmd window balance

# Cycle backward through preset widths.
$ paneru send-cmd window shrink

# Grow the focused window's height inside its stack.
$ paneru send-cmd window vertical grow

# Jump to the left-most window.
$ paneru send-cmd window focus first

# Jump to the second window from the left.
$ paneru send-cmd window focus 2

# Switch directly to virtual workspace 3.
$ paneru send-cmd window virtualnum 3

# Send the focused window to virtual workspace 3 without following it.
$ paneru send-cmd window virtualsendnum 3
```

### Querying and Subscribing to State

Paneru also exposes structured JSON state for scripts and status bars:

```shell
$ paneru query state --json
$ paneru query virtual-workspaces --json
$ paneru query desktops --json
$ paneru query active --json
$ paneru subscribe --json
```

`query` prints a JSON snapshot and exits. `subscribe --json` keeps the channel
open and emits line-delimited JSON events for changes that integrations usually
care about, including focus changes, virtual workspace changes, window-list
changes, title changes, and display changes. See
[`QUERY_AND_SUBSCRIBE_FORMAT.md`](./QUERY_AND_SUBSCRIBE_FORMAT.md) for the
full payload contract.

Each window entry includes its accessibility `role` and `subrole` (e.g.
`AXStandardWindow`, `AXDialog`), which
[window rules](./CONFIGURATION.md#6-window-rules-windows) can also match on.

#### Scripting ideas

Because `send-cmd` talks to the running daemon, you can drive Paneru from shell
scripts, `cron` jobs, or other automation tools:

- **Launch-and-arrange workflow.** Open an application and immediately position
  it: `open -a Safari && sleep 0.5 && paneru send-cmd window resize`.
- **One-key layout reset.** Use `paneru send-cmd window balance` to make every
  column the same width as the focused window — great for resetting layouts
  after unplugging a monitor or when windows get shuffled.
- **Integration with other tools.** Pipe focus events from tools like
  [Hammerspoon](https://www.hammerspoon.org) or
  [skhd](https://github.com/koekeishiya/skhd) into `paneru send-cmd` for
  compound actions that go beyond a single hotkey.
- **Multi-display orchestration.** Move a window to the next display and
  immediately warp the mouse there:
  ```shell
  paneru send-cmd window nextdisplay && paneru send-cmd mouse nextdisplay
  ```
- **Status bar integration.** Use `paneru query state --json` to render the
  initial workspace labels, then keep them current with `paneru subscribe --json`.

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
- [PaperWM.spoon]: scrollable tiling for macOS on top of Hammerspoon.
- [Nehir]: scrollable tiling for macOS.

[Yabai]: https://github.com/koekeishiya/yabai
[Niri]: https://github.com/YaLTeR/niri
[PaperWM]: https://github.com/paperwm/PaperWM
[karousel]: https://github.com/peterfajdiga/karousel
[papersway]: https://spwhitton.name/tech/code/papersway/
[hyprscroller]: https://github.com/dawsers/hyprscroller
[hyprslidr]: https://gitlab.com/magus/hyprslidr
[PaperWM.spoon]: https://github.com/mogenson/PaperWM.spoon
[Nehir]: https://github.com/Guria/Nehir
