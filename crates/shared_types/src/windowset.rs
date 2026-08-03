//! A window layout as a **pure value**, in the style of xmonad's `StackSet`.
//!
//! The window manager's real layout lives in the ECS and can only be changed by
//! systems. This is a photograph of it that a script can transform freely:
//! every operation returns a *new* [`WindowSet`] instead of mutating the one it
//! was given, so a handler can branch, compute speculatively, and discard what
//! it doesn't want, without any of it reaching a real window.
//!
//! What makes that useful rather than merely tidy is that each transform also
//! records what it *meant* — a [`LayoutOp`] — onto the value it returns. The
//! handler's return value therefore carries both the layout it wants and the
//! sequence of intents that produced it; the host replays the intents against
//! the live world. A value the handler computes but does not return carries its
//! ops off to the garbage collector, which is exactly the behaviour you want
//! from `if some_condition then return ws:swap(a, b) end`.
//!
//! ```text
//! ws                       -- ops: []
//!   :focus(w)              -- ops: [Focus(w)]
//!   :shift(w, 3)           -- ops: [Focus(w), MoveToWorkspace(w, 3)]
//! ```
//!
//! # Cost
//!
//! Every level is behind an [`Rc`], so cloning a `WindowSet` is a handful of
//! refcount bumps and a transform copies only the spine it touches
//! ([`Rc::make_mut`]). Two values branched off one parent share everything they
//! didn't change, including the common prefix of their op lists.
//!
//! # Scope
//!
//! Every transform here changes the layout in a way that follows from the
//! layout alone — focus, ordering, workspace membership, stacking, floating,
//! width ratios — and can therefore be both reflected in the returned tree and
//! replayed faithfully against one named window.
//!
//! The operations that are the layout engine's to decide (centring,
//! full-width, equalise, balance, raising a float, moving to another display)
//! are deliberately absent. They act on whatever is focused rather than on a
//! window you name, and a value that recorded them could neither show their
//! result nor promise it applied to the window you meant. They remain available
//! as the imperative `paneru.window.*` verbs, where that is exactly what they
//! say they do.
//!
//! Even for what is here, the returned tree is a prediction: the layout engine
//! settles the actual geometry. A handler that needs the settled result should
//! read it from the next event's `WindowSet`, not from the one it just built.

use std::rc::Rc;

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
            | LayoutOp::Stack { window, .. } => Some(*window),
            LayoutOp::View { .. } => None,
        }
    }
}

/// One link of the recorded op list.
///
/// A cons list rather than a `Vec`: appending is O(1) and, more to the point,
/// two values branched off the same parent get independent tails without
/// copying the prefix they share.
#[derive(Debug)]
struct OpNode {
    op: LayoutOp,
    prev: Option<Rc<OpNode>>,
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
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSet {
    pub kind: ColumnKind,
    /// Width as a fraction of the display, as the layout engine has it.
    pub width_ratio: f64,
    /// Which of `windows` is on top, for tabs and stacks.
    pub selected: usize,
    pub windows: Rc<Vec<WindowRec>>,
}

impl ColumnSet {
    /// A column holding one window.
    #[must_use]
    pub fn single(window: WindowRec, width_ratio: f64) -> Self {
        Self {
            kind: ColumnKind::Single,
            width_ratio,
            selected: 0,
            windows: Rc::new(vec![window]),
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
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSet {
    /// The virtual workspace number a script addresses it by.
    pub number: u32,
    /// The macOS space it lives on.
    pub native_id: u64,
    /// Whether it is the one currently shown on its display.
    pub active: bool,
    pub columns: Rc<Vec<ColumnSet>>,
    pub floating: Rc<Vec<WindowRec>>,
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
#[derive(Clone, Debug, PartialEq)]
pub struct DisplaySet {
    pub id: u32,
    pub frame: Frame,
    /// Whether it holds the focus.
    pub active: bool,
    pub workspaces: Rc<Vec<WorkspaceSet>>,
}

/// The whole layout, as a value.
///
/// See the module documentation for what "as a value" buys and what it costs.
#[derive(Clone, Debug, Default)]
pub struct WindowSet {
    displays: Rc<Vec<DisplaySet>>,
    focused: Option<WinID>,
    /// What has been asked of this value, most recent first. Not part of the
    /// layout, and deliberately not compared: two window sets are equal when
    /// they describe the same layout, however they got there.
    ops: Option<Rc<OpNode>>,
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
            displays: Rc::new(displays),
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
        let displays: &mut Vec<DisplaySet> = Rc::make_mut(&mut next.displays);
        edit(displays);
        next
    }

    /// Records `op` without changing the tree at all — not even copying it. For
    /// the ops whose outcome is the layout engine's to decide, see the note on
    /// fidelity in the module docs.
    fn recording(&self, op: LayoutOp) -> Self {
        Self {
            displays: Rc::clone(&self.displays),
            focused: self.focused,
            ops: Some(Rc::new(OpNode {
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
                let Some(record) = take_window(displays, window) else {
                    return;
                };
                let ratio = record.frame.map_or(0.5, |_| 0.5);
                if let Some(target) = find_workspace_mut(displays, workspace) {
                    Rc::make_mut(&mut target.columns).push(ColumnSet::single(record, ratio));
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
            for candidate in Rc::make_mut(&mut displays[index].workspaces) {
                candidate.active = candidate.number == workspace;
            }
        })
    }

    /// Takes `window` out of the tiling layout.
    #[must_use]
    pub fn float(&self, window: WinID) -> Self {
        self.set_floating(window, true)
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
                Rc::make_mut(&mut display.workspaces)
                    .iter_mut()
                    .find(|workspace| workspace.active)
            });
            let Some(target) = target else {
                return;
            };
            if floating {
                Rc::make_mut(&mut target.floating).push(record);
            } else {
                Rc::make_mut(&mut target.columns).push(ColumnSet::single(record, 0.5));
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
                Rc::make_mut(&mut column.windows).push(record.take().expect("checked above"));
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
                Rc::make_mut(&mut display.workspaces)
                    .iter_mut()
                    .find(|workspace| workspace.active)
            });
            if let Some(target) = target {
                Rc::make_mut(&mut target.columns).push(ColumnSet::single(record, 0.5));
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
        for workspace in Rc::make_mut(&mut display.workspaces) {
            for column in Rc::make_mut(&mut workspace.columns) {
                for window in Rc::make_mut(&mut column.windows) {
                    visit(window);
                }
            }
            for window in Rc::make_mut(&mut workspace.floating) {
                visit(window);
            }
        }
    }
}

/// Visits every column in the tree.
fn for_each_column(displays: &mut [DisplaySet], mut visit: impl FnMut(&mut ColumnSet)) {
    for display in displays.iter_mut() {
        for workspace in Rc::make_mut(&mut display.workspaces) {
            for column in Rc::make_mut(&mut workspace.columns) {
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
        Rc::make_mut(&mut display.workspaces)
            .iter_mut()
            .find(|workspace| workspace.number == number)
    })
}

/// Removes a window from wherever it is, leaving no empty column behind, and
/// hands it back for the caller to place somewhere else.
fn take_window(displays: &mut [DisplaySet], id: WinID) -> Option<WindowRec> {
    for display in displays.iter_mut() {
        for workspace in Rc::make_mut(&mut display.workspaces) {
            let columns = Rc::make_mut(&mut workspace.columns);
            for index in 0..columns.len() {
                let windows = Rc::make_mut(&mut columns[index].windows);
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
            let floating = Rc::make_mut(&mut workspace.floating);
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
                workspaces: Rc::new(vec![
                    WorkspaceSet {
                        number: 1,
                        native_id: 10,
                        active: true,
                        columns: Rc::new(columns),
                        floating: Rc::new(Vec::new()),
                    },
                    WorkspaceSet {
                        number: 2,
                        native_id: 11,
                        active: false,
                        columns: Rc::new(Vec::new()),
                        floating: Rc::new(Vec::new()),
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
        // ...and neither reached the value they branched from.
        assert!(base.ops().is_empty());
        assert_eq!(base.focused(), Some(1));
    }

    #[test]
    fn untouched_subtrees_are_shared_not_copied() {
        let base = fixture();
        // Cloning shares the whole tree; nothing is copied until something is
        // changed, which is what makes speculative branching cheap.
        let clone = base.clone();
        assert!(
            Rc::ptr_eq(&base.displays, &clone.displays),
            "cloning should share, not copy"
        );

        // A transform copies the spine it edits but leaves the contents it
        // never reached alone: workspace 2 is untouched by a change to
        // workspace 1.
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
    fn transforming_a_missing_window_still_records_the_intent() {
        // The snapshot is a frame stale; the host may still resolve the window.
        let set = fixture().focus(99);
        assert_eq!(set.ops(), vec![LayoutOp::Focus(99)]);
    }
}
