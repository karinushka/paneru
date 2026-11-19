//! Native macOS Space census domain model, `DesktopBridge` abstraction, and
//! deterministic in-memory `FakeDesktopBridge` for headless ECS testing.

pub mod bridge;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use objc2_core_graphics::CGDirectDisplayID;

use crate::platform::{Pid, WinID, WorkspaceId};

/// Bit 11 (`0x800`) in `SLSWindowIteratorGetTags`: set when a window carries
/// `NSWindowCollectionBehaviorCanJoinAllSpaces` or its owning process is
/// assigned to all Desktops.
pub const STICKY_WINDOW_TAG: u64 = 0x800;

/// Manageable `CoreGraphics` application window layers (`0` = normal,
/// `3` = floating/utility panel, `8` = modal panel).
pub const APP_WINDOW_LAYERS: [i32; 3] = [0, 3, 8];

/// Non-zero native `SkyLight` Space identifier (`id64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpaceId(pub NonZeroU64);

impl SpaceId {
    #[must_use]
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for SpaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Classification of a `SkyLight` Space (`"type"` in `SLSCopyManagedDisplaySpaces`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceKind {
    /// Ordinary user Desktop (`type == 0`).
    Desktop,
    /// Fullscreen or Split-View Space (`type == 4`).
    Fullscreen {
        /// Owning fullscreen window (`"fs_wid"`), when unambiguously reported.
        owner_wid: Option<WinID>,
        /// True when `TileSpaces` reports a single tile owned by `owner_wid`
        /// (eligible for guest-window rescue under §5.6 Case B).
        single_tile: bool,
    },
    /// Any other `WindowServer` Space type.
    Other(i32),
}

impl SpaceKind {
    #[must_use]
    pub const fn is_desktop(self) -> bool {
        matches!(self, Self::Desktop)
    }

    #[must_use]
    pub const fn is_fullscreen(self) -> bool {
        matches!(self, Self::Fullscreen { .. })
    }
}

/// One Space entry on a managed display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceRecord {
    pub id: SpaceId,
    pub uuid: String,
    pub display_id: CGDirectDisplayID,
    /// 0-based position within `display_id`'s managed `"Spaces"` array (includes Fullscreen).
    pub space_index: usize,
    /// 1-based ordinal among ordinary Desktops (`SpaceKind::Desktop`) on `display_id`,
    /// matching Mission Control's per-display Desktop ordering; `None` for Fullscreen.
    pub desktop_number: Option<u32>,
    pub kind: SpaceKind,
    pub active: bool,
}

/// Raw display snapshot before cross-display validation.
#[derive(Debug, Clone)]
pub struct RawDisplaySpaces {
    pub display_id: CGDirectDisplayID,
    pub skylight_display: String,
    pub current_space: SpaceId,
    pub spaces: Vec<(SpaceId, String, SpaceKind)>,
}

/// Validated Spaces owned by a single display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaySpaces {
    pub display_id: CGDirectDisplayID,
    pub skylight_display: String,
    pub current_space: SpaceId,
    pub spaces: Vec<SpaceRecord>,
}

impl DisplaySpaces {
    /// Number of ordinary `SpaceKind::Desktop` Spaces on this display.
    #[must_use]
    pub fn desktop_count(&self) -> usize {
        self.spaces.iter().filter(|s| s.kind.is_desktop()).count()
    }

    /// Looks up the `n`th (1-based) ordinary Desktop on this display.
    #[must_use]
    pub fn desktop_by_number(&self, number: u32) -> Option<&SpaceRecord> {
        (number > 0)
            .then(|| {
                self.spaces
                    .iter()
                    .find(|s| s.desktop_number == Some(number))
            })
            .flatten()
    }

    /// True if every managed Space on this display is an ordinary Desktop (`type == 0`).
    #[must_use]
    pub fn all_desktops(&self) -> bool {
        self.spaces.iter().all(|s| s.kind.is_desktop())
    }
}

/// Validated snapshot of all managed Spaces across all connected displays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpaceCensus {
    pub displays: Vec<DisplaySpaces>,
}

impl SpaceCensus {
    /// Validates raw display entries (non-empty displays, unique non-zero
    /// display IDs and Space IDs, non-empty spaces per display, active
    /// `current_space` present on every display) and assigns 0-based
    /// `space_index` and 1-based `desktop_number`.
    pub fn from_raw(raw_displays: Vec<RawDisplaySpaces>) -> Result<Self, SpaceError> {
        if raw_displays.is_empty() {
            return Err(SpaceError::new(
                SpaceErrorKind::QueryFailed,
                "SkyLight census returned zero displays",
            ));
        }
        let mut seen_displays = HashSet::new();
        let mut seen_ids = HashSet::new();
        let displays = raw_displays
            .into_iter()
            .map(|raw| {
                if raw.display_id == 0 || !seen_displays.insert(raw.display_id) {
                    return Err(SpaceError::new(
                        SpaceErrorKind::QueryFailed,
                        format!(
                            "invalid or duplicate display id {} in SkyLight census",
                            raw.display_id
                        ),
                    ));
                }
                if raw.skylight_display.is_empty() || raw.spaces.is_empty() {
                    return Err(SpaceError::new(
                        SpaceErrorKind::QueryFailed,
                        "display in SkyLight census has empty identifier or zero spaces",
                    ));
                }
                let mut desktop_ordinal = 0u32;
                let mut found_current = false;
                let spaces = raw
                    .spaces
                    .into_iter()
                    .enumerate()
                    .map(|(space_index, (id, uuid, kind))| {
                        if !seen_ids.insert(id) {
                            return Err(SpaceError::new(
                                SpaceErrorKind::QueryFailed,
                                format!("duplicate space id {id} in SkyLight census"),
                            ));
                        }
                        let active = id == raw.current_space;
                        found_current |= active;
                        let desktop_number = kind.is_desktop().then(|| {
                            desktop_ordinal += 1;
                            desktop_ordinal
                        });
                        Ok(SpaceRecord {
                            id,
                            uuid,
                            display_id: raw.display_id,
                            space_index,
                            desktop_number,
                            kind,
                            active,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if !found_current {
                    return Err(SpaceError::new(
                        SpaceErrorKind::QueryFailed,
                        format!(
                            "display {} active space {} missing from its Spaces list",
                            raw.display_id, raw.current_space
                        ),
                    ));
                }
                Ok(DisplaySpaces {
                    display_id: raw.display_id,
                    skylight_display: raw.skylight_display,
                    current_space: raw.current_space,
                    spaces,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { displays })
    }

    #[must_use]
    pub fn display(&self, display_id: CGDirectDisplayID) -> Option<&DisplaySpaces> {
        self.displays.iter().find(|d| d.display_id == display_id)
    }

    #[must_use]
    pub fn find_space(&self, space_id: SpaceId) -> Option<&SpaceRecord> {
        self.displays
            .iter()
            .flat_map(|d| &d.spaces)
            .find(|s| s.id == space_id)
    }

    #[must_use]
    pub fn display_for_space(&self, space_id: SpaceId) -> Option<&DisplaySpaces> {
        self.displays
            .iter()
            .find(|d| d.spaces.iter().any(|s| s.id == space_id))
    }
}

/// Resolves an opaque `SkyLight` `"Display Identifier"` (`"Main"` or UUID) to a
/// `CGDirectDisplayID` without letting `"Main"` shadow external displays.
///
/// Priority:
/// 1. Exact UUID match against `active_displays` `(display_id, uuid, current_space)`.
/// 2. Active-Space match (`current_space` matches `SLSManagedDisplayGetCurrentSpace` for that display).
/// 3. `"Main"` fallback to `main_display_id` (`CGMainDisplayID()`).
#[must_use]
pub fn resolve_skylight_display_id<S: AsRef<str>>(
    identifier: &str,
    current_space: WorkspaceId,
    main_display_id: CGDirectDisplayID,
    active_displays: &[(CGDirectDisplayID, S, WorkspaceId)],
) -> Option<CGDirectDisplayID> {
    if let Some((id, _, _)) = active_displays
        .iter()
        .find(|(_, uuid, _)| !uuid.as_ref().is_empty() && uuid.as_ref() == identifier)
    {
        return Some(*id);
    }
    if current_space != 0
        && let Some((id, _, _)) = active_displays
            .iter()
            .find(|(_, _, active_sid)| *active_sid == current_space)
    {
        return Some(*id);
    }
    (identifier == "Main" && main_display_id != 0).then_some(main_display_id)
}

/// Process-wide Space assignment target (§5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignMode {
    AllSpaces,
    Space(SpaceId),
    Clear,
}

/// Minimal `WindowServer` metadata read from `CGWindowListCopyWindowInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowServerMeta {
    pub layer: i32,
    pub pid: Pid,
}

impl WindowServerMeta {
    #[must_use]
    pub fn is_app_window(self) -> bool {
        APP_WINDOW_LAYERS.contains(&self.layer)
    }
}

/// Machine-readable error category for native Space operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceErrorKind {
    NoGuiSession,
    SessionLocked,
    Unsupported,
    QueryFailed,
    OperationFailed,
}

impl fmt::Display for SpaceErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::NoGuiSession => "no_gui_session",
            Self::SessionLocked => "session_locked",
            Self::Unsupported => "unsupported",
            Self::QueryFailed => "query_failed",
            Self::OperationFailed => "operation_failed",
        };
        f.write_str(label)
    }
}

/// Structured error returned by `DesktopBridge` queries and mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceError {
    pub kind: SpaceErrorKind,
    /// True once `-performWithWMBridgeDelegate` was invoked: callers must
    /// refresh census from OS reality rather than assuming state is unchanged.
    pub request_may_have_applied: bool,
    pub message: String,
}

impl SpaceError {
    #[must_use]
    pub fn new(kind: SpaceErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            request_may_have_applied: false,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn with_applied(mut self, applied: bool) -> Self {
        self.request_may_have_applied |= applied;
        self
    }
}

impl fmt::Display for SpaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} (may_have_applied={})",
            self.kind, self.message, self.request_may_have_applied
        )
    }
}

impl std::error::Error for SpaceError {}

/// Main-thread interface to `SkyLight` Space census queries and `WMBridge` mutations.
pub trait DesktopBridgeApi {
    fn read_census(&self) -> Result<SpaceCensus, SpaceError>;
    fn session_unlocked(&self) -> Result<(), SpaceError>;
    fn spaces_for_window(&self, window_id: WinID) -> Result<Vec<SpaceId>, SpaceError>;
    fn windows_in_space(
        &self,
        space_id: SpaceId,
        include_minimized: bool,
    ) -> Result<Vec<WinID>, SpaceError>;
    fn window_is_sticky(&self, window_id: WinID) -> Option<bool>;
    /// Batched `0x800` sticky-tag lookup across `windows` in a single `WindowServer` query.
    fn sticky_windows(&self, windows: &[WinID]) -> HashMap<WinID, bool> {
        windows
            .iter()
            .filter_map(|&wid| self.window_is_sticky(wid).map(|sticky| (wid, sticky)))
            .collect()
    }
    fn associated_windows(&self, window_id: WinID) -> Result<Vec<WinID>, SpaceError>;
    fn window_meta(&self, window_id: WinID) -> Option<WindowServerMeta>;
    fn windows_for_pid(&self, pid: Pid) -> Vec<WinID>;

    fn activate_space(
        &mut self,
        skylight_display: &str,
        target: SpaceId,
        hide: &[SpaceId],
    ) -> Result<(), SpaceError>;
    fn create_desktop(&mut self) -> Result<SpaceId, SpaceError>;
    fn destroy_space(&mut self, space_id: SpaceId) -> Result<(), SpaceError>;
    fn move_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
    ) -> Result<(), SpaceError>;
    fn add_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
        remove_others: bool,
    ) -> Result<(), SpaceError>;
    fn assign_process(&mut self, pid: Pid, mode: AssignMode) -> Result<(), SpaceError>;
    fn move_space_to_index(
        &mut self,
        space_id: SpaceId,
        skylight_display: &str,
        index: u32,
    ) -> Result<(), SpaceError>;
}

/// Bevy `NonSend` wrapper around [`DesktopBridgeApi`].
pub struct DesktopBridge(pub Box<dyn DesktopBridgeApi>);

impl Deref for DesktopBridge {
    type Target = dyn DesktopBridgeApi;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl DerefMut for DesktopBridge {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.0
    }
}

#[cfg(test)]
type DeferredFakeOp = Box<dyn FnOnce(&mut FakeDesktopState) + Send>;

/// Simulated `WindowServer` Space state for headless unit and `TestHarness` tests.
#[cfg(test)]
pub struct FakeDesktopState {
    pub locked: bool,
    pub displays: Vec<RawDisplaySpaces>,
    pub memberships: HashMap<WinID, Vec<SpaceId>>,
    pub sticky: HashSet<WinID>,
    pub associated: HashMap<WinID, Vec<WinID>>,
    pub window_meta: HashMap<WinID, WindowServerMeta>,
    pub next_space_id: u64,
    /// When `true`, async mutations queue into `deferred` until [`Self::flush_deferred`]
    /// runs, exercising multi-tick confirmation polling in ECS tests.
    pub defer_mutations: bool,
    deferred: Vec<DeferredFakeOp>,
}

#[cfg(test)]
impl Default for FakeDesktopState {
    fn default() -> Self {
        let first = SpaceId::new(1).expect("nonzero");
        Self {
            locked: false,
            displays: vec![RawDisplaySpaces {
                display_id: 1,
                skylight_display: "Main".to_string(),
                current_space: first,
                spaces: vec![(first, "uuid-1".to_string(), SpaceKind::Desktop)],
            }],
            memberships: HashMap::new(),
            sticky: HashSet::new(),
            associated: HashMap::new(),
            window_meta: HashMap::new(),
            next_space_id: 100,
            defer_mutations: false,
            deferred: Vec::new(),
        }
    }
}

#[cfg(test)]
impl FakeDesktopState {
    pub fn flush_deferred(&mut self) {
        let ops = std::mem::take(&mut self.deferred);
        for op in ops {
            op(self);
        }
    }

    fn run_or_defer(&mut self, op: impl FnOnce(&mut Self) + Send + 'static) {
        if self.defer_mutations {
            self.deferred.push(Box::new(op));
        } else {
            op(self);
        }
    }

    pub fn register_window(&mut self, wid: WinID, pid: Pid, space_id: SpaceId) {
        self.window_meta
            .insert(wid, WindowServerMeta { layer: 0, pid });
        self.memberships.insert(wid, vec![space_id]);
        self.associated.entry(wid).or_insert_with(|| vec![wid]);
    }
}

/// Cloneable handle to a [`FakeDesktopState`] implementing [`DesktopBridgeApi`].
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeDesktopBridge {
    state: Arc<Mutex<FakeDesktopState>>,
}

#[cfg(test)]
impl FakeDesktopBridge {
    #[must_use]
    pub fn state(&self) -> Arc<Mutex<FakeDesktopState>> {
        Arc::clone(&self.state)
    }
}

#[cfg(test)]
impl DesktopBridgeApi for FakeDesktopBridge {
    fn read_census(&self) -> Result<SpaceCensus, SpaceError> {
        let guard = self.state.lock().expect("fake state mutex");
        SpaceCensus::from_raw(guard.displays.clone())
    }

    fn session_unlocked(&self) -> Result<(), SpaceError> {
        let guard = self.state.lock().expect("fake state mutex");
        if guard.locked {
            Err(SpaceError::new(
                SpaceErrorKind::SessionLocked,
                "simulated screen is locked",
            ))
        } else {
            Ok(())
        }
    }

    fn spaces_for_window(&self, window_id: WinID) -> Result<Vec<SpaceId>, SpaceError> {
        let guard = self.state.lock().expect("fake state mutex");
        Ok(guard
            .memberships
            .get(&window_id)
            .cloned()
            .unwrap_or_default())
    }

    fn windows_in_space(
        &self,
        space_id: SpaceId,
        _include_minimized: bool,
    ) -> Result<Vec<WinID>, SpaceError> {
        let guard = self.state.lock().expect("fake state mutex");
        Ok(guard
            .memberships
            .iter()
            .filter(|(wid, spaces)| {
                spaces.contains(&space_id)
                    && guard
                        .window_meta
                        .get(wid)
                        .is_none_or(|meta| meta.is_app_window())
            })
            .map(|(&wid, _)| wid)
            .collect())
    }

    fn window_is_sticky(&self, window_id: WinID) -> Option<bool> {
        let guard = self.state.lock().expect("fake state mutex");
        guard
            .memberships
            .contains_key(&window_id)
            .then(|| guard.sticky.contains(&window_id))
    }

    fn associated_windows(&self, window_id: WinID) -> Result<Vec<WinID>, SpaceError> {
        let guard = self.state.lock().expect("fake state mutex");
        Ok(guard
            .associated
            .get(&window_id)
            .cloned()
            .unwrap_or_else(|| vec![window_id]))
    }

    fn window_meta(&self, window_id: WinID) -> Option<WindowServerMeta> {
        let guard = self.state.lock().expect("fake state mutex");
        guard.window_meta.get(&window_id).copied()
    }

    fn windows_for_pid(&self, pid: Pid) -> Vec<WinID> {
        let guard = self.state.lock().expect("fake state mutex");
        guard
            .window_meta
            .iter()
            .filter(|(wid, meta)| {
                meta.pid == pid
                    && meta.is_app_window()
                    && guard.memberships.get(wid).is_some_and(|s| !s.is_empty())
            })
            .map(|(&wid, _)| wid)
            .collect()
    }

    fn activate_space(
        &mut self,
        skylight_display: &str,
        target: SpaceId,
        _hide: &[SpaceId],
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let disp = skylight_display.to_string();
        let mut guard = self.state.lock().expect("fake state mutex");
        guard.run_or_defer(move |state| {
            if let Some(display) = state.displays.iter_mut().find(|d| {
                d.skylight_display == disp && d.spaces.iter().any(|(id, _, _)| *id == target)
            }) {
                display.current_space = target;
            }
        });
        Ok(())
    }

    fn create_desktop(&mut self) -> Result<SpaceId, SpaceError> {
        self.session_unlocked()?;
        let mut guard = self.state.lock().expect("fake state mutex");
        let raw_id = guard.next_space_id;
        guard.next_space_id += 1;
        let new_id = SpaceId::new(raw_id).expect("nonzero");
        guard.run_or_defer(move |state| {
            if let Some(display) = state.displays.first_mut() {
                display
                    .spaces
                    .push((new_id, format!("uuid-{raw_id}"), SpaceKind::Desktop));
            }
        });
        Ok(new_id)
    }

    fn destroy_space(&mut self, space_id: SpaceId) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let mut guard = self.state.lock().expect("fake state mutex");
        guard.run_or_defer(move |state| {
            let Some(display) = state
                .displays
                .iter_mut()
                .find(|d| d.spaces.iter().any(|(id, _, _)| *id == space_id))
            else {
                return;
            };
            if display.current_space == space_id {
                return;
            }
            let active = display.current_space;
            display.spaces.retain(|(id, _, _)| *id != space_id);
            for spaces in state.memberships.values_mut() {
                if let Some(pos) = spaces.iter().position(|&s| s == space_id) {
                    spaces.remove(pos);
                    if spaces.is_empty() {
                        spaces.push(active);
                    }
                }
            }
        });
        Ok(())
    }

    fn move_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let win_vec = windows.to_vec();
        let mut guard = self.state.lock().expect("fake state mutex");
        guard.run_or_defer(move |state| {
            for wid in win_vec {
                if !state.sticky.contains(&wid) {
                    state.memberships.insert(wid, vec![space_id]);
                }
            }
        });
        Ok(())
    }

    fn add_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
        remove_others: bool,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let win_vec = windows.to_vec();
        let mut guard = self.state.lock().expect("fake state mutex");
        guard.run_or_defer(move |state| {
            for wid in win_vec {
                if state.sticky.contains(&wid) {
                    continue;
                }
                let entry = state.memberships.entry(wid).or_default();
                if remove_others {
                    entry.clear();
                    entry.push(space_id);
                } else if !entry.contains(&space_id) {
                    entry.push(space_id);
                }
            }
        });
        Ok(())
    }

    fn assign_process(&mut self, pid: Pid, mode: AssignMode) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let mut guard = self.state.lock().expect("fake state mutex");
        guard.run_or_defer(move |state| {
            let pid_windows: Vec<WinID> = state
                .window_meta
                .iter()
                .filter(|(_, m)| m.pid == pid && m.is_app_window())
                .map(|(&wid, _)| wid)
                .collect();
            for wid in pid_windows {
                let disp_idx = state
                    .memberships
                    .get(&wid)
                    .and_then(|s| s.first())
                    .and_then(|first| {
                        state
                            .displays
                            .iter()
                            .position(|d| d.spaces.iter().any(|(id, _, _)| id == first))
                    })
                    .unwrap_or(0);
                let Some(disp) = state.displays.get(disp_idx) else {
                    continue;
                };
                match mode {
                    AssignMode::AllSpaces => {
                        let desktops: Vec<SpaceId> = disp
                            .spaces
                            .iter()
                            .filter(|(_, _, k)| k.is_desktop())
                            .map(|(id, _, _)| *id)
                            .collect();
                        state.sticky.insert(wid);
                        state.memberships.insert(wid, desktops);
                    }
                    AssignMode::Space(target) => {
                        state.sticky.remove(&wid);
                        state.memberships.insert(wid, vec![target]);
                    }
                    AssignMode::Clear => {
                        if state.sticky.remove(&wid) {
                            state.memberships.insert(wid, vec![disp.current_space]);
                        }
                    }
                }
            }
        });
        Ok(())
    }

    fn move_space_to_index(
        &mut self,
        space_id: SpaceId,
        skylight_display: &str,
        index: u32,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let disp_name = skylight_display.to_string();
        let mut guard = self.state.lock().expect("fake state mutex");
        guard.run_or_defer(move |state| {
            let Some(display) = state
                .displays
                .iter_mut()
                .find(|d| d.skylight_display == disp_name)
            else {
                return;
            };
            let Some(from) = display.spaces.iter().position(|(id, _, _)| *id == space_id) else {
                return;
            };
            let entry = display.spaces.remove(from);
            let to = (index as usize).min(display.spaces.len());
            display.spaces.insert(to, entry);
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(raw: u64) -> SpaceId {
        SpaceId::new(raw).unwrap()
    }

    #[test]
    fn resolve_skylight_display_never_lets_main_shadow_external_uuid() {
        let displays = [
            (1u32, "BUILTIN-UUID-1111", 10u64),
            (2u32, "EXTERNAL-UUID-2222", 20u64),
        ];
        // "Main" maps to display 1 via current_space / main_display_id, never display 2.
        assert_eq!(
            resolve_skylight_display_id("Main", 10, 1, &displays),
            Some(1)
        );
        // External display's UUID maps strictly to display 2.
        assert_eq!(
            resolve_skylight_display_id("EXTERNAL-UUID-2222", 20, 1, &displays),
            Some(2)
        );
        // Unknown UUID with unknown space resolves to None rather than stealing "Main".
        assert_eq!(
            resolve_skylight_display_id("GHOST-UUID", 999, 1, &displays),
            None
        );
    }

    #[test]
    fn census_numbers_desktops_skipping_fullscreen_while_preserving_space_index() {
        let raw = vec![RawDisplaySpaces {
            display_id: 1,
            skylight_display: "Main".to_string(),
            current_space: sid(20),
            spaces: vec![
                (sid(10), "d1".into(), SpaceKind::Desktop),
                (
                    sid(15),
                    "fs".into(),
                    SpaceKind::Fullscreen {
                        owner_wid: Some(500),
                        single_tile: true,
                    },
                ),
                (sid(20), "d2".into(), SpaceKind::Desktop),
            ],
        }];
        let census = SpaceCensus::from_raw(raw).expect("valid census");
        let disp = census.display(1).unwrap();
        assert_eq!(disp.desktop_count(), 2);
        assert!(!disp.all_desktops());

        assert_eq!(disp.spaces[0].space_index, 0);
        assert_eq!(disp.spaces[0].desktop_number, Some(1));

        assert_eq!(disp.spaces[1].space_index, 1);
        assert_eq!(disp.spaces[1].desktop_number, None);

        assert_eq!(disp.spaces[2].space_index, 2);
        assert_eq!(disp.spaces[2].desktop_number, Some(2));
        assert!(disp.spaces[2].active);
        assert_eq!(disp.desktop_by_number(2).unwrap().id, sid(20));
    }

    #[test]
    fn census_rejects_empty_zero_and_duplicate_displays() {
        assert_eq!(
            SpaceCensus::from_raw(vec![]).unwrap_err().kind,
            SpaceErrorKind::QueryFailed
        );

        let zero_disp = vec![RawDisplaySpaces {
            display_id: 0,
            skylight_display: "Main".into(),
            current_space: sid(1),
            spaces: vec![(sid(1), String::new(), SpaceKind::Desktop)],
        }];
        assert_eq!(
            SpaceCensus::from_raw(zero_disp).unwrap_err().kind,
            SpaceErrorKind::QueryFailed
        );

        let dup_disp = vec![
            RawDisplaySpaces {
                display_id: 1,
                skylight_display: "Main".into(),
                current_space: sid(1),
                spaces: vec![(sid(1), String::new(), SpaceKind::Desktop)],
            },
            RawDisplaySpaces {
                display_id: 1,
                skylight_display: "UUID-1".into(),
                current_space: sid(2),
                spaces: vec![(sid(2), String::new(), SpaceKind::Desktop)],
            },
        ];
        assert_eq!(
            SpaceCensus::from_raw(dup_disp).unwrap_err().kind,
            SpaceErrorKind::QueryFailed
        );

        assert_eq!(resolve_skylight_display_id::<&str>("Main", 1, 0, &[]), None);
    }

    #[test]
    fn census_rejects_duplicate_space_ids_and_missing_current_space() {
        let dup = vec![
            RawDisplaySpaces {
                display_id: 1,
                skylight_display: "Main".into(),
                current_space: sid(1),
                spaces: vec![(sid(1), String::new(), SpaceKind::Desktop)],
            },
            RawDisplaySpaces {
                display_id: 2,
                skylight_display: "EXT".into(),
                current_space: sid(1),
                spaces: vec![(sid(1), String::new(), SpaceKind::Desktop)],
            },
        ];
        assert_eq!(
            SpaceCensus::from_raw(dup).unwrap_err().kind,
            SpaceErrorKind::QueryFailed
        );

        let missing_active = vec![RawDisplaySpaces {
            display_id: 1,
            skylight_display: "Main".into(),
            current_space: sid(99),
            spaces: vec![(sid(1), String::new(), SpaceKind::Desktop)],
        }];
        assert_eq!(
            SpaceCensus::from_raw(missing_active).unwrap_err().kind,
            SpaceErrorKind::QueryFailed
        );
    }

    #[test]
    fn fake_bridge_simulates_create_activate_move_destroy_migration_and_deferred_apply() {
        let mut bridge = FakeDesktopBridge::default();
        bridge
            .state()
            .lock()
            .unwrap()
            .register_window(42, 1000, sid(1));

        let created = bridge.create_desktop().expect("creates desktop 2");
        assert_eq!(
            bridge
                .read_census()
                .unwrap()
                .display(1)
                .unwrap()
                .desktop_count(),
            2
        );

        // Move window 42 to the newly created Desktop, then destroy that Desktop
        // while Desktop 1 is active: window 42 migrates back to active Desktop 1.
        bridge.move_windows_to_space(&[42], created).unwrap();
        assert_eq!(bridge.spaces_for_window(42).unwrap(), vec![created]);

        bridge.destroy_space(created).unwrap();
        assert_eq!(bridge.spaces_for_window(42).unwrap(), vec![sid(1)]);

        // Deferred mode holds mutations until `flush_deferred()`.
        let s2 = bridge.create_desktop().unwrap();
        bridge.state().lock().unwrap().defer_mutations = true;
        bridge.activate_space("Main", s2, &[sid(1)]).unwrap();
        assert_eq!(
            bridge
                .read_census()
                .unwrap()
                .display(1)
                .unwrap()
                .current_space,
            sid(1)
        );
        bridge.state().lock().unwrap().flush_deferred();
        assert_eq!(
            bridge
                .read_census()
                .unwrap()
                .display(1)
                .unwrap()
                .current_space,
            s2
        );
    }
}
