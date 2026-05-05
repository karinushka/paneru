# Query and Subscribe Format

Paneru exposes structured state over the same Unix socket used by `send-cmd`.
The CLI commands below require a running Paneru daemon.

All query responses are JSON followed by a newline. `subscribe` emits
line-delimited JSON, with one complete event object per line.

## Query Commands

```shell
paneru query state --json
paneru query virtual-workspaces --json
paneru query active --json
```

`--json` is accepted for clarity. The socket protocol also accepts the query
without it, but callers should include `--json`.

### `paneru query state --json`

Returns the complete state document.

```json
{
  "version": 1,
  "timestamp": 1777740000,
  "active": {
    "display_id": 1,
    "native_workspace_id": 4,
    "virtual_workspace_number": 3,
    "focused_window_id": 321,
    "focused_bundle_id": "com.apple.Terminal",
    "focused_app_name": "Terminal",
    "focused_window_title": "paneru"
  },
  "virtual_workspaces": [
    {
      "number": 1,
      "native_workspace_id": 4,
      "active": false,
      "windows": []
    },
    {
      "number": 2,
      "native_workspace_id": 4,
      "active": false,
      "windows": []
    },
    {
      "number": 3,
      "native_workspace_id": 4,
      "active": true,
      "windows": [
        {
          "window_id": 321,
          "bundle_id": "com.apple.Terminal",
          "app_name": "Terminal",
          "title": "paneru",
          "focused": true,
          "floating": false
        }
      ]
    }
  ]
}
```

### `paneru query virtual-workspaces --json`

Returns only the `virtual_workspaces` array from the complete state document.

```json
[
  {
    "number": 1,
    "native_workspace_id": 4,
    "active": false,
    "windows": []
  },
  {
    "number": 2,
    "native_workspace_id": 4,
    "active": false,
    "windows": []
  },
  {
    "number": 3,
    "native_workspace_id": 4,
    "active": true,
    "windows": [
      {
        "window_id": 321,
        "bundle_id": "com.apple.Terminal",
        "app_name": "Terminal",
        "title": "paneru",
        "focused": true,
        "floating": false
      }
    ]
  }
]
```

### `paneru query active --json`

Returns only the active display, workspace, and focused-window state.

```json
{
  "display_id": 1,
  "native_workspace_id": 4,
  "virtual_workspace_number": 3,
  "focused_window_id": 321,
  "focused_bundle_id": "com.apple.Terminal",
  "focused_app_name": "Terminal",
  "focused_window_title": "paneru"
}
```

## Fields

| Field | Type | Description |
| :--- | :--- | :--- |
| `version` | number | State document format version. Currently `1`. |
| `timestamp` | number | Unix timestamp in seconds when the response was built. |
| `active` | object | Current active display/native workspace/virtual workspace/focused window. |
| `display_id` | number or null | CoreGraphics display id for the active display, when known. |
| `native_workspace_id` | number or null | macOS Space id for the active native workspace, when known. |
| `virtual_workspace_number` | number or null | One-based Paneru virtual workspace number, when known. |
| `focused_window_id` | number or null | Focused window id, when known. |
| `focused_bundle_id` | string or null | Bundle id of the focused window's app, when known. |
| `focused_app_name` | string or null | Display name of the focused window's app, when known. |
| `focused_window_title` | string or null | Title of the focused window, when known. |
| `virtual_workspaces` | array | Virtual workspace rows known to Paneru. |
| `number` | number | One-based virtual workspace number. |
| `active` | boolean | Whether this virtual workspace is currently selected. |
| `windows` | array | Managed windows in this virtual workspace row. |
| `window_id` | number | Window id. |
| `bundle_id` | string | Bundle id for the owning application, or an empty string if unknown. |
| `app_name` | string | Display name for the owning application, or an empty string if unknown. |
| `title` | string | Window title, or an empty string if unknown. |
| `focused` | boolean | Whether this window is focused. |
| `floating` | boolean | Whether this window is unmanaged/floating. |

Paneru may include empty `windows` arrays for missing virtual workspace numbers
inside a native workspace so integrations can render stable numbered slots.

## Subscribe Command

```shell
paneru subscribe --json
```

`subscribe` keeps the socket open and writes one JSON event per line. The stream
is intended for integrations such as SketchyBar, so it emits changes that are
useful for keeping a bar in sync: focus changes, native or virtual workspace
changes, managed window-list changes, window title changes, and display changes.
Paneru coalesces duplicate internal events from the same ECS tick and skips
events whose relevant state has not changed since the last emitted event.
Consumers should parse each line independently and then call
`paneru query state --json` when they need a full refresh.

### Event Types

```json
{"event":"virtual_workspace_changed","active":{"display_id":1,"native_workspace_id":4,"virtual_workspace_number":3,"focused_window_id":321,"focused_bundle_id":"com.apple.Terminal","focused_app_name":"Terminal","focused_window_title":"paneru"}}
```

Emitted after native Space changes and Paneru virtual workspace switches. Paneru
derives this from both incoming workspace events and ECS active-workspace marker
changes, so integrations receive the event when the visible workspace state
changes.

```json
{"event":"windows_changed","virtual_workspace_number":3,"active":{"display_id":1,"native_workspace_id":4,"virtual_workspace_number":3,"focused_window_id":321,"focused_bundle_id":"com.apple.Terminal","focused_app_name":"Terminal","focused_window_title":"paneru"}}
```

Emitted after managed window creation/destruction/minimize/deminimize events and
after Paneru moves or sends a window between virtual workspaces. The event is
emitted only when Paneru's virtual workspace/window state differs from the last
emitted `windows_changed` event.

```json
{"event":"window_focused","window_id":321,"bundle_id":"com.apple.Terminal","title":"paneru","virtual_workspace_number":3}
```

Emitted when focus changes. Paneru derives this from both incoming focus events
and ECS focused-window marker changes, so internally handled focus transitions
are visible to subscribers. The `window_id`, `bundle_id`, `title`, and
`virtual_workspace_number` fields are taken from the final active state for the
tick, so stale lower-level focus notifications are not forwarded with mismatched
window metadata.

```json
{"event":"window_title_changed","window_id":321,"title":"paneru"}
```

Emitted when a window title changes.

```json
{"event":"display_changed","display_id":1}
```

Emitted when display configuration changes. `display_id` can be `null` when the
event is a global display-change notification and Paneru cannot resolve an
active display id.

## Virtual Workspace Commands

Absolute virtual workspace selection is addressed as a window command:

```shell
paneru send-cmd window virtualnum 3
paneru send-cmd window virtualmovenum 3
paneru send-cmd window virtualsendnum 3
```

The matching config binding names are:

```toml
[bindings]
window_virtualnum_3 = "cmd + alt - 3"
window_virtualmovenum_3 = "cmd + alt + ctrl - 3"
window_virtualsendnum_3 = "cmd + alt + shift - 3"
```
