//! Native macOS Space census domain model and [`DesktopBridge`] abstraction.

pub mod bridge;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};

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
    /// Ordinary Desktop (`type == 0`) Space IDs on this display, in Mission Control order.
    pub fn desktop_ids(&self) -> impl Iterator<Item = SpaceId> + '_ {
        self.spaces
            .iter()
            .filter(|s| s.kind.is_desktop())
            .map(|s| s.id)
    }

    /// Record for this display's active (`current_space`) Space.
    #[must_use]
    pub fn current_space_record(&self) -> Option<&SpaceRecord> {
        self.spaces.iter().find(|s| s.id == self.current_space)
    }

    /// Number of ordinary `SpaceKind::Desktop` Spaces on this display.
    #[must_use]
    pub fn desktop_count(&self) -> usize {
        self.desktop_ids().count()
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
                            active: id == raw.current_space,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if !spaces.iter().any(|s| s.active) {
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
    /// Probes whether `WindowManager` accepts `SLSBridged*Operation` (`WMBridge`)
    /// requests from this process by dispatching a synchronous read-only
    /// `SLSBridgedSpaceCopyValuesOperation` on `space_id`.
    ///
    /// Returns `Ok(true)` when `WindowManager` serviced the request, `Ok(false)`
    /// when it rejected the bridged operation (e.g. missing private entitlement
    /// on macOS 27.3+), or `Err(...)` when the session is locked/headless or the
    /// probe class/ABI is unavailable.
    fn probe_wm_bridge(&self, _space_id: SpaceId) -> Result<bool, SpaceError> {
        Ok(true)
    }
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
}
