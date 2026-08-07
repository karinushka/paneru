//! A window layout as a pure value, in the style of xmonad's `StackSet`: every
//! transform returns a new [`WindowSet`] rather than mutating the one it was
//! given, and records what it did as a [`LayoutOp`] so the host can replay the
//! change against the live layout.
//!
//! Each level is behind an [`Arc`], so cloning is cheap and a transform copies
//! only the spine it touches. `Arc` rather than `Rc` because the value is built
//! on the window manager's thread and handed to the interpreter on another.
//!
//! Only transforms that follow from the layout alone are here (focus,
//! ordering, workspace membership, stacking, floating, width ratios); ops the
//! layout engine decides (centring, equalise, raising a float, ...) stay as
//! imperative `paneru.window.*` verbs instead. The returned tree is a
//! prediction — the layout engine settles the actual geometry.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::state::Frame;

/// A window's id, as the accessibility layer reports it.
pub type WinID = i32;

/// What a transform meant, as opposed to what it did to the tree. Replayed
/// against the live world when a handler returns the value carrying it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutOp {
    /// Give `window` the focus.
    Focus(WinID),
    /// Exchange the two windows' positions in the layout.
    Swap(WinID, WinID),
    /// Send `window` to a virtual workspace, optionally following it there.
    MoveToWorkspace {
        window: WinID,
        workspace: u32,
        follow: bool,
    },
    /// Show a virtual workspace on its display.
    View { workspace: u32 },
    /// Take `window` out of the tiling layout, or put it back in.
    SetFloating { window: WinID, floating: bool },
    /// Let the window manager lay `window` out, or stop doing so.
    SetManaged { window: WinID, managed: bool },
    /// Set the column width `window` occupies, as a fraction of the display.
    SetWidth { window: WinID, ratio: f64 },
    /// Put `window` at an exact frame. Only meaningful for a floating window:
    /// the layout engine owns where a tiled one goes.
    SetFrame { window: WinID, frame: Frame },
    /// Put `window` into `onto`'s column, as a stack entry or a tab.
    Stack {
        window: WinID,
        onto: WinID,
        tabs: bool,
    },
    /// Give `window` a column of its own again.
    Unstack(WinID),
}

impl LayoutOp {
    /// The window this op acts on, if it names one. `None` for ops that act on
    /// a workspace as a whole.
    #[must_use]
    pub fn target(&self) -> Option<WinID> {
        match self {
            LayoutOp::Focus(window)
            | LayoutOp::Swap(window, _)
            | LayoutOp::Unstack(window)
            | LayoutOp::MoveToWorkspace { window, .. }
            | LayoutOp::SetFloating { window, .. }
            | LayoutOp::SetManaged { window, .. }
            | LayoutOp::SetWidth { window, .. }
            | LayoutOp::SetFrame { window, .. }
            | LayoutOp::Stack { window, .. } => Some(*window),
            LayoutOp::View { .. } => None,
        }
    }
}

/// A rectangle as fractions of a display, in the style of xmonad's
/// `RationalRect`. Proportional rather than absolute, so a scratchpad
/// placement means the same thing on a laptop panel and an external display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RelativeRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl RelativeRect {
    /// Resolves against a display's bounds.
    #[must_use]
    pub fn resolve(self, display: Frame) -> Frame {
        // The clamp is the truncation guard: whatever a script passes lands
        // inside `i32` before the cast, so there is nothing left to truncate.
        #[allow(clippy::cast_possible_truncation)]
        let scale = |fraction: f64, extent: i32| -> i32 {
            let scaled = fraction * f64::from(extent);
            // Saturating rather than wrapping: a script can pass anything.
            if scaled.is_finite() {
                scaled
                    .round()
                    .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
            } else {
                0
            }
        };
        Frame {
            x: display.x + scale(self.x, display.width),
            y: display.y + scale(self.y, display.height),
            width: scale(self.width, display.width).max(1),
            height: scale(self.height, display.height).max(1),
        }
    }
}

/// One link of the recorded op list. A cons list rather than a `Vec` so two
/// values branched off the same parent get independent tails without copying
/// the shared prefix.
#[derive(Debug)]
struct OpNode {
    op: LayoutOp,
    prev: Option<Arc<OpNode>>,
}

/// How a column arranges the windows in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    /// One window filling the column.
    Single,
    /// Windows stacked vertically, all visible.
    Stack,
    /// Windows sharing the column, one visible at a time.
    Tabs,
    /// One window covering the display.
    Fullscreen,
}

/// One window, as a script sees it.
// The four flags are genuinely independent -- a window can be any combination
// of floating, managed, visible and focused -- so there is no enum hiding here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowRec {
    pub id: WinID,
    pub app_name: String,
    pub bundle_id: String,
    pub title: String,
    /// Where it is now, in global display coordinates, when known.
    pub frame: Option<Frame>,
    /// Outside the tiling layout, positioned by hand.
    pub floating: bool,
    /// Laid out by the window manager at all.
    pub managed: bool,
    /// More than a sliver of it is actually showing.
    pub visible: bool,
    pub focused: bool,
}

/// One column of a workspace's layout strip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnSet {
    pub kind: ColumnKind,
    /// Width as a fraction of the display, as the layout engine has it.
    pub width_ratio: f64,
    /// Which of `windows` is on top, for tabs and stacks.
    pub selected: usize,
    pub windows: Arc<Vec<WindowRec>>,
}

impl ColumnSet {
    /// A column holding one window.
    #[must_use]
    pub fn single(window: WindowRec, width_ratio: f64) -> Self {
        Self {
            kind: ColumnKind::Single,
            width_ratio,
            selected: 0,
            windows: Arc::new(vec![window]),
        }
    }

    /// The window on top: the only one for a `Single`, the selected one
    /// otherwise.
    #[must_use]
    pub fn top(&self) -> Option<&WindowRec> {
        self.windows
            .get(self.selected)
            .or_else(|| self.windows.first())
    }
}

/// One virtual workspace: an ordered strip of columns, plus whatever floats
/// above it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSet {
    /// The virtual workspace number a script addresses it by.
    pub number: u32,
    /// The macOS space it lives on.
    pub native_id: u64,
    /// Whether it is the one currently shown on its display.
    pub active: bool,
    pub columns: Arc<Vec<ColumnSet>>,
    pub floating: Arc<Vec<WindowRec>>,
}

impl WorkspaceSet {
    /// Every window on the workspace, tiled first then floating.
    pub fn windows(&self) -> impl Iterator<Item = &WindowRec> {
        self.columns
            .iter()
            .flat_map(|column| column.windows.iter())
            .chain(self.floating.iter())
    }
}

/// One display and the workspaces on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplaySet {
    pub id: u32,
    pub frame: Frame,
    /// Whether it holds the focus.
    pub active: bool,
    pub workspaces: Arc<Vec<WorkspaceSet>>,
}

/// The whole layout, as a value.
///
/// See the module documentation for what "as a value" buys and what it costs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WindowSet {
    displays: Arc<Vec<DisplaySet>>,
    focused: Option<WinID>,
    /// What has been asked of this value, most recent first. Not part of the
    /// layout: two window sets are equal when they describe the same layout,
    /// however they got there, so this field is excluded from `PartialEq` and
    /// not serialized.
    #[serde(skip)]
    ops: Option<Arc<OpNode>>,
}

impl PartialEq for WindowSet {
    fn eq(&self, other: &Self) -> bool {
        self.displays == other.displays && self.focused == other.focused
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

impl WindowSet {
    /// Builds a window set from an extracted layout.
    #[must_use]
    pub fn new(displays: Vec<DisplaySet>, focused: Option<WinID>) -> Self {
        Self {
            displays: Arc::new(displays),
            focused,
            ops: None,
        }
    }

    #[must_use]
    pub fn displays(&self) -> &[DisplaySet] {
        &self.displays
    }

    /// The focused window's id, if anything is focused.
    #[must_use]
    pub fn focused(&self) -> Option<WinID> {
        self.focused
    }

    /// Every workspace, across every display.
    pub fn workspaces(&self) -> impl Iterator<Item = &WorkspaceSet> {
        self.displays
            .iter()
            .flat_map(|display| display.workspaces.iter())
    }

    /// Every window known to the layout.
    pub fn windows(&self) -> impl Iterator<Item = &WindowRec> {
        self.workspaces().flat_map(WorkspaceSet::windows)
    }

    /// One window by id.
    #[must_use]
    pub fn window(&self, id: WinID) -> Option<&WindowRec> {
        self.windows().find(|window| window.id == id)
    }

    /// The workspace numbered `number`.
    #[must_use]
    pub fn workspace(&self, number: u32) -> Option<&WorkspaceSet> {
        self.workspaces()
            .find(|workspace| workspace.number == number)
    }

    /// The active workspace of the active display — "here", for a script.
    #[must_use]
    pub fn current(&self) -> Option<&WorkspaceSet> {
        self.displays
            .iter()
            .find(|display| display.active)
            .or_else(|| self.displays.first())?
            .workspaces
            .iter()
            .find(|workspace| workspace.active)
    }

    /// The display showing `id`.
    #[must_use]
    pub fn display_of(&self, id: WinID) -> Option<&DisplaySet> {
        self.displays.iter().find(|display| {
            display
                .workspaces
                .iter()
                .any(|workspace| workspace.windows().any(|window| window.id == id))
        })
    }

    /// The workspace holding `id`.
    #[must_use]
    pub fn workspace_of(&self, id: WinID) -> Option<&WorkspaceSet> {
        self.workspaces()
            .find(|workspace| workspace.windows().any(|window| window.id == id))
    }

    /// Which column of its workspace holds `id`, counted from the left.
    #[must_use]
    pub fn column_of(&self, id: WinID) -> Option<usize> {
        let workspace = self.workspace_of(id)?;
        workspace
            .columns
            .iter()
            .position(|column| column.windows.iter().any(|window| window.id == id))
    }

    /// The window one column to the east (right), staying on the workspace.
    #[must_use]
    pub fn east(&self, id: WinID) -> Option<WinID> {
        self.neighbour(id, 1)
    }

    /// The window one column to the west (left), staying on the workspace.
    #[must_use]
    pub fn west(&self, id: WinID) -> Option<WinID> {
        self.neighbour(id, -1)
    }

    /// The next window in the workspace's own order, wrapping at the end.
    #[must_use]
    pub fn next(&self, id: WinID) -> Option<WinID> {
        self.cycle(id, 1)
    }

    /// The previous window in the workspace's own order, wrapping at the start.
    #[must_use]
    pub fn prev(&self, id: WinID) -> Option<WinID> {
        self.cycle(id, -1)
    }

    /// The top window of the column `offset` columns away, if there is one.
    fn neighbour(&self, id: WinID, offset: isize) -> Option<WinID> {
        let workspace = self.workspace_of(id)?;
        let column = self.column_of(id)?;
        let target = usize::try_from(isize::try_from(column).ok()? + offset).ok()?;
        workspace.columns.get(target)?.top().map(|window| window.id)
    }

    /// The window `offset` places away in workspace order, wrapping around.
    fn cycle(&self, id: WinID, offset: isize) -> Option<WinID> {
        let workspace = self.workspace_of(id)?;
        let ids: Vec<WinID> = workspace.windows().map(|window| window.id).collect();
        if ids.is_empty() {
            return None;
        }
        let at = ids.iter().position(|&window| window == id)?;
        let length = isize::try_from(ids.len()).ok()?;
        let index = (isize::try_from(at).ok()? + offset).rem_euclid(length);
        ids.get(usize::try_from(index).ok()?).copied()
    }

    /// The ops recorded on this value, oldest first. This is what the host
    /// replays; an untransformed value yields nothing.
    #[must_use]
    pub fn ops(&self) -> Vec<LayoutOp> {
        let mut ops = Vec::new();
        let mut node = self.ops.as_ref();
        while let Some(current) = node {
            ops.push(current.op);
            node = current.prev.as_ref();
        }
        ops.reverse();
        ops
    }

    /// Whether anything has been asked of this value.
    #[must_use]
    pub fn is_transformed(&self) -> bool {
        self.ops.is_some()
    }
}

// ---------------------------------------------------------------------------
// Transforming
// ---------------------------------------------------------------------------

impl WindowSet {
    /// Returns a copy of `self` with `op` recorded and `edit` applied to the
    /// tree. The engine every transform below is built from: `self` is never
    /// touched, and only the spine `edit` reaches gets copied.
    fn with(&self, op: LayoutOp, edit: impl FnOnce(&mut [DisplaySet])) -> Self {
        let mut next = self.recording(op);
        let displays: &mut Vec<DisplaySet> = Arc::make_mut(&mut next.displays);
        edit(displays);
        next
    }

    /// Records `op` without changing the tree at all — not even copying it.
    fn recording(&self, op: LayoutOp) -> Self {
        Self {
            displays: Arc::clone(&self.displays),
            focused: self.focused,
            ops: Some(Arc::new(OpNode {
                op,
                prev: self.ops.clone(),
            })),
        }
    }

    /// Focuses `window`.
    #[must_use]
    pub fn focus(&self, window: WinID) -> Self {
        let mut next = self.with(LayoutOp::Focus(window), |displays| {
            for_each_window(displays, |record| record.focused = record.id == window);
        });
        next.focused = Some(window);
        next
    }

    /// Exchanges two windows' places in the layout. A no-op on the tree if
    /// either is missing, though the intent is still recorded — the host may
    /// well be able to resolve a window this snapshot has already lost.
    #[must_use]
    pub fn swap(&self, first: WinID, second: WinID) -> Self {
        self.with(LayoutOp::Swap(first, second), |displays| {
            let (Some(left), Some(right)) = (
                find_window(displays, first).cloned(),
                find_window(displays, second).cloned(),
            ) else {
                return;
            };
            for_each_window(displays, |record| {
                if record.id == first {
                    let focused = record.focused;
                    *record = right.clone();
                    record.focused = focused;
                } else if record.id == second {
                    let focused = record.focused;
                    *record = left.clone();
                    record.focused = focused;
                }
            });
        })
    }

    /// Sends `window` to virtual workspace `workspace`, without following it.
    #[must_use]
    pub fn shift(&self, window: WinID, workspace: u32) -> Self {
        self.shift_following(window, workspace, false)
    }

    /// Sends `window` to virtual workspace `workspace`, following it there.
    #[must_use]
    pub fn shift_following(&self, window: WinID, workspace: u32, follow: bool) -> Self {
        self.with(
            LayoutOp::MoveToWorkspace {
                window,
                workspace,
                follow,
            },
            |displays| {
                // Check the destination before lifting the window out, so a
                // missing workspace doesn't lose the window from the tree.
                if !displays.iter().any(|display| {
                    display
                        .workspaces
                        .iter()
                        .any(|candidate| candidate.number == workspace)
                }) {
                    return;
                }
                let Some(record) = take_window(displays, window) else {
                    return;
                };
                if let Some(target) = find_workspace_mut(displays, workspace) {
                    Arc::make_mut(&mut target.columns).push(ColumnSet::single(record, 0.5));
                }
            },
        )
    }

    /// Shows virtual workspace `workspace` on its display.
    #[must_use]
    pub fn view(&self, workspace: u32) -> Self {
        self.with(LayoutOp::View { workspace }, |displays| {
            let on_display = displays.iter().position(|display| {
                display
                    .workspaces
                    .iter()
                    .any(|candidate| candidate.number == workspace)
            });
            let Some(index) = on_display else {
                return;
            };
            for candidate in Arc::make_mut(&mut displays[index].workspaces) {
                candidate.active = candidate.number == workspace;
            }
        })
    }

    /// Takes `window` out of the tiling layout, leaving it where it is —
    /// xmonad's `defaultFloating`.
    #[must_use]
    pub fn float(&self, window: WinID) -> Self {
        self.set_floating(window, true)
    }

    /// Takes `window` out of the tiling layout and puts it at `rect` —
    /// xmonad's `customFloating`.
    ///
    /// The fractions are resolved against the display the window is on *in this
    /// snapshot*, so the op carries an absolute frame and the returned tree can
    /// show where the window ended up.
    #[must_use]
    pub fn float_at(&self, window: WinID, rect: RelativeRect) -> Self {
        let display = self
            .display_of(window)
            .or_else(|| self.displays.iter().find(|display| display.active))
            .or_else(|| self.displays.first());
        let Some(frame) = display.map(|display| rect.resolve(display.frame)) else {
            // No display to resolve against; floating alone is still meaningful.
            return self.float(window);
        };
        self.float(window).set_frame(window, frame)
    }

    /// Puts `window` at an exact frame, in global display coordinates.
    #[must_use]
    pub fn set_frame(&self, window: WinID, frame: Frame) -> Self {
        self.with(LayoutOp::SetFrame { window, frame }, |displays| {
            for_each_window(displays, |record| {
                if record.id == window {
                    record.frame = Some(frame);
                }
            });
        })
    }

    /// Puts a floating `window` back into the tiling layout.
    #[must_use]
    pub fn sink(&self, window: WinID) -> Self {
        self.set_floating(window, false)
    }

    fn set_floating(&self, window: WinID, floating: bool) -> Self {
        self.with(LayoutOp::SetFloating { window, floating }, |displays| {
            let Some(mut record) = take_window(displays, window) else {
                return;
            };
            record.floating = floating;
            // The window stays where it was; only which side of the workspace
            // it sits on changes.
            let target = displays.iter_mut().find_map(|display| {
                Arc::make_mut(&mut display.workspaces)
                    .iter_mut()
                    .find(|workspace| workspace.active)
            });
            let Some(target) = target else {
                return;
            };
            if floating {
                Arc::make_mut(&mut target.floating).push(record);
            } else {
                Arc::make_mut(&mut target.columns).push(ColumnSet::single(record, 0.5));
            }
        })
    }

    /// Starts laying `window` out.
    #[must_use]
    pub fn manage(&self, window: WinID) -> Self {
        self.set_managed(window, true)
    }

    /// Stops laying `window` out, leaving it where it is.
    #[must_use]
    pub fn unmanage(&self, window: WinID) -> Self {
        self.set_managed(window, false)
    }

    fn set_managed(&self, window: WinID, managed: bool) -> Self {
        self.with(LayoutOp::SetManaged { window, managed }, |displays| {
            for_each_window(displays, |record| {
                if record.id == window {
                    record.managed = managed;
                }
            });
        })
    }

    /// Sets the width of `window`'s column, as a fraction of the display.
    #[must_use]
    pub fn width(&self, window: WinID, ratio: f64) -> Self {
        self.with(LayoutOp::SetWidth { window, ratio }, |displays| {
            for_each_column(displays, |column| {
                if column.windows.iter().any(|record| record.id == window) {
                    column.width_ratio = ratio;
                }
            });
        })
    }

    /// Puts `window` into `onto`'s column as a stack entry.
    #[must_use]
    pub fn stack(&self, window: WinID, onto: WinID) -> Self {
        self.stack_as(window, onto, false)
    }

    /// Puts `window` into `onto`'s column as a tab.
    #[must_use]
    pub fn tab(&self, window: WinID, onto: WinID) -> Self {
        self.stack_as(window, onto, true)
    }

    fn stack_as(&self, window: WinID, onto: WinID, tabs: bool) -> Self {
        self.with(LayoutOp::Stack { window, onto, tabs }, |displays| {
            // Same reasoning as `shift`: do not lift the window out unless
            // there is somewhere to put it.
            if find_window(displays, onto).is_none() {
                return;
            }
            let Some(record) = take_window(displays, window) else {
                return;
            };
            let mut record = Some(record);
            for_each_column(displays, |column| {
                if record.is_none() || !column.windows.iter().any(|held| held.id == onto) {
                    return;
                }
                column.kind = if tabs {
                    ColumnKind::Tabs
                } else {
                    ColumnKind::Stack
                };
                Arc::make_mut(&mut column.windows).push(record.take().expect("checked above"));
            });
        })
    }

    /// Gives `window` a column of its own again.
    #[must_use]
    pub fn unstack(&self, window: WinID) -> Self {
        self.with(LayoutOp::Unstack(window), |displays| {
            let Some(record) = take_window(displays, window) else {
                return;
            };
            let target = displays.iter_mut().find_map(|display| {
                Arc::make_mut(&mut display.workspaces)
                    .iter_mut()
                    .find(|workspace| workspace.active)
            });
            if let Some(target) = target {
                Arc::make_mut(&mut target.columns).push(ColumnSet::single(record, 0.5));
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tree helpers
// ---------------------------------------------------------------------------

/// Visits every window record in the tree, copying only what it reaches.
fn for_each_window(displays: &mut [DisplaySet], mut visit: impl FnMut(&mut WindowRec)) {
    for display in displays.iter_mut() {
        for workspace in Arc::make_mut(&mut display.workspaces) {
            for column in Arc::make_mut(&mut workspace.columns) {
                for window in Arc::make_mut(&mut column.windows) {
                    visit(window);
                }
            }
            for window in Arc::make_mut(&mut workspace.floating) {
                visit(window);
            }
        }
    }
}

/// Visits every column in the tree.
fn for_each_column(displays: &mut [DisplaySet], mut visit: impl FnMut(&mut ColumnSet)) {
    for display in displays.iter_mut() {
        for workspace in Arc::make_mut(&mut display.workspaces) {
            for column in Arc::make_mut(&mut workspace.columns) {
                visit(column);
            }
        }
    }
}

/// Finds a window without copying anything.
fn find_window(displays: &[DisplaySet], id: WinID) -> Option<&WindowRec> {
    displays
        .iter()
        .flat_map(|display| display.workspaces.iter())
        .flat_map(WorkspaceSet::windows)
        .find(|window| window.id == id)
}

/// Finds a workspace by number, ready to be changed.
fn find_workspace_mut(displays: &mut [DisplaySet], number: u32) -> Option<&mut WorkspaceSet> {
    displays.iter_mut().find_map(|display| {
        Arc::make_mut(&mut display.workspaces)
            .iter_mut()
            .find(|workspace| workspace.number == number)
    })
}

/// Removes a window from wherever it is, leaving no empty column behind, and
/// hands it back for the caller to place somewhere else.
fn take_window(displays: &mut [DisplaySet], id: WinID) -> Option<WindowRec> {
    for display in displays.iter_mut() {
        for workspace in Arc::make_mut(&mut display.workspaces) {
            let columns = Arc::make_mut(&mut workspace.columns);
            for index in 0..columns.len() {
                let windows = Arc::make_mut(&mut columns[index].windows);
                if let Some(at) = windows.iter().position(|window| window.id == id) {
                    let taken = windows.remove(at);
                    if windows.is_empty() {
                        columns.remove(index);
                    } else {
                        let column = &mut columns[index];
                        column.selected = column.selected.min(column.windows.len() - 1);
                        if column.windows.len() == 1 {
                            column.kind = ColumnKind::Single;
                        }
                    }
                    return Some(taken);
                }
            }
            let floating = Arc::make_mut(&mut workspace.floating);
            if let Some(at) = floating.iter().position(|window| window.id == id) {
                return Some(floating.remove(at));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: WinID, name: &str) -> WindowRec {
        WindowRec {
            id,
            app_name: name.to_string(),
            bundle_id: format!("com.example.{name}"),
            title: format!("{name} window"),
            frame: None,
            floating: false,
            managed: true,
            visible: true,
            focused: false,
        }
    }

    /// One display, two workspaces; workspace 1 is active and holds three
    /// single-window columns, workspace 2 is empty.
    fn fixture() -> WindowSet {
        let columns: Vec<ColumnSet> = [(1, "alpha"), (2, "beta"), (3, "gamma")]
            .into_iter()
            .map(|(id, name)| {
                let mut record = window(id, name);
                record.focused = id == 1;
                ColumnSet::single(record, 0.33)
            })
            .collect();

        WindowSet::new(
            vec![DisplaySet {
                id: 1,
                frame: Frame {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                active: true,
                workspaces: Arc::new(vec![
                    WorkspaceSet {
                        number: 1,
                        native_id: 10,
                        active: true,
                        columns: Arc::new(columns),
                        floating: Arc::new(Vec::new()),
                    },
                    WorkspaceSet {
                        number: 2,
                        native_id: 11,
                        active: false,
                        columns: Arc::new(Vec::new()),
                        floating: Arc::new(Vec::new()),
                    },
                ]),
            }],
            Some(1),
        )
    }

    #[test]
    fn a_fresh_window_set_has_asked_for_nothing() {
        let set = fixture();
        assert!(set.ops().is_empty());
        assert!(!set.is_transformed());
    }

    #[test]
    fn transforms_leave_the_original_alone() {
        let before = fixture();
        let after = before.focus(3);

        assert_eq!(
            before.focused(),
            Some(1),
            "the original still has its focus"
        );
        assert!(
            before.ops().is_empty(),
            "and has still asked for nothing: {:?}",
            before.ops()
        );
        assert_eq!(after.focused(), Some(3));
        assert_eq!(after.ops(), vec![LayoutOp::Focus(3)]);
        assert!(after.window(3).unwrap().focused);
        assert!(!after.window(1).unwrap().focused);
    }

    #[test]
    fn chained_transforms_record_in_order() {
        let set = fixture().focus(2).width(2, 0.75).shift(2, 2);
        assert_eq!(
            set.ops(),
            vec![
                LayoutOp::Focus(2),
                LayoutOp::SetWidth {
                    window: 2,
                    ratio: 0.75
                },
                LayoutOp::MoveToWorkspace {
                    window: 2,
                    workspace: 2,
                    follow: false
                },
            ]
        );
    }

    #[test]
    fn branches_do_not_see_each_others_ops() {
        let base = fixture();
        let left = base.focus(2);
        let right = base.focus(3);

        assert_eq!(left.ops(), vec![LayoutOp::Focus(2)]);
        assert_eq!(right.ops(), vec![LayoutOp::Focus(3)]);
        assert!(base.ops().is_empty());
        assert_eq!(base.focused(), Some(1));
    }

    #[test]
    fn untouched_subtrees_are_shared_not_copied() {
        let base = fixture();
        let clone = base.clone();
        assert!(
            Arc::ptr_eq(&base.displays, &clone.displays),
            "cloning should share, not copy"
        );

        let edited = base.width(1, 0.9);
        let before = &base.displays()[0].workspaces[1];
        let after = &edited.displays()[0].workspaces[1];
        assert_eq!(before, after, "the untouched workspace keeps its contents");
    }

    #[test]
    fn shift_moves_the_window_between_workspaces() {
        let set = fixture().shift(2, 2);
        assert!(
            set.workspace(1).unwrap().windows().all(|w| w.id != 2),
            "the window has left its old workspace"
        );
        assert!(
            set.workspace(2).unwrap().windows().any(|w| w.id == 2),
            "...and arrived at the new one"
        );
        assert_eq!(
            set.workspace(1).unwrap().columns.len(),
            2,
            "its emptied column is gone"
        );
    }

    #[test]
    fn swap_exchanges_positions_not_focus() {
        let set = fixture().swap(1, 3);
        let workspace = set.workspace(1).unwrap();
        let order: Vec<WinID> = workspace
            .columns
            .iter()
            .filter_map(|column| column.top().map(|window| window.id))
            .collect();
        assert_eq!(order, vec![3, 2, 1]);
        assert_eq!(set.focused(), Some(1), "swapping does not move the focus");
    }

    #[test]
    fn stacking_merges_columns_and_unstacking_splits_them() {
        let stacked = fixture().stack(2, 1);
        let workspace = stacked.workspace(1).unwrap();
        assert_eq!(workspace.columns.len(), 2, "two columns became one");
        assert_eq!(workspace.columns[0].kind, ColumnKind::Stack);
        assert_eq!(workspace.columns[0].windows.len(), 2);

        let split = stacked.unstack(2);
        let workspace = split.workspace(1).unwrap();
        assert_eq!(workspace.columns.len(), 3);
        assert_eq!(
            workspace.columns[0].kind,
            ColumnKind::Single,
            "the column it left is single again"
        );
    }

    #[test]
    fn floating_moves_a_window_off_the_strip_and_back() {
        let floated = fixture().float(2);
        let workspace = floated.workspace(1).unwrap();
        assert_eq!(workspace.columns.len(), 2);
        assert_eq!(workspace.floating.len(), 1);
        assert!(workspace.floating[0].floating);

        let sunk = floated.sink(2);
        let workspace = sunk.workspace(1).unwrap();
        assert_eq!(workspace.columns.len(), 3);
        assert!(workspace.floating.is_empty());
    }

    #[test]
    fn float_at_resolves_fractions_against_the_display() {
        // The fixture's display is 1920x1080 at the origin.
        let set = fixture().float_at(
            2,
            RelativeRect {
                x: 0.25,
                y: 0.5,
                width: 0.5,
                height: 0.25,
            },
        );
        let placed = Frame {
            x: 480,
            y: 540,
            width: 960,
            height: 270,
        };
        assert_eq!(
            set.ops(),
            vec![
                LayoutOp::SetFloating {
                    window: 2,
                    floating: true
                },
                LayoutOp::SetFrame {
                    window: 2,
                    frame: placed
                },
            ],
            "customFloating is float-then-place"
        );
        let window = set.window(2).expect("the window is still there");
        assert_eq!(window.frame, Some(placed));
        assert!(window.floating);
    }

    #[test]
    fn a_relative_rect_is_offset_by_the_display_origin() {
        let second = Frame {
            x: 1920,
            y: -200,
            width: 1000,
            height: 800,
        };
        let full = RelativeRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        assert_eq!(full.resolve(second), second, "a full rect is the display");

        let half = RelativeRect {
            x: 0.5,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        assert_eq!(
            half.resolve(second),
            Frame {
                x: 2420,
                y: -200,
                width: 500,
                height: 800
            }
        );
    }

    #[test]
    fn a_degenerate_rect_still_yields_a_usable_frame() {
        // Scripts can pass anything; a zero-size or non-finite rect must not
        // produce a window that cannot be seen or a panicking cast.
        let display = Frame {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let zero = RelativeRect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
        let resolved = zero.resolve(display);
        assert_eq!((resolved.width, resolved.height), (1, 1));

        let nonsense = RelativeRect {
            x: f64::NAN,
            y: f64::INFINITY,
            width: 1e30,
            height: -1.0,
        };
        let resolved = nonsense.resolve(display);
        assert!(resolved.width >= 1 && resolved.height >= 1);
    }

    #[test]
    fn view_switches_which_workspace_is_active() {
        let set = fixture().view(2);
        assert!(!set.workspace(1).unwrap().active);
        assert!(set.workspace(2).unwrap().active);
        assert_eq!(set.ops(), vec![LayoutOp::View { workspace: 2 }]);
    }

    #[test]
    fn navigation_follows_the_strip_and_wraps() {
        let set = fixture();
        assert_eq!(set.east(1), Some(2));
        assert_eq!(set.west(2), Some(1));
        assert_eq!(set.west(1), None, "nothing west of the first column");
        assert_eq!(set.next(3), Some(1), "next wraps around");
        assert_eq!(set.prev(1), Some(3), "and so does prev");
    }

    #[test]
    fn lookups_find_where_a_window_lives() {
        let set = fixture();
        assert_eq!(set.column_of(2), Some(1));
        assert_eq!(set.workspace_of(2).map(|w| w.number), Some(1));
        assert_eq!(set.display_of(2).map(|d| d.id), Some(1));
        assert_eq!(set.current().map(|w| w.number), Some(1));
        assert_eq!(set.window(2).map(|w| w.app_name.as_str()), Some("beta"));
        assert_eq!(set.window(99), None);
    }

    #[test]
    fn ops_naming_a_window_expose_it_for_resolution() {
        assert_eq!(LayoutOp::Focus(4).target(), Some(4));
        assert_eq!(
            LayoutOp::Stack {
                window: 4,
                onto: 5,
                tabs: true
            }
            .target(),
            Some(4)
        );
        assert_eq!(LayoutOp::View { workspace: 2 }.target(), None);
    }

    #[test]
    fn shifting_to_a_workspace_the_snapshot_lacks_keeps_the_window() {
        let set = fixture().shift(2, 42);
        assert!(
            set.window(2).is_some(),
            "the window should still be somewhere"
        );
        assert_eq!(set.workspace_of(2).map(|w| w.number), Some(1));
        assert_eq!(
            set.ops(),
            vec![LayoutOp::MoveToWorkspace {
                window: 2,
                workspace: 42,
                follow: false
            }]
        );
    }

    #[test]
    fn stacking_onto_a_window_the_snapshot_lacks_keeps_the_window() {
        let set = fixture().stack(2, 99);
        assert!(
            set.window(2).is_some(),
            "the window should still be somewhere"
        );
        assert_eq!(set.workspace(1).unwrap().columns.len(), 3);
    }

    #[test]
    fn transforming_a_missing_window_still_records_the_intent() {
        let set = fixture().focus(99);
        assert_eq!(set.ops(), vec![LayoutOp::Focus(99)]);
    }
}
