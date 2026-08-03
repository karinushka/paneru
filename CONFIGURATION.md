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
| `horizontal_mouse_warp` | Integer ``(-1, 1)`` | Off | If enabled, the mouse will warp to another screen above or below, when touching the left or right edge. The direction depends on the direction - a negative value will cause the left edge to warp to a screen above and the right edge to a screen below. This allows having horizontal positioning of displays while having them aligned in a virtual layout in macOS settings. The cursor lands at the *opposite* edge of the target display (preserving cursor flow), with the source's relative Y position. Carries pre-warp horizontal velocity to avoid a "standing start", and skips the warp when the equivalent Y has no position on the target — matching macOS's native side-by-side behavior for displays of unequal height. (inspired by https://github.com/mogenson/WarpMouse.spoon) |
| `horizontal_mouse_warp_offset` | Integer (px) | `0` | Vertical pixel offset applied to the `horizontal_mouse_warp` landing position, signed by warp direction. Positive values shift the cursor lower when warping to a display *below* (in macOS arrangement) and higher when warping to one *above*. Use to compensate for physical desk arrangement differing from the macOS arrangement (e.g. portrait monitor sitting physically higher or lower than the laptop). |
| `preset_column_widths` | Array (Float) | `[0.25, 0.33, 0.5, 0.66, 0.75, 1.0, 1.5, 2.0]` | Ratios of the screen width used by the `window_resize` command and the menu bar width picker. Values above `1.0` create a horizontally scrollable oversized window. |
| `animation_speed` | Float | *None* | Speed of window animations. Comfortable range is from 8 to 20. Unset or set to a very high value to effectively disable animations. |
| `auto_center` | Boolean | `false` | Automatically center the focused window on the screen when switching focus. |
| `sliver_height` | Float (0.1–1.0) | `1.0` | Vertical ratio of off-screen windows kept visible to prevent macOS from relocating them. |
| `sliver_width` | Integer (px) | `5` | Horizontal width of off-screen windows kept visible. |
| `menubar_height` | Integer (px) | *Auto* | Manually override the detected macOS menubar height. |
| `window_hidden_ratio` | Float (0.0–1.0) | `0.0` | How much of a window can be hidden before it's forced into view on focus change. `0.0` = eager, `1.0` = lazy. |
| `window_resize_cycle` | Boolean | `true` | If disabled, `window_resize` and `window_shrink` stop at the largest/smallest preset instead of cycling back. |
| `mouse_resize_modifier` | String | *None* | If enabled allows window resizing using mouse movement. For example `cmd + shift` will allow resizing of the window when holding those keys. Proximity of the pointer to left or right window edge determines which side will be adjusted. |
| `reap_empty_workspaces` | String | `false` | If enabled, a virtual workspace without any windows will be removed. |
| `disable_native_tabs` | Boolean | `false` | If enabled, Paneru will not auto-merge a newly-spawned window into a tab group with an existing same-app sibling that shares its frame. Use this if you find unrelated windows being grouped together. |
| `virtual_workspace_animations` | Boolean | `false` | If enabled, Paneru will animate virtual workspace swaps. Off by default, because people use virtual workspaces due to the slow animation of the native macOS workspaces. |
| `insert_windows_mid_strip` | Boolean | `false` | When moving a window to another virtual workspace, insert it at the column matching its current on-screen position (keeping it where you see it and shifting the rest) instead of appending it to the end of the destination strip. |
| `create_virtual_workspace_automatically` | Boolean | `false` | Automatically creates a new virtual workspace when using `window_virtual_south `or Southward gesture controls. |

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
| `continuous` | Boolean | `true` | If `true`, the windows are allowed to fully move across the desktop, potentially exposing the empty desktop space. If `false`, the window strip will not move further than the left or right most window. This also affects the windows during keyboard focus - if `false` the left or right most windows will snap to the edge of display. |

### `[swipe.gesture]`
| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `fingers_count` | Integer | *None* | Number of fingers for the swipe gesture. Set to 3 or more to enable. |
| `direction` | String | `"Natural"` | Direction of movement: `"Natural"` or `"Reversed"`. |
| `vertical` | Boolean | `true` | Interpret the vertical gestures with `fingers_count` or ignore them. Enabling this allows using vertical swipe gestures to change virtual desktops. |

When `fingers_count` is omitted or set below 3, Paneru does not intercept native macOS gestures. If macOS uses three-finger horizontal swipes for Spaces, prefer `[swipe.scroll]` with a modifier or configure a different finger count.

### `[swipe.scroll]`
| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `modifier` | String | `"alt"` | Modifier key(s) required to slide windows with the scroll wheel: `"alt"`, `"rcmd"`, `"ralt + cmd"`, `"lctrl + lalt + cmd"`, etc. |
| `vertical_modifier` | String | *None* | Additional modifier key that, when held together with `modifier`, switches virtual workspaces vertically instead of scrolling horizontally. For example, if `modifier = "alt"` and `vertical_modifier = "shift"`, then `alt + scroll` slides windows horizontally and `alt + shift + scroll` switches virtual workspace rows. |

---

## 4. Decorations (`[decorations]`)

Visual styling for workspaces, active and inactive windows.

### Virtual Workspace indicators

Toggles display of the currently active virtual workspace in the menubar or in a brief status popup window. Both are enabled by default.
(Note: disabling menubar indicator requires a restart)

**Example:**
```toml
[decorations]
# Both default to true
workspace_menu_status = false
workspace_popup_status = true
```


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

Format: `"[modifiers-]key"`. For example `alt + cmd - j` and `cmd - j`.

Available modifiers are:
- `alt`, `lalt`, `ralt`
- `ctrl`, `lctrl`, `rctrl`
- `cmd`, `lcmd`, `rcmd`
- `shift`, `lshift`, `rshift`
- `fn`

For a full list of parseable keys (i.e. `leftarrow`) check the source:
https://github.com/karinushka/paneru/blob/3790b01f8d65df5d9000142db7cf25f9270dcccc/src/config.rs#L1466-L1601


### Window commands

| Action | Description |
| :--- | :--- |
| `window_focus_west` / `_east` | Focus window to the left/right. |
| `window_focus_north` / `_south` | Focus window above/below. If no window exists, switches focus to the display in that direction. |
| `window_focus_first` / `_last` | Jump to the start/end of the strip. |
| `window_focus_managed` | Switch to a previously focused window on this workspace. |
| `window_focus_unmanaged` | Switch to a previously focused floating window on this workspace. |
| `window_swap_west` / `_east` | Swap current window with neighbor. |
| `window_swap_north` / `_south` | Swap current window above/below. If no window exists, moves the window to the display in that direction. |
| `window_swap_first` / `_last` | Move current window to start/end of strip. |
| `window_center` | Center the current window in the viewport. |
| `window_resize` | Cycle through preset widths (Grow). |
| `window_grow` | Alias for `window_resize`. |
| `window_shrink` | Cycle through preset widths (Shrink). |
| `window_fullwidth` | Toggle full-width mode. |
| `window_manage` | Toggle between tiled and floating state. |
| `window_stack` | Stack the current window into the column on the left. |
| `window_unstack` | Pull a window out of a stack into its own column. |
| `window_equalize` | Make all windows in a stack equal height. |
| `window_balance` | Make all columns in the strip the same width as the focused window. |
| `window_nextdisplay` | Move focused window to the next monitor and follow it. |
| `window_nextdisplaysend` | Move focused window to the next monitor but stay on current. |
| `mouse_nextdisplay` | Warp mouse cursor to the next monitor. |
| `window_snap` | Snap an overflowing window into the viewport. |
| `window_raise_floating` | Make the floating windows layer visible on the current workspace. |
| `window_togglefloatlayer` | Selectively move the floating windows in front or behind of the workspace windows. |
| `quit` | Exit Paneru. |
| `restart` | Restart the Paneru service (`paneru restart`). |

**Example:**
```toml
[bindings]
window_focus_west = "cmd - h"
window_resize = ["alt - r", "ctrl - r"]
```

### Virtual workspaces (Experimental)

Paneru allows having virtual spaces inside of the native macOS workspace.
Logically it can be thought of several strips of windows (rows) stacked on top
of each other within the single workspace. Similar to how Niri implements the
movement between the vertical workspaces.

Shifting up or down goes to the previous or next strip of windows - wrapping
around at the start or the end.

Moving the last window out of the virtual row, will "collapse it".

Virtual workspaces can also be navigated using trackpad gestures. If `[swipe.gesture]` is configured, a vertical 3/4-finger swipe will switch between virtual workspace rows, while horizontal swipes continue to scroll the strip as usual. For mouse users, see the `vertical_modifier` option under `[swipe.scroll]`.

| Action | Description |
| :--- | :--- |
| `window_virtual_north` / `_south` | Switch to the previous/next virtual workspace (row of windows). |
| `window_virtualnum_<number>` | Switch directly to the numbered virtual workspace. |
| `window_virtualmove_north` / `_south` | Move currently focused window to the previous/next virtual workspace and follow it. |
| `window_virtualsend_north` / `_south` | Move currently focused window to the previous/next virtual workspace but stay on the current one. |
| `window_virtualmovenum_<number>` | Move currently focused window to the numbered virtual workspace and follow it. |
| `window_virtualsendnum_<number>` | Move currently focused window to the numbered virtual workspace but stay on the current one. |


**Example:**
```toml
[bindings]
window_virtual_north = "cmd + shift - k"
window_virtual_south = "cmd + shift - j"
window_virtualmove_north = "cmd + alt - k"
window_virtualmove_south = "cmd + alt - j"
window_virtualnum_1 = "cmd + alt - 1"
window_virtualnum_2 = "cmd + alt - 2"
window_virtualnum_3 = "cmd + alt - 3"
window_virtualmovenum_1 = "cmd + alt + ctrl - 1"
window_virtualmovenum_2 = "cmd + alt + ctrl - 2"
window_virtualmovenum_3 = "cmd + alt + ctrl - 3"
window_virtualsendnum_1 = "cmd + alt + shift - 1"
window_virtualsendnum_2 = "cmd + alt + shift - 2"
window_virtualsendnum_3 = "cmd + alt + shift - 3"
```

**Example command line:**
```shell
# Move to the previous virtual workspace.
$ paneru send-cmd window virtual north
# Move the current window to the next virtual workspace.
$ paneru send-cmd window virtualmove south
# Move directly to virtual workspace 3.
$ paneru send-cmd window virtualnum 3
# Move the current window to virtual workspace 3 and follow it.
$ paneru send-cmd window virtualmovenum 3
# Send the current window to virtual workspace 3 and stay here.
$ paneru send-cmd window virtualsendnum 3
```

See [QUERY_AND_SUBSCRIBE_FORMAT.md](QUERY_AND_SUBSCRIBE_FORMAT.md) for the
structured `paneru query` responses and `paneru subscribe` event stream.

---

## 6. Window Rules (`[windows]`)

Define specific behaviors for applications based on their Title or Bundle ID.

| Option | Type | Description |
| :--- | :--- | :--- |
| `title` | Regex | **(Required)** Regex pattern to match the window title. |
| `bundle_id` | String | Optional Bundle ID to match (e.g., `com.apple.Terminal`). |
| `floating` | Boolean | Force the window to be floating/unmanaged. |
| `manage` | Boolean | Force Paneru to manage this app/window even if macOS reports the app as unobservable or the window has a non-standard role/subrole. |
| `index` | Integer | Preferred position in the strip when spawned. |
| `dont_focus` | Boolean | Prevent the window from taking focus when spawned. |
| `width` | Positive Float | Initial width ratio for the window. Values above `1.0` create an oversized, horizontally scrollable window. |
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

### Forcing management of LSUIElement or non-standard windows

Some applications (e.g., BetterTouchTool, ProtonVPN) are flagged as background apps
(`LSUIElement`) or expose windows with unusual accessibility roles such as `AXTable`
or `AXTextField`. Paneru normally ignores these processes and windows. Use `manage = true`
to opt in and forcibly manage the matching windows.

```toml
[windows.btt_main]
bundle_id = "com.hegenberg.BetterTouchTool"
title = "BetterTouchTool"
manage = true

[windows.btt_screenshot]
bundle_id = "com.hegenberg.BetterTouchTool"
title = "Screenshot.*"
floating = true
```

### Session Restore

Paneru saves its managed window layout and can restore it the next time it
starts. Restore is a startup-only phase: Paneru loads the saved session, applies
it after initial window discovery, keeps matching open for a short grace period,
then stops consulting the saved state until the next Paneru process start.

The saved session includes:

- native workspace ids
- virtual workspace rows and the selected row per native workspace
- layout structure: singles, stacks, tabs, and fullscreen strips
- display/screen association
- window identity for matching across restarts

Matched startup windows use the saved session before static `[windows]` rules.
That means saved layout, virtual workspace, display, and managed/floating state
win over configured `index`, `floating`, `width`, and `grid` rules during
restore. Unmatched startup windows, and all windows created after the restore
grace period ends, keep normal `[windows]` behavior.

| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `enabled` | Boolean | `true` | Enables session restore on startup. |
| `startup_grace_ms` | Integer (ms) | `2000` | How long Paneru keeps restore matching active after startup. This gives apps a chance to create windows shortly after Paneru starts. |
| `missing_windows` | String | `"ignore"` | Behavior when a saved window is not present during restore. Currently only `"ignore"` is supported, which drops the missing window and compacts the restored layout. |

**Example:**
```toml
[restore]
enabled = true
startup_grace_ms = 2000
missing_windows = "ignore"
```

Restore matches windows first by stable startup identity:

- window id
- process id
- bundle id

If an application has restarted and those ids changed, Paneru can use a
conservative fallback match:

- bundle id
- non-empty window title
- window identifier
- accessibility role
- accessibility subrole

Fallback matching is only used when it is unambiguous. If multiple current
windows could match the same saved window, Paneru skips that saved window rather
than moving the wrong one.

If a saved app or window is missing, the default `"ignore"` policy simply drops
it from the restored layout. Empty stacks, tab groups, columns, and virtual rows
are removed. If the previously selected virtual row is removed because all of
its windows are missing, Paneru selects the nearest remaining restored row for
that native workspace. If no restored row remains, the normal startup workspace
selection is kept.

For screens, Paneru prefers the current macOS workspace-to-display mapping when
the workspace is already present on a display. Otherwise it restores to the
saved display id when that screen is still connected, then falls back to the
current active display, then the first available display by id. Paneru does not
create placeholder displays or off-screen state for disconnected monitors.

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

---

## 8. Lua Scripting

Paneru embeds a Lua runtime that runs alongside the TOML config, letting a
script hook into window-manager events (`paneru.on`) and bind keys to Lua
callbacks or command strings (`paneru.bind`).

By default it looks for a script in the following locations (in order):

1. `$PANERU_LUA` (environment variable)
2. `$HOME/.paneru.lua`
3. `$XDG_CONFIG_HOME/paneru/init.lua`

Like the TOML config, the script is automatically reloaded when the file is
saved.

```lua
paneru.on("window_focused", function(event)
  paneru.run("window balance")
end)

paneru.bind("alt - j", "window focus east")
```

### Querying state

Inside a `paneru.on` handler or a `paneru.bind` callback, the script can read
the same state documents `paneru query …` returns — no socket round trip, no
`io.popen`:

```lua
paneru.on("window_focused", function(event)
  for _, window in ipairs(paneru.query_on_screen()) do  -- actually visible
    paneru.log(window.app_name .. ": " .. window.title)
  end

  local active = paneru.query_active()
  paneru.flash("workspace " .. tostring(active.virtual_workspace_number))
end)
```

| Function | Returns |
| --- | --- |
| `paneru.query(kind)` | the raw JSON string, `kind` defaulting to `"state"` |
| `paneru.query_json(kind)` | the same document, decoded into a table |
| `paneru.query_state()` | the complete state document |
| `paneru.query_active()` | the active display, workspace and focused window |
| `paneru.query_workspaces()` | the virtual workspace rows |
| `paneru.query_on_screen()` | the windows currently visible |

These are spelled exactly as in the loadable client module (`require("paneru")`,
see [`crates/lua`](crates/lua)), so a helper that reads state works unchanged in
either host. The payloads are documented in
[`QUERY_AND_SUBSCRIBE_FORMAT.md`](QUERY_AND_SUBSCRIBE_FORMAT.md).

State is gathered on demand and at most once per callback, so handlers that
never query cost nothing extra — which matters for events as frequent as
`mouse_moved`. Outside a callback there is no window-manager state to read, so
calling one of these at script top level raises an error; do it from a handler.

Because the script runs on its own thread (see below), a query is answered by
the window manager on its next scheduled pass rather than instantly: expect the
document to be up to roughly a frame old, and the call itself to take about
that long. This does not apply to handlers that never query.

### Programmatic window management

Handlers are given a **window set**: the whole layout — displays, workspaces,
columns, the windows in them — as a value you can transform. It is modelled on
xmonad's `StackSet`, and it is *pure*: every operation returns a **new** window
set rather than changing the one you were given, and nothing touches a real
window until you **return** one.

```lua
paneru.bind("alt - h",       function(ws) return ws:focus(ws:west(ws:focused())) end)
paneru.bind("alt - shift-h", function(ws) return ws:swap(ws:focused(), ws:west(ws:focused())) end)
paneru.bind("alt - 3",       function(ws) return ws:view(3) end)
paneru.bind("alt - shift-3", function(ws) return ws:shift(ws:focused(), 3) end)
```

That the value is pure is the point, not a technicality:

```lua
paneru.bind("alt - b", function(ws)
  local tidied = ws:width(ws:focused(), 0.6)   -- computed, not applied
  if #ws:columns() < 3 then
    return                                     -- returning nothing changes nothing
  end
  return tidied                                -- only what you return commits
end)
```

A handler that raises partway through changes nothing either, because it never
returned anything. You can branch, compute several candidate layouts, and keep
one.

`paneru.windows(fn)` is the same contract for use partway through a handler:
it hands `fn` the window set and commits what it gives back.

#### Reading

| Method | Returns |
| --- | --- |
| `ws:focused()` | id of the focused window, or `nil` |
| `ws:windows()` | every window, as records |
| `ws:window(id)` | one window record |
| `ws:find(pred)` / `ws:filter(pred)` | the first / all windows matching a predicate |
| `ws:current()` | the number of the workspace on screen |
| `ws:workspaces()` | every workspace number |
| `ws:workspace_windows(n)` | the windows on workspace `n` |
| `ws:columns([n])` | the columns of a workspace, each a list of window ids |
| `ws:column_of(id)` / `ws:workspace_of(id)` / `ws:display_of(id)` | where a window is |
| `ws:east(id)` / `ws:west(id)` | the window one column over |
| `ws:next(id)` / `ws:prev(id)` | the next/previous window, wrapping |

A window record has `id`, `app_name`, `bundle_id`, `title`, `frame`, `floating`,
`managed`, `visible` and `focused`.

`paneru.match{ app = …, bundle = …, title = …, floating = …, managed = … }`
builds a predicate; `app`, `bundle` and `title` are regular expressions, checked
when the script loads. Any function taking a window record works too.

#### Transforming

Each returns a new window set: `ws:focus(id)`, `ws:swap(a, b)`,
`ws:shift(id, workspace[, follow])`, `ws:view(workspace)`, `ws:float(id)`,
`ws:sink(id)`, `ws:manage(id)`, `ws:unmanage(id)`, `ws:width(id, ratio)`,
`ws:stack(id, onto)`, `ws:tab(id, onto)`, `ws:unstack(id)`.

Operations that act on whatever is focused rather than on a window you name —
centring, full width, equalise, balance, raising a float, moving to the next
display — are not here. Use the imperative verbs for those:
`paneru.window.center()`, `paneru.window.balance()`, and so on.

Two things to keep in mind. The set is a snapshot, so a window it lists may be
gone by the time your operations are applied; those are skipped and the rest
still happen, and a handler that cares can check for itself. And the returned
layout is a prediction — the layout engine settles the actual geometry, so read
the settled result from the next event rather than from the set you just built.

#### Example: named scratchpads

A worked port of xmonad's
[`XMonad.Util.NamedScratchpad`](https://xmonad.github.io/xmonad-docs/xmonad-contrib/src/XMonad.Util.NamedScratchpad.html).
A scratchpad is a window you toggle in and out of view; when it is not wanted it
is parked on a workspace you never look at — xmonad's `NSP`.

```lua
scratchpad = { stash = 9, pads = {}, order = {} }

function scratchpad.define(name, spec)
  scratchpad.pads[name] = spec
  table.insert(scratchpad.order, name)
end

-- The pad a window belongs to, if any. Declaration order decides ties.
function scratchpad.pad_of(window)
  for _, name in ipairs(scratchpad.order) do
    if scratchpad.pads[name].match(window) then
      return name, scratchpad.pads[name]
    end
  end
end

-- Park every pad in `names` that is currently on screen.
function scratchpad.hide(ws, names)
  for _, name in ipairs(names) do
    local window = ws:find(scratchpad.pads[name].match)
    if window and ws:workspace_of(window.id) == ws:current() then
      ws = ws:shift(window.id, scratchpad.stash)
    end
  end
  return ws
end

function scratchpad.hide_all(ws)
  return scratchpad.hide(ws, scratchpad.order)
end

-- Everything declared in the same group as `name`, except itself.
function scratchpad.group_of(name)
  local group, mine = {}, scratchpad.pads[name].group
  if not mine then return group end
  for _, other in ipairs(scratchpad.order) do
    if other ~= name and scratchpad.pads[other].group == mine then
      table.insert(group, other)
    end
  end
  return group
end

function scratchpad.toggle(name)
  return function(ws)
    local pad = scratchpad.pads[name]
    local window = ws:find(pad.match)
    if not window then
      os.execute(pad.spawn .. " &")                 -- not running: start it
      return
    end
    if ws:workspace_of(window.id) == ws:current() then
      return ws:shift(window.id, scratchpad.stash)  -- in view: put it away
    end
    ws = scratchpad.hide(ws, scratchpad.group_of(name))
    return ws:shift(window.id, ws:current(), true):focus(window.id)
  end
end
```

The three toggle branches are xmonad's, in the same order: spawn if nothing
matches, stash if it is here, otherwise summon and focus. `group` is
`addExclusives`: summoning one pad puts its group-mates away first.

Two event handlers finish it off — the equivalent of `namedScratchpadManageHook`
and `nsHideOnFocusLoss`:

```lua
-- Place a pad window the first time we see it (xmonad's per-pad `hook`).
scratchpad.seen = {}
paneru.on("window_focused", function(event, ws)
  if scratchpad.seen[event.window_id] then return end
  scratchpad.seen[event.window_id] = true
  local window = ws:window(event.window_id)
  if not window then return end
  local _, pad = scratchpad.pad_of(window)
  if pad and pad.float and window.managed then
    return ws:float(window.id)
  end
end)

-- Hide a pad when the focus leaves it.
scratchpad.focused = nil
paneru.on("window_focused", function(event, ws)
  local previous = scratchpad.focused
  scratchpad.focused = event.window_id
  if not previous or previous == event.window_id then return end
  local window = ws:window(previous)
  if window and scratchpad.pad_of(window) then
    return ws:shift(previous, scratchpad.stash)
  end
end)
```

Both are registered for the same event and each commits its own returned set
independently, so they compose without knowing about each other. Neither touches
the window set on the paths where it bails early, so the common case — focusing
an ordinary window — costs nothing.

Finally, declare the pads and bind them:

```lua
scratchpad.define("terminal", {
  match = paneru.match{ app = "Alacritty", title = "^scratch" },
  spawn = "open -na Alacritty --args --title scratch",
  float = true, group = "console",
})
scratchpad.define("notes", {
  match = paneru.match{ app = "Obsidian" },
  spawn = "open -a Obsidian",
  group = "console",
})

paneru.bind("alt - s", scratchpad.toggle("terminal"))
paneru.bind("alt - n", scratchpad.toggle("notes"))
paneru.bind("alt - 0", scratchpad.hide_all)
```

`os.execute` blocks, which is normally the last thing you want in a window
manager — but the script has its own thread, so a slow launch delays only the
script.

**Where this differs from xmonad.** Three things do not carry over:

* `customFloating` places a float at a proportional rectangle. There is no
  set-frame operation on the window set, so `ws:float(id)` floats a window
  where it is. Use a `[[windows]]` rule with `grid` for the geometry.
* xmonad applies the manage hook when a window *appears*. Paneru has no
  window-created event for scripts — the window has no id yet at that point —
  so the hook above keys off first focus instead, which is when a new window
  normally lands.
* The stash workspace is an ordinary virtual workspace, so it shows up in the
  workspace indicators, and `reap_empty_workspaces` will remove it when the last
  pad leaves. Pick a high number and leave that option off if it bothers you.

### The script runs on its own thread

Handlers are your code, and the window manager cannot know how long yours will
take. So it does not wait for them: the script runs on a dedicated thread, and
events, keybind callbacks and hot reloads are handed to it over a queue. A
handler that blocks for a second no longer freezes window dragging, focus
tracking or the menubar for that second — it only delays the script's own
subsequent handlers, which queue up behind it.

Three things follow:

* Commands a handler issues with `paneru.run` take effect a frame later than
  they would have if the handler ran inline. Nothing you can observe from Lua
  depends on this, but it is why a handler cannot see its own commands reflected
  in a query it makes immediately afterwards.
* A handler that never returns will queue events behind it indefinitely. There
  is no watchdog and no timeout: the window manager stays fully responsive, but
  your script stops reacting until it is reloaded. Save the file to reload it.
* C modules loaded with `require` (sketchybar's `sbar`, say) run on that thread
  too, not the main one. `SbarLua` writes to a socket and is fine with this; a
  module that expects to be called from the process's main thread would not be.

### Extra Lua modules

`$PANERU_LUA_PATH` and `$PANERU_LUA_CPATH` extend the script's `require()`
search path (`package.path`/`package.cpath`), so `init.lua` can load modules
beyond Lua's stock library — for example, calling into
[sbarlua](https://github.com/FelixKratz/SbarLua) directly from a
`paneru.on` handler, instead of shelling out to `sketchybar --trigger`:

```lua
local sbar = require("sbar")

paneru.on("window_focused", function(event)
  sbar.set("front_app", { label = { string = tostring(event.window_id) } })
end)
```

Both are set automatically when using the nix-darwin or Home Manager module's
`services.paneru.extraLuaPackages` option — see
[`nix/README.md`](nix/README.md).
