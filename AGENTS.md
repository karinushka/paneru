# Agent Instructions: Paneru macOS Window Manager (Bevy-based)

This document provides project-specific guidance for AI agents contributing to Paneru. It builds upon the core philosophy and technical architecture of the codebase.

## 1. Bevy ECS Architecture & Patterns (Bevy First)

Paneru is built on Bevy and strictly follows Data-Driven Design (ECS). **Always prioritize Bevy ECS rules over conventional Rust patterns**:
*   Avoid traditional Object-Oriented patterns, abstract trait hierarchies, or "god structs" holding internal state machines.
*   Model state as small, queryable Components and compose functionality through Systems and Observers.
*   **Marker Components:** Use markers extensively for filtering and state tracking (e.g., `ActiveWorkspaceMarker`, `FocusedMarker`, `FreshMarker`, `Unmanaged`). Most markers are found in `src/ecs.rs` or `src/ecs/mod.rs`.
*   **Avoid Unbounded / Constantly Running Systems:** Systems should not run unconditionally on every tick if there is nothing to process.
    *   **Use `Populated<Query<...>>` instead of `Query<...>`** where a system only acts when matching entities exist. Bevy automatically adds a run condition for `Populated`, ensuring the system is not even scheduled if the query is empty.
    *   Pair queries with change filters (`Changed<T>`, `Added<T>`) and explicit run conditions (`run_if(...)`) so systems remain idle when state is static.
    *   When tracking per-entity history in `Local<EntityHashMap<...>>`, gate the system on `Populated<..., Changed<T>>` and update the map in-place rather than rebuilding it on every tick.
*   **Bounded Retry & Intent Components:** Any component introduced to poll, retry, or await asynchronous OS confirmation (e.g., `PendingWindowRecovery`, `ResizeAnchor`, `AwaitResize`) MUST have a bounded lifetime/attempt budget and clean up its component or despawn dead/headless entities when expired or superseded by conflicting user input.
*   **Triggers & Observers:** Prefer Bevy's observer pattern for reactive logic. See `src/ecs/triggers.rs` and `src/ecs/workspace.rs` for examples like `SpawnWindowTrigger` and `WMEventTrigger`.
*   **System Grouping:** Systems are registered in `src/ecs.rs` via `register_systems`. Follow the existing schedule-based organization (`PreUpdate`, `Update`, `PostUpdate`).
*   **System Params:** Use custom system parameters like `Windows` and `ActiveDisplay` (defined in `src/ecs/params.rs`) to simplify queries.

## 2. macOS & AppKit Integration (The Bridge)

*   **Main Thread Constraint:** All AppKit/CoreGraphics calls MUST happen on the main thread.
*   **NonSend Resources:** Use `NonSend` and `NonSendMut` for resources that are not thread-safe (e.g., `WindowManager`, `OverlayManager`).
*   **FFI Wrappers:** Interact with macOS via the abstractions in `src/manager/` and `src/platform/`. Avoid direct `objc2` or `icrate` calls in ECS systems; use the `WindowManager` API.
*   **AX IPC vs. WindowServer Queries:** Accessibility calls (`AXUIElementCopyAttributeValue`, `app.window_list()`, `app.focused_window()`, `app.ax_window_ids()`) are synchronous IPC round-trips to the target application process on the main thread, which also hosts the `CGEventTap` input tap. A single unresponsive app can stall input and cause macOS to disable the tap. Never fan out AX queries across all `Application` entities (`for app in &apps`) in event handlers or frequent timers; narrow the target first using WindowServer/SkyLight metadata (e.g., `window_manager.window_owner(window_id)`, `windows_on_screen()`) or ECS state.
*   **Change Detection:** Use `Changed<T>` to trigger expensive macOS API updates (like window repositioning) only when the ECS state actually changes.
*   **The Lua Worker:** The scripting runtime (`src/lua/worker.rs`, `lua` feature) runs on its own thread — handlers are user code of unbounded duration and must never stall `pump_events`. Anything crossing that boundary must be plain `Send` data, never a Lua value or an ECS borrow; world access from a script goes through the `serve_lua_queries` round-trip. If you add a main-thread-only FFI call to a path a script can reach (`resolve_chord` is the existing example), compute it on the main thread and cache it — see `config::prime_virtual_keymap`.

## 3. Layout & Workspace Logic

*   **LayoutStrip:** The core layout data structure is `LayoutStrip` (in `src/ecs/layout.rs`). It manages columns, stacks, and tabs.
    *   **Validate Preconditions Before Mutating:** In `LayoutStrip` methods and `apply_layout_ops`, check all preconditions (such as `Column::Fullscren(_)` guards, same-column checks, and index bounds) via `&LayoutStrip` *before* removing or moving columns/entities (`self.columns.remove(...)`, `strip.remove(...)`). Early returns after partial removal drop windows from the layout, and dereferencing `&mut LayoutStrip` unnecessarily marks `Changed<LayoutStrip>`.
*   **Multi-Display Geometry:** Do not assume a window being resized, spawned, or targeted by a `LayoutOp` lives on `ActiveDisplay`. When computing display-relative geometry (such as `WidthRatio` or `LayoutOp::SetWidth`), resolve the `Display` parent (`ChildOf`) of the `LayoutStrip` containing the window, falling back to `ActiveDisplay` only when the window is in no strip.
*   **Virtual Workspaces:** Paneru manages virtual workspaces that map to macOS "Spaces". See `src/ecs/workspace.rs` for how window movement and workspace switching are handled.
*   **Coordinate Systems:** Be aware of the difference between Bevy's coordinate system (often Y-up) and macOS/AppKit (Y-down). Use the `Position` and `Size` abstractions to handle conversions.

## 4. Coding Standards & Idioms

*   **Clippy:** Paneru enforces strict Clippy lints. Run `cargo clippy` before finalizing changes.
*   **Formatting:** All code must be formatted using `cargo fmt`.
*   **Tracing:** Use the `tracing` crate for logging. Use `#[instrument(level = Level::DEBUG, skip_all, fields(...))]` for complex systems.
*   **Error Handling:** Use the project's `Result` type and `Error` enum in `src/errors.rs`. Avoid `unwrap()` in systems; log errors or use `inspect_err`.

## 5. Testing Strategy

*   **Mocking:** When adding features that interact with macOS, ensure the logic is separable so it can be tested with a mock `WindowManager`.
*   **Harness-Based Integration Tests:** Unit and integration tests drive an isolated Bevy `World` via `TestHarness`. Tests should adhere to the established harness structure:
    1. Define a list of input events/commands (`vec![Event::..., Event::Command { ... }]`).
    2. Configure the harness (e.g., `TestHarness::new().with_windows(n)` or `.with_config(...)`).
    3. Assert expected world/state outcomes on specific, **0-indexed** iterations (`0..commands.len()`) using `.on_iteration(step, |world, state| { ... })`. Iteration `0` is often a warm-up/settling event (e.g., `Event::MenuOpened` or `Command::PrintState`). `TestHarness::run` panics if any verifier's `step >= commands.len()`.
    4. Execute via `.run(commands)`.

    **Example:**
    ```rust
    #[test]
    fn test_stack_focus_or_switch_virtual() {
        let commands = vec![
            Event::MenuOpened { window_id: 0 }, // step 0 (warm-up)
            Event::Command {
                command: Command::Window(Operation::FocusOrVirtual(Direction::South)),
            }, // step 1
            Event::Command {
                command: Command::Window(Operation::FocusOrVirtual(Direction::South)),
            }, // step 2
        ];

        TestHarness::new()
            .with_windows(2)
            .on_iteration(1, |world, _state| {
                // After commands[1]: focus moves to sibling window in stack
                assert_focused!(world, 1);
                assert_eq!(active_virtual_index(world), 0);
            })
            .on_iteration(2, |world, _state| {
                // After commands[2]: at bottom of stack, falls through to workspace switch
                assert_eq!(active_virtual_index(world), 1);
            })
            .run(commands);
    }
    ```
*   **Pure Functions:** Extract complex layout math into pure functions (e.g., in `src/ecs/layout.rs`) and add unit tests.

## 6. Code Cleanup & Verification

Before concluding a task, creating a commit, or presenting work as complete, agents MUST run the standard cleanup and verification suite:

1. **Format Code:**
   ```sh
   cargo fmt
   ```
   Ensures all code across the workspace complies with standard formatting rules and contains proper trailing newlines. Verify with `cargo fmt --check`.

2. **Lint with Clippy:**
   ```sh
   cargo clippy --all-targets -- -D warnings
   ```
   Paneru enforces strict lints. Fix all warnings; do not leave warnings unaddressed.

3. **Run All Tests:**
   ```sh
   cargo test --all-targets
   ```
   Ensure the complete test suite passes without regressions across all packages in the workspace. For targeted debugging of a specific test:
   ```sh
   RUST_LOG=debug cargo test <test_name> -- --nocapture
   ```

## 7. Contribution Workflow

*   **Testing branch:** Use the `testing` branch as a base for the PR, unless the change is very small or is an urgent fix for an issue in the `main` branch. This way the changes get additional baking before unleashin them into the `main` population.
*   **Research:** Before implementing, check `src/ecs/...` to see if a similar system already exists.
*   **Implementation:** Follow the **Plan -> Act -> Validate** cycle.
*   **Verification:** Execute the cleanup and verification steps in Section 6. If changes affect window tiling or animation, verify that existing layout interactions continue to work as expected.

