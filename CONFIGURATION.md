# Configuration Guide

Paneru is configured via a TOML file. By default, it looks for the configuration in the following locations (in order):

1.  `$PANERU_CONFIG` (environment variable)
2.  `$HOME/.paneru`
3.  `$HOME/.paneru.toml`
4.  `$XDG_CONFIG_HOME/paneru/paneru.toml`

The configuration is automatically reloaded when the file is saved.

---

## 1. Global Options (`[options]`)

General behavior settings for the window manager.

| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `focus_follows_mouse` | Boolean | `true` | If enabled, the window under the mouse cursor will automatically gain focus. |
| `mouse_follows_focus` | Boolean | `true` | If enabled, the mouse cursor will warp to the center of the focused window when focus changes via keyboard. |
| `preset_column_widths` | Array (Float) | `[0.25, 0.33, 0.5, 0.66, 0.75]` | Ratios of the screen width used by the `window_resize` command to cycle sizes. |
| `animation_speed` | Float | `50` | Speed of window animations (1/10th of screen size per second). Set to a very high value to effectively disable animations. |
| `auto_center` | Boolean | `false` | Automatically center the focused window on the screen when switching focus. |
| `sliver_height` | Float (0.1–1.0) | `1.0` | Vertical ratio of off-screen windows kept visible to prevent macOS from relocating them. |
| `sliver_width` | Integer (px) | `5` | Horizontal width of off-screen windows kept visible. |
| `menubar_height` | Integer (px) | *Auto* | Manually override the detected macOS menubar height. |
| `window_hidden_ratio` | Float (0.0–1.0) | `0.0` | How much of a window can be hidden before it's forced into view on focus change. `0.0` = eager, `1.0` = lazy. |

---

## 2. Padding (`[padding]`)

Sets the margins at the edges of the screen.

| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `top` | Integer (px) | `0` | Padding at the top of the screen. |
| `bottom` | Integer (px) | `0` | Padding at the bottom of the screen. |
| `left` | Integer (px) | `0` | Padding at the left edge. |
| `right` | Integer (px) | `0` | Padding at the right edge. |

---

## 3. Swipe & Gestures (`[swipe]`)

Configure trackpad gestures and scroll-wheel window sliding.

| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `sensitivity` | Float (0.1–2.0) | `0.35` | Multiplier for swipe distance. |
| `deceleration` | Float (1.0–10.0) | `4.0` | Rate at which inertia slows down after a swipe. |

### `[swipe.gesture]`
| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `fingers_count` | Integer | *None* | Number of fingers for the swipe gesture. Set to 3 or more to enable. |
| `direction` | String | `"Natural"` | Direction of movement: `"Natural"` or `"Reversed"`. |

### `[swipe.scroll]`
| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `modifier` | String | `"alt"` | Modifier key(s) required to slide windows with the scroll wheel: `"alt"`, `"cmd"`, `"alt + cmd"`, `"ctrl + alt + cmd"`, etc. |

---

## 4. Decorations (`[decorations]`)

Visual styling for active and inactive windows.

### `[decorations.inactive.dim] (Native macOS Dimming)`

Paneru supports native macOS window dimming. To use this mode, **only** set `opacity` (and optionally `opacity_night`). Do not set a `color`.

| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `opacity` | Float (-1.0 to 1.0) | `0.0` | Dimming intensity. `-1.0` is fully black, `1.0` is fully white. |
| `opacity_night` | Float (-1.0 to 1.0) | *opacity* | Dimming intensity used when macOS is in Dark Mode. |

**Example:**
```toml
[decorations.inactive.dim]
opacity = -0.15
opacity_night = -0.25
```

---

## 5. Keybindings (`[bindings]`)

Bindings map a key combination to an action. A binding can be a single string or an array of strings.
Format: `"[modifiers-]key"`. Available modifiers are:
- `alt`, `lalt`, `ralt`
- `ctrl`, `lctrl`, `rctrl`
- `cmd`, `lcmd`, `rcmd`
- `shift`, `lshift`, `rshift`

| Action | Description |
| :--- | :--- |
| `window_focus_west` / `_east` | Focus window to the left/right. |
| `window_focus_north` / `_south` | Focus window above/below. If no window exists, switches focus to the display in that direction. |
| `window_focus_first` / `_last` | Jump to the start/end of the strip. |
| `window_swap_west` / `_east` | Swap current window with neighbor. |
| `window_swap_north` / `_south` | Swap current window above/below. If no window exists, moves the window to the display in that direction. |
| `window_swap_first` / `_last` | Move current window to start/end of strip. |
| `window_center` | Center the current window in the viewport. |
| `window_resize` | Cycle through preset widths (Grow). |
| `window_shrink` | Cycle through preset widths (Shrink). |
| `window_fullwidth` | Toggle full-width mode. |
| `window_manage` | Toggle between tiled and floating state. |
| `window_stack` | Stack the current window into the column on the left. |
| `window_unstack` | Pull a window out of a stack into its own column. |
| `window_equalize` | Make all windows in a stack equal height. |
| `window_nextdisplay` | Move focused window to the next monitor. |
| `mouse_nextdisplay` | Warp mouse cursor to the next monitor. |
| `window_snap` | Snap an overflowing window into the viewport. |
| `quit` | Exit Paneru. |

**Example:**
```toml
[bindings]
window_focus_west = "cmd - h"
window_resize = ["alt - r", "ctrl - r"]
```

---

## 6. Window Rules (`[windows]`)

Define specific behaviors for applications based on their Title or Bundle ID.

| Option | Type | Description |
| :--- | :--- | :--- |
| `title` | Regex | **(Required)** Regex pattern to match the window title. |
| `bundle_id` | String | Optional Bundle ID to match (e.g., `com.apple.Terminal`). |
| `floating` | Boolean | Force the window to be floating/unmanaged. |
| `index` | Integer | Preferred position in the strip when spawned. |
| `dont_focus` | Boolean | Prevent the window from taking focus when spawned. |
| `width` | Float (0.0–1.0) | Initial width ratio for the window. |
| `grid` | String | placement for floating windows: `"cols:rows:x:y:w:h"`. |
| `horizontal_padding` | Integer | Gaps to the left/right of this window. |
| `vertical_padding` | Integer | Gaps to the top/bottom of this window. |
| `bindings_passthrough`| Array (String)| Keys that should bypass Paneru and go directly to the app. |

**Example:**
```toml
[windows.terminal]
title = ".*"
bundle_id = "com.apple.Terminal"
horizontal_padding = 5
bindings_passthrough = ["ctrl-h", "ctrl-l"]
```

---

## 7. Experimental Features

> [!WARNING]
> These features rely on undocumented macOS window-server APIs and have known issues. For example, overlay windows (like YouTube Picture-in-Picture) may be partially shaded, and layer ordering can behave unexpectedly. Both features are **disabled by default**. 
>
> Disabling **System Integrity Protection (SIP)** is **not required**, but without it Paneru has limited control over window layering, which is the root cause of most visual edge-cases. Enable these only if you are comfortable with occasional glitches.

### Inactive Window Overlay Dimming
Another dimming option that draws a translucent overlay on every inactive window to visually emphasize the focused one. 

**Activation:** This mode is enabled by setting **both** `opacity` and `color` under `[decorations.inactive.dim]`. In this mode, `opacity` ranges from `0.0` to `1.0`.

| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `opacity` | Float (0.0 to 1.0) | `0.0` | Opacity of the dim overlay. `0.0` is transparent, `1.0` is opaque. |
| `color` | String (Hex) | `"#000000"` | Hex color for the dim overlay (default: black). |

**Example:**
```toml
[decorations.inactive.dim]
opacity = 0.3
color = "#000000"
```

### Active Window Border
Draws a colored border around the currently focused window.

| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `enabled` | Boolean | `false` | Enable the active window border. |
| `color` | String (Hex) | `"#FFFFFF"` | Hex color for the active window border. |
| `opacity` | Float (0.0–1.0) | `1.0` | Opacity of the active window border. |
| `width` | Float (px) | `2.0` | Width of the border in pixels. |
| `radius` | Number/String | `"auto"` | Corner radius in pixels or `"auto"` to match system. |

**Example:**
```toml
[decorations.active.border]
enabled = true
color = "#89b4fa"
width = 2.0
radius = 12.0
```

> **Tip:** You can override the `border_radius` for specific applications in the `[windows]` section. See [Window Rules](#6-window-rules).
