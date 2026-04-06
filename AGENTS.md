# Agent Instructions: Paneru macOS Window Manager (Bevy-based)

This document provides project-specific guidance for AI agents contributing to Paneru. It builds upon the core philosophy and technical architecture of the codebase.

## 1. Bevy ECS Architecture & Patterns

Paneru is built on Bevy and follows a strict Data-Driven Design (ECS).

*   **Marker Components:** Use markers extensively for filtering and state tracking (e.g., `ActiveWorkspaceMarker`, `FocusedMarker`, `FreshMarker`, `Unmanaged`). Most markers are found in `src/ecs.rs` or `src/ecs/mod.rs`.
*   **Triggers & Observers:** Prefer Bevy's observer pattern for reactive logic. See `src/ecs/triggers.rs` and `src/ecs/workspace.rs` for examples like `SpawnWindowTrigger` and `WMEventTrigger`.
*   **System Grouping:** Systems are registered in `src/ecs.rs` via `register_systems`. Follow the existing schedule-based organization (`PreUpdate`, `Update`, `PostUpdate`).
*   **System Params:** Use custom system parameters like `Windows` and `ActiveDisplay` (defined in `src/ecs/params.rs`) to simplify queries.

## 2. macOS & AppKit Integration (The Bridge)

*   **Main Thread Constraint:** All AppKit/CoreGraphics calls MUST happen on the main thread.
*   **NonSend Resources:** Use `NonSend` and `NonSendMut` for resources that are not thread-safe (e.g., `WindowManager`, `OverlayManager`).
*   **FFI Wrappers:** Interact with macOS via the abstractions in `src/manager/` and `src/platform/`. Avoid direct `objc2` or `icrate` calls in ECS systems; use the `WindowManager` API.
*   **Change Detection:** Use `Changed<T>` to trigger expensive macOS API updates (like window repositioning) only when the ECS state actually changes.

## 3. Layout & Workspace Logic

*   **LayoutStrip:** The core layout data structure is `LayoutStrip` (in `src/ecs/layout.rs`). It manages columns, stacks, and tabs.
*   **Virtual Workspaces:** Paneru manages virtual workspaces that map to macOS "Spaces". See `src/ecs/workspace.rs` for how window movement and workspace switching are handled.
*   **Coordinate Systems:** Be aware of the difference between Bevy's coordinate system (often Y-up) and macOS/AppKit (Y-down). Use the `Position` and `Size` abstractions to handle conversions.

## 4. Coding Standards & Idioms

*   **Clippy:** Paneru enforces strict Clippy lints. Run `cargo clippy` before finalizing changes.
*   **Formatting:** All code must be formatted using `cargo fmt`.
*   **Tracing:** Use the `tracing` crate for logging. Use `#[instrument(level = Level::DEBUG, skip_all, fields(...))]` for complex systems.
*   **Error Handling:** Use the project's `Result` type and `Error` enum in `src/errors.rs`. Avoid `unwrap()` in systems; log errors or use `inspect_err`.

## 5. Testing Strategy

*   **Mocking:** When adding features that interact with macOS, ensure the logic is separable so it can be tested with a mock `WindowManager`.
*   **System Tests:** Add tests to `src/tests.rs` or new files in `src/tests/` that drive a mock Bevy `World`.
*   **Pure Functions:** Extract complex layout math into pure functions (e.g., in `src/ecs/layout.rs`) and add unit tests.

## 6. Contribution Workflow

*   **Research:** Before implementing, check `src/ecs/systems.rs` to see if a similar system already exists.
*   **Implementation:** Follow the **Plan -> Act -> Validate** cycle.
*   **Verification:** Run `cargo fmt`, `cargo check`, and relevant tests. If the change affects layout, verify it doesn't break existing tiling behavior.
