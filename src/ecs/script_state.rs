//! The script-owned key-value store, as the daemon holds it.
//!
//! [`ScriptState`] is the data; this is where it lives while Paneru runs and how
//! it gets to disk. Two writers reach it — the embedded Lua runtime through
//! `paneru.state.*`, a client through the socket — so the resource here is the
//! single authority, and the Lua worker (which caches a copy so a read costs it
//! no frame) checks [`ScriptStateStore::revision_handle`] to know when its copy
//! has gone stale.
//!
//! It is deliberately *not* part of [`PaneruState`]: that is layout, rebuilt
//! from the world on every save and removed from the world entirely once
//! session restore is done with it. Script state has neither property, so it
//! gets a file of its own beside it.
//!
//! [`PaneruState`]: super::state::PaneruState

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bevy::app::AppExit;
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::ResMut;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, error, warn};

use crate::events::{Event, ScriptStateRequest};
use paneru_shared_types::script_state::{ScriptState, ScriptStateWrite, WriteOutcome};

pub const SCRIPT_STATE_FILE_NAME: &str = "script-state.json";
const SUPPORTED_SCRIPT_STATE_VERSION: u32 = 1;

/// The on-disk shape. Versioned separately from the layout state file, since
/// the two have nothing to say to each other.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct SavedScriptState {
    version: u32,
    state: ScriptState,
}

/// The live store.
#[derive(Debug, Default, Resource)]
pub struct ScriptStateStore {
    state: ScriptState,
    /// Bumped on every applied mutation, whoever made it. Shared with the Lua
    /// worker, which compares it against the stamp its cached copy was
    /// hydrated at: equal means the cache is good, different means re-read.
    /// Carries no data, only a version, so it needs no synchronisation beyond
    /// the atomic itself.
    revision: Arc<AtomicU64>,
    dirty: bool,
}

impl ScriptStateStore {
    /// The store as saved by a previous run, or an empty one if there is no
    /// file, it cannot be read, or it was written by an incompatible version.
    /// Script state is a convenience, never load-bearing, so a bad file is
    /// worth a warning and nothing more.
    #[must_use]
    pub fn load() -> Self {
        let path = Self::default_file_path();
        let Some(saved) = Self::read_file(&path) else {
            return Self::default();
        };
        debug!("Loaded script state from {}", path.display());
        Self {
            state: saved,
            ..Self::default()
        }
    }

    fn read_file(path: &Path) -> Option<ScriptState> {
        let data = fs::read_to_string(path).ok()?;
        match serde_json::from_str::<SavedScriptState>(&data) {
            Ok(saved) if saved.version == SUPPORTED_SCRIPT_STATE_VERSION => Some(saved.state),
            Ok(saved) => {
                warn!(
                    "Ignoring script state at {}: version {}, expected {SUPPORTED_SCRIPT_STATE_VERSION}",
                    path.display(),
                    saved.version
                );
                None
            }
            Err(err) => {
                warn!(
                    "Ignoring unreadable script state at {}: {err}",
                    path.display()
                );
                None
            }
        }
    }

    /// A handle on the revision stamp, for the Lua worker to watch.
    #[must_use]
    pub fn revision_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.revision)
    }

    /// The whole store, for answering a read.
    #[must_use]
    pub fn snapshot(&self) -> ScriptState {
        self.state.clone()
    }

    /// Read-only access, for a caller that wants one key rather than a copy of
    /// everything.
    #[must_use]
    pub fn state(&self) -> &ScriptState {
        &self.state
    }

    /// Applies `write`, bumping the revision and marking the store for saving
    /// only if it actually changed something.
    ///
    /// This is the one place the store is written, whoever asked: a Lua
    /// handler's write arrives here, and so does a socket client's. That is
    /// what a compare-and-set write is measured against, and why
    /// `paneru.state.mutate` can promise that nothing slipped in between its
    /// read and its write.
    ///
    /// # Errors
    ///
    /// If the key is unacceptable or the write would push the store past its
    /// size limit. A write that merely lost a race is not an error — it comes
    /// back as [`WriteOutcome::Conflict`].
    pub fn apply(&mut self, write: &ScriptStateWrite) -> Result<WriteOutcome, String> {
        let outcome = self.state.apply(write)?;
        if matches!(outcome, WriteOutcome::Applied { changed: true }) {
            self.revision.fetch_add(1, Ordering::Release);
            self.dirty = true;
        }
        Ok(outcome)
    }

    /// Writes the store out if anything has changed since the last save. Same
    /// write-to-temp-then-rename as the layout state file, so a crash mid-save
    /// leaves the previous file intact rather than a truncated one.
    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let path = Self::default_file_path();
        match self.write_file(&path) {
            Ok(()) => {
                self.dirty = false;
                debug!("Script state saved to {}", path.display());
            }
            Err(err) => error!("Failed to save script state to {}: {err}", path.display()),
        }
    }

    fn write_file(&self, path: &Path) -> Result<(), std::io::Error> {
        let saved = SavedScriptState {
            version: SUPPORTED_SCRIPT_STATE_VERSION,
            state: self.state.clone(),
        };
        let json = serde_json::to_string_pretty(&saved).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, json)?;
        fs::rename(tmp_path, path)?;
        Ok(())
    }

    #[must_use]
    pub fn default_file_path() -> PathBuf {
        xdg::BaseDirectories::with_prefix("paneru")
            .get_state_file(SCRIPT_STATE_FILE_NAME)
            .expect("XDG state directory should be available")
    }
}

/// Answers the script-state requests a socket client made.
///
/// The client half of `paneru.state`: same store, same names, so a value a
/// script wrote is one a client can read and vice versa. Every reply is a
/// single JSON object, shaped like the query handler's so a client can tell an
/// answer from an error the same way.
pub fn script_state_handler(
    mut messages: MessageReader<Event>,
    store: Option<ResMut<ScriptStateStore>>,
) {
    let mut store = store;
    for message in messages.read() {
        let Event::ScriptState {
            request,
            respond_to,
        } = message
        else {
            continue;
        };
        let Some(store) = store.as_mut() else {
            let _ = respond_to.try_send(error_reply("the script state store is not available"));
            continue;
        };
        // `try_send`, never `send`: the reply channel holds one message and
        // exactly one is sent, so this cannot fill, and the main thread must
        // never wait on a socket client to collect its answer.
        let _ = respond_to.try_send(answer(store, request.clone()));
    }
}

fn answer(store: &mut ScriptStateStore, request: ScriptStateRequest) -> String {
    let reply = match request {
        ScriptStateRequest::Get { key } => Ok(json!({ "value": store.state().get(&key) })),
        ScriptStateRequest::Write(write) => store.apply(&write).map(|outcome| match outcome {
            WriteOutcome::Applied { changed } => json!({ "ok": true, "changed": changed }),
            // A client's `mutate` re-runs its function against this and tries
            // again, exactly as a script's does.
            WriteOutcome::Conflict { current } => json!({ "conflict": true, "current": current }),
        }),
    };
    match reply {
        Ok(value) => value.to_string(),
        Err(err) => error_reply(&err),
    }
}

fn error_reply(message: &str) -> String {
    json!({ "error": message }).to_string()
}

/// Saves the store on the same timer as the layout state, and costs nothing on
/// a run where no script ever wrote to it.
pub fn periodic_script_state_save(store: Option<ResMut<ScriptStateStore>>) {
    if let Some(mut store) = store {
        store.save_if_dirty();
    }
}

/// Saves the store on the way out, so the last write of a session is not the
/// one that gets lost.
pub fn script_state_cleanup_on_exit(
    mut exit_events: MessageReader<AppExit>,
    store: Option<ResMut<ScriptStateStore>>,
) {
    if exit_events.read().next().is_some()
        && let Some(mut store) = store
    {
        store.save_if_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(key: &str, value: serde_json::Value) -> ScriptStateWrite {
        ScriptStateWrite::set(key.to_string(), value)
    }

    fn applied(store: &mut ScriptStateStore, write: &ScriptStateWrite) -> bool {
        match store.apply(write).expect("accepted") {
            WriteOutcome::Applied { changed } => changed,
            WriteOutcome::Conflict { .. } => panic!("unexpected conflict"),
        }
    }

    #[test]
    fn apply_bumps_the_revision_only_on_a_real_change() {
        let mut store = ScriptStateStore::default();
        let revision = store.revision_handle();

        assert!(applied(&mut store, &set("a", json!(1))));
        assert_eq!(revision.load(Ordering::Acquire), 1);

        // The same value again changes nothing, so nothing downstream is told
        // to re-read.
        assert!(!applied(&mut store, &set("a", json!(1))));
        assert_eq!(revision.load(Ordering::Acquire), 1);

        assert!(applied(&mut store, &set("a", json!(2))));
        assert_eq!(revision.load(Ordering::Acquire), 2);
    }

    #[test]
    fn remove_takes_a_key_out() {
        let mut store = ScriptStateStore::default();
        applied(&mut store, &set("pad.a", json!(1)));
        applied(&mut store, &set("pad.b", json!(2)));

        let removed = ScriptStateWrite::remove("pad.a".to_string());
        assert!(applied(&mut store, &removed));
        assert!(store.state().get("pad.a").is_none());
        assert_eq!(store.state().get("pad.b"), Some(&json!(2)));

        // Removing what is not there changes nothing, so nothing re-reads.
        assert!(!applied(&mut store, &removed));
    }

    #[test]
    fn a_compare_and_set_lands_only_against_the_value_it_read() {
        let mut store = ScriptStateStore::default();
        applied(&mut store, &set("counter", json!(1)));

        // Someone else got there first: the write is refused, and comes back
        // with what the key holds now so the caller can retry against it.
        let stale = ScriptStateWrite::compare_and_set(
            "counter".to_string(),
            Some(json!(0)),
            Some(json!(1)),
        );
        assert_eq!(
            store.apply(&stale).expect("accepted"),
            WriteOutcome::Conflict {
                current: Some(json!(1))
            }
        );

        let fresh = ScriptStateWrite::compare_and_set(
            "counter".to_string(),
            Some(json!(1)),
            Some(json!(2)),
        );
        assert_eq!(
            store.apply(&fresh).expect("accepted"),
            WriteOutcome::Applied { changed: true }
        );
        assert_eq!(store.state().get("counter"), Some(&json!(2)));
    }

    #[test]
    fn a_compare_and_set_can_expect_an_absent_key() {
        let mut store = ScriptStateStore::default();

        let first = ScriptStateWrite::compare_and_set("fresh".to_string(), None, Some(json!("a")));
        assert_eq!(
            store.apply(&first).expect("accepted"),
            WriteOutcome::Applied { changed: true }
        );

        // The same write again now finds a value where it expected none.
        assert_eq!(
            store.apply(&first).expect("accepted"),
            WriteOutcome::Conflict {
                current: Some(json!("a"))
            }
        );
    }

    #[test]
    fn an_empty_key_is_refused() {
        let mut store = ScriptStateStore::default();
        assert!(store.apply(&set("", json!(1))).is_err());
    }

    #[test]
    fn an_oversized_value_is_refused_and_leaves_the_store_alone() {
        let mut store = ScriptStateStore::default();
        let huge = "x".repeat(paneru_shared_types::script_state::MAX_SERIALISED_BYTES + 1);
        assert!(store.apply(&set("big", json!(huge))).is_err());
        assert!(store.state().is_empty());
    }

    #[test]
    fn round_trips_through_a_file() {
        let path = unique_path("round-trip");

        let mut store = ScriptStateStore::default();
        applied(&mut store, &set("counter", json!(7)));
        store.write_file(&path).expect("written");

        let loaded = ScriptStateStore::read_file(&path).expect("read back");
        assert_eq!(loaded.get("counter"), Some(&json!(7)));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_file_from_another_version_is_ignored() {
        let path = unique_path("version-mismatch");
        let stale = json!({ "version": SUPPORTED_SCRIPT_STATE_VERSION + 1, "state": { "a": 1 } });
        fs::write(&path, stale.to_string()).expect("written");

        assert!(ScriptStateStore::read_file(&path).is_none());

        let _ = fs::remove_file(path);
    }

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "paneru-script-state-{name}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ))
    }
}
