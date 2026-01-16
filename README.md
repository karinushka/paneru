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
- **Focus follows mouse on MacOS:** Very useful for people who would like to
  avoid an extra click.
- **Sliding windows with touchpad:** Using a touchpad is quite natural for
  navigation of the window pane.
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

### Installing with Nix

Add the paneru flake to your inputs.

```nix
# flake.nix
inputs.paneru = {
  url = "github:karinushka/paneru";
  inputs.nixpkgs.follows = "nixpkgs";
}
```

#### Home Manager

Paneru provides a home manager module to install and configure paneru.

> [!NOTE]
> You still need to enable accessibility permissions in the macOS settings
> the first time paneru is launched or any time it is updated.

```nix
# home.nix
{ inputs, ... }:

{
  imports = [
    inputs.paneru.homeModules.paneru
  ];

  services.paneru = {
    enable = true;
    # Equivalent to what you would put into `~/.paneru` (See Configuration options below).
    settings = {
      options = {
        preset_column_widths = [
          0.25
          0.33
          0.5
          0.66
          0.75
        ];
        swipe_gesture_fingers = 4;
        animation_speed = 4000;
      };
      bindings = {
        window_focus_west = "cmd - h";
        window_focus_east = "cmd - l";
        window_focus_north = "cmd - k";
        window_focus_south = "cmd - j";
        window_swap_west = "alt - h";
        window_swap_east = "alt - l";
        window_swap_first = "alt + shift - h";
        window_swap_last = "alt + shift - l";
        window_center = "alt - c";
        window_resize = "alt - r";
        window_fullwidth = "alt - f";
        window_manage = "ctrl + alt - t";
        window_stack = "alt - ]";
        window_unstack = "alt + shift - ]";
        quit = "ctrl + alt - q";
      };
    };
  };
}
```

#### Standalone Package

Although we strongly recommend using home manager, the paneru flake also exposes a standalone package.

```nix
{ inputs, ... }:

{
  # nix-darwin configuration (configuration.nix)
  # system-wide
  environment.systemPackages = [ inputs.paneru.packages.paneru ]
  # or per-user
  users.users."<name>".packages = [ inputs.paneru.packages.paneru ]
}
```

### Configuration

Paneru checks for configuration in following locations:

* `$HOME/.paneru`
* `$HOME/.paneru.toml`
* `$XDG_CONFIG_HOME/paneru/paneru.toml`

Additionally it allows overriding the location with `$PANERU_CONFIG` environment variable.

You can use the following example configuration as a starting point:

```
# syntax=toml
#
# Example configuration for Paneru.
#
[options]
# Enables focus follows mouse. Enabled by default, set to false to disable.
# focus_follows_mouse = true

# Enables mouse follows focus. Enabled by default, set to false to disable.
# mouse_follows_focus = true

# Array of widths used by the `window_resize` action to cycle between.
# Defaults to 25%, 33%, 50%, 66% and 75%.
preset_column_widths = [ 0.25, 0.33, 0.50, 0.66, 0.75 ]

# How many fingers to use for moving windows left and right.
# Make sure that it doesn't clash with OS setting for workspace switching.
# Values lower than 3 will be ignored.
# Remove the line to disable the gesture feature.
# Apple touchpads support gestures with more than five fingers (!),
# but it is probably not that useful to use two hands :)
swipe_gesture_fingers = 4

# Swiping the windows left and right will keep sliding them until the first or
# last window. Set to false to always keep the current window on screen and
# fully exposed. Enabled by default.
# continuous_swipe = true

# Animation speed in 1/10th of display resolution per second.
# E.g. a value of 20 means: move at a speed of two display sizes per second.
# To disable animations, leave this unset or set to a very large value.
animation_speed = 50

[bindings]
# Moves the focus between windows.
window_focus_west = ["cmd - h", "cmd - leftarrow"]
window_focus_east = ["cmd - l", "cmd - rightarrow"]
window_focus_north = ["cmd - k", "cmd - uparrow"]
window_focus_south = ["cmd - j", "cmd - downarrow"]

# Swaps windows in chosen direction.
window_swap_west = "alt - h"
window_swap_east = "alt - l"

# Jump to the left-most or right-most windows.
window_focus_first = "cmd + shift - h"
window_focus_last = "cmd + shift - l"

# Move the current window into the left-most or right-most positions.
window_swap_first = "alt + shift - h"
window_swap_last = "alt + shift - l"

# Centers the current window on screen.
window_center = "alt - c"

# Cycles between the window sizes defined in the `preset_column_widths` option.
window_resize = "alt - r"

# Toggle full width for the current focused window.
window_fullwidth = "alt - f"

# Toggles the window for management. If unmanaged, the window will be "floating".
window_manage = "ctrl + alt - t"

# Stacks and unstacks a window into the left column. Each window gets a 1/N of the height.
window_stack = "alt - ]"
window_unstack = "alt + shift - ]"

# Moves currently focused window to another display.
window_nextdisplay = "alt - n"

# Quits the window manager.
quit = "ctrl + alt - q"

# Window properties, matched by a RegExp title string.
[windows]

[windows.pip]
# Title RegExp pattern is required.
title = "Picture.*(in)?.*[Pp]icture"
# Do not manage this window, e.g. it will be floating.
floating = true

[windows.neovide]
# Matches an editor and always inserts its window at index 1.
title = ".*"
bundle_id = "com.neovide.neovide"
index = 1

[windows.all]
# Matches all windows and adds a few pixels of spacing to their borders.
title = ".*"
horizontal_padding = 4
vertical_padding = 2
```

Paste this into your terminal to create a default configuration file:

```
$ cat > ~/.paneru <<EOF

# ... paste the above configuration here ...

EOF
```

**Live Reloading:** Configuration changes made to your `~/.paneru` file are
automatically reloaded while Paneru is running. This is extremely useful for
tweaking keyboard bindings and other settings without restarting the
application. The settings can be changed while Paneru is running - they will
be automatically reloaded.

### Running as a service

```shell
$ paneru install
$ paneru start
```

### Running in the foreground

```shell
$ paneru
```


## Future Enhancements

- More commands for manipulating windows: fullscreen, finegrained size adjustments, etc.
- Scriptability. A nice feature would be to use Lua for configuration and simple scripting,
  like triggering and positioning specific windows or applications.

## Communication

There is a public Matrix room [`#paneru:matrix.org`](https://matrix.to/#/%23paneru%3Amatrix.org). Join and ask any questions.

## Architecture Overview

The overall architecture is layered, with a platform interaction layer at the base.
This bottom layer, primarily within `platform.rs`, interfaces directly with the MacOS operating system via Objective-C and Core Graphics APIs.
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
