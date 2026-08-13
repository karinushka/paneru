# Lua Scripting Guide

Paneru embeds a Lua runtime, letting a script declare the entire configuration via `paneru.setup{...}`, hook into window-manager events (`paneru.on`), bind keys to Lua callbacks or command strings (`paneru.bind`), query state, persist data across reloads, and programmatically manipulate window sets.

---

## 1. Getting Started

### Script Locations

By default, Paneru looks for a Lua script in the following locations (in order):

1. `$PANERU_LUA` (environment variable)
2. `$HOME/.paneru.lua`
3. `$XDG_CONFIG_HOME/paneru/init.lua`

### TOML Replacement

**A script replaces the TOML.** When any of those files exist, the TOML path is switched off completely: no `paneru.toml` is read, created, or watched, and anything the script does not set takes its built-in default. Two authoritative configs cannot coexist, so a leftover TOML never quietly overrides the script. Conversely, no `init.lua` is created for you when a `paneru.toml` already exists — TOML setups keep working untouched until you write a script yourself.

Like the TOML config, the script is automatically reloaded when the file is saved.

```lua
paneru.on("window_focused", function(event, ws)
  paneru.run("window balance")
end)

paneru.bind("alt - j", "window focus east")
```

---

## 2. Configuration from Lua (`paneru.setup`)

`paneru.setup{...}` declares the whole configuration from Lua, so `init.lua` can replace `paneru.toml` entirely. The table mirrors the TOML sections one-for-one — `options`, `padding`, `swipe`, `decorations`, `restore`, `windows`, and top-level `default_workspaces`.

```lua
paneru.setup {
  default_workspaces = 3,
  options = {
    focus_follows_mouse = true,
    sliver_width = 5,
    animation_speed = 12.0,   -- write floats with a decimal point
  },
  padding = { top = 10, bottom = 10, left = 8, right = 8 },
  swipe = { sensitivity = 0.4, scroll = { modifier = "alt" } },
  decorations = {
    active = { border = { enabled = true, color = "#89b4fa", width = 2.0 } },
  },
  restore = { enabled = true, startup_grace_ms = 2000 },
  windows = {
    -- keys are just rule names; `title` is a required regex
    term = { title = "kitty", floating = true, bindings_passthrough = { "ctrl+alt-h" } },
  },
}
```

### Keybindings

Two equivalent ways to bind keys, both accepting the exact chord syntax of the TOML `[bindings]` table:

- `paneru.bind(chord, handler)` — the handler is a command string **or a Lua function** (function handlers receive a state snapshot; only `paneru.bind` supports them).
- a `bindings` sub-table inside `setup`, keyed by command with the chord as the value — a shorthand that desugars onto the same path as `paneru.bind`:

```lua
paneru.setup {
  bindings = {
    ["window focus east"] = "alt - l",
    ["quit"] = "ctrl + alt - q",
  },
}
```

### Precedence & Reloading

An `init.lua` disables the TOML entirely, whether or not it calls `paneru.setup`. With `setup`, that table is the configuration; without it, the built-in defaults are used — never a `paneru.toml` sitting next to the script. To keep using TOML, do not create a script.

Editing and saving `init.lua` hot-reloads the whole configuration (including menubar and passthrough updates), just like editing the TOML file.

**Notes:**
- Float-valued options (`animation_speed`, border `width`/`opacity`, window `width`, …) should be written with a decimal point (`12.0`, not `12`).
- A reload that *removes* a previous `paneru.setup` call keeps the last config it produced rather than reverting to TOML.
- With Nix modules, set `services.paneru.config` to this `init.lua` (Lua source or a path). See [`nix/README.md`](nix/README.md).

---

## 3. Event Handling (`paneru.on`)

`paneru.on` registers callback functions that execute when window-manager events occur.

### Registration Syntax

`paneru.on` accepts 2 or 3 arguments:

```lua
paneru.on(event_name, [filter,] handler)
```

- `event_name` (string): The event to listen for.
- `filter` (optional table or function): Filter criteria. Only events matching the filter will trigger the handler. Non-matching events avoid cross-thread state queries and Lua execution.
- `handler` (function): The callback function, receiving `(event, workspace)`.

```lua
-- Unfiltered handler:
paneru.on("window_focused", function(event, ws)
  paneru.log("Focused window: " .. tostring(event.window_id))
end)

-- Filtered handler with table spec:
paneru.on("window_spawned", { bundle = "libreoffice" }, function(event, ws)
  if event.frame.width < 400 or event.frame.height < 400 then
    return ws:float(event.window_id)
  end
end)

-- Filtered handler with paneru.match:
paneru.on("window_spawned", paneru.match{ app = "Ghostty" }, function(event, ws)
  paneru.log("Spawned Ghostty window")
end)
```

### Supported Events

| Event Name | Description | Event Payload Fields |
| --- | --- | --- |
| `window_spawned` | A window was fully spawned and initialized in Paneru | `type`, `window_id`, `pid`, `app_name`, `bundle_id`, `title`, `frame` (`{x, y, width, height}`), `floating`, `managed` |
| `window_focused` | A window gained focus | `type`, `window_id` |
| `window_destroyed` | A window was closed / destroyed | `type`, `window_id` |
| `window_moved` | A window was moved | `type`, `window_id` |
| `window_resized` | A window was resized | `type`, `window_id` |
| `window_minimized` | A window was minimized | `type`, `window_id` |
| `window_deminimized` | A window was un-minimized | `type`, `window_id` |
| `window_title_changed` | A window's title changed | `type`, `window_id` |
| `application_activated` | An application was activated | `type`, `pid` |
| `application_deactivated` | An application was deactivated | `type`, `pid` |
| `application_visible` | An application became visible | `type`, `pid` |
| `application_hidden` | An application became hidden | `type`, `pid` |
| `mouse_down` / `mouse_up` / `mouse_dragged` / `mouse_moved` | Mouse actions | `type`, `x`, `y`, `modifiers` |
| `space_changed` | Active workspace changed | `type` |
| `space_created` / `space_destroyed` | Virtual workspace added or removed | `type`, `space_id` |

---

## 4. Querying State

Inside a `paneru.on` handler or a `paneru.bind` callback, the script can read the same state documents `paneru query …` returns — no round trip, no `io.popen`:

```lua
paneru.on("window_focused", function(event, ws)
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

These are spelled exactly as in the loadable client module (`require("paneru")`, see [`crates/lua`](crates/lua)), so a helper that reads state works unchanged in either host. The payloads are documented in [`QUERY_AND_SUBSCRIBE_FORMAT.md`](QUERY_AND_SUBSCRIBE_FORMAT.md).

State is gathered on demand and at most once per callback, so handlers that never query cost nothing extra. Outside a callback there is no window-manager state to read, so calling one of these at script top level raises an error; call them inside a handler or keybinding callback.

---

## 5. Persistent State (`paneru.state`)

A handler that wants to remember something — which window is the scratchpad, what was focused a moment ago, how many times something has happened — cannot keep it in a Lua global. Saving `init.lua` rebuilds the interpreter, and every global goes with it. `paneru.state` is the store that survives reloads and daemon restarts.

```lua
paneru.state.set("pads.term", 4213)     -- any JSON-shaped value
paneru.state.get("pads.term")           -- 4213, or nil
paneru.state.set("pads.term", nil)      -- nil removes the key

paneru.state.mutate("count", function(n) return (n or 0) + 1 end)
```

| Function | Description |
| --- | --- |
| `paneru.state.get(key)` | Returns the stored value, or `nil` |
| `paneru.state.set(key, value)` | Stores a value; passing `nil` removes the key |
| `paneru.state.mutate(key, fn)` | Passes the current value to `fn` and atomically stores what it returns |

Reach for `mutate` whenever the new value depends on the old one. It reads, runs your function, and stores the result only if the value is still what it read; if something else modified it first, `mutate` re-runs your function against the new value.

Keys are plain strings; values can be strings, numbers, booleans, or JSON-shaped tables. The store is saved in `$XDG_STATE_HOME/paneru/script-state.json`.

---

## 6. Programmatic Window Management

Handlers are given a **window set** (`ws`): the whole layout — displays, workspaces, columns, and the windows in them — as a value you can transform. It is modeled on xmonad's `StackSet`, and it is *pure*: every operation returns a **new** window set rather than changing the one you were given, and nothing touches a real window until you **return** it.

```lua
paneru.bind("alt - h",       function(ws) return ws:focus(ws:west(ws:focused())) end)
paneru.bind("alt - shift-h", function(ws) return ws:swap(ws:focused(), ws:west(ws:focused())) end)
paneru.bind("alt - 3",       function(ws) return ws:view(3) end)
paneru.bind("alt - shift-3", function(ws) return ws:shift(ws:focused(), 3) end)
```

Because the window set is pure:

```lua
paneru.bind("alt - b", function(ws)
  local tidied = ws:width(ws:focused(), 0.6)   -- computed, not applied
  if #ws:columns() < 3 then
    return                                     -- returning nothing changes nothing
  end
  return tidied                                -- only what you return commits
end)
```

A handler that raises partway through changes nothing either, because it never returned anything. You can branch, compute candidate layouts, and return the chosen one.

`paneru.windows(fn)` is the same contract for use partway through a handler: it hands `fn` the window set and commits what it gives back.

### Reading Layout State

| Method | Returns |
| --- | --- |
| `ws:focused()` | ID of the focused window, or `nil` |
| `ws:windows()` | Every window, as records |
| `ws:window(id)` | One window record |
| `ws:find(pred)` / `ws:filter(pred)` | The first / all windows matching a predicate |
| `ws:current()` | The number of the workspace on screen |
| `ws:workspaces()` | Every workspace number |
| `ws:workspace_windows(n)` | The windows on workspace `n` |
| `ws:columns([n])` | The columns of a workspace, each a list of window IDs |
| `ws:column_of(id)` / `ws:workspace_of(id)` | The column index / workspace number a window is on |
| `ws:display_of(id)` | The display a window is on: `{ id, active, x, y, width, height }` |
| `ws:east(id)` / `ws:west(id)` | The window one column over |
| `ws:next(id)` / `ws:prev(id)` | The next/previous window, wrapping |

A window record contains `id`, `app_name`, `bundle_id`, `title`, `frame`, `floating`, `managed`, `visible` and `focused`.

`paneru.match{ app = …, bundle = …, title = …, floating = …, managed = … }` builds a compiled predicate; `app`, `bundle` and `title` are regular expressions.

### Transforming Layout State

Each method returns a new window set:
- `ws:focus(id)`
- `ws:swap(a, b)`
- `ws:shift(id, workspace[, follow])`
- `ws:view(workspace)`
- `ws:float(id[, rect])`
- `ws:sink(id)`
- `ws:manage(id)`
- `ws:unmanage(id)`
- `ws:width(id, ratio)`
- `ws:stack(id, onto)`
- `ws:tab(id, onto)`
- `ws:unstack(id)`

`ws:float(id)` takes a window out of the tiling layout and leaves it where it is. `ws:float(id, rect)` places it relative to display fractions:

```lua
ws:float(id, { x = 0.1, y = 0.05, width = 0.8, height = 0.5 })
```

---

## 7. Example: Named Scratchpads

A worked port of xmonad's `NamedScratchpad`. A scratchpad is a window you toggle in and out of view; when not wanted, it is parked on a workspace you never look at (`stash = 9`).

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
    ws = scratchpad.hide(ws, scratchpad.order)
    return ws:shift(window.id, ws:current(), true):focus(window.id)
  end
end

-- Place a pad window the first time we see it
paneru.on("window_spawned", { bundle = "org.libreoffice.script" }, function(event, ws)
  if event.frame.width < 400 or event.frame.height < 400 then
    return ws:float(event.window_id)
  end
end)
```
