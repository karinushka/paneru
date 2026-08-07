//! The script-owned key-value store: arbitrary named state a script can put
//! somewhere that outlives it, surviving both a Lua hot reload and a daemon
//! restart.
//!
//! Shared by the daemon and its clients: the embedded runtime writes it via
//! `paneru.state.*`, a client via the same spelling over the socket. Because
//! there are two writers, a write carries what it [`Expected`] to find — that
//! is what makes `paneru.state.mutate` a real read-modify-write.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::script_value::ScriptValue;

/// How large the serialised store is allowed to get. A script that writes on
/// every event has no natural stopping point, and the store is saved to disk;
/// this is the backstop that keeps a runaway loop from growing the state file
/// without bound.
pub const MAX_SERIALISED_BYTES: usize = 1024 * 1024;

/// How long a key may be. Long enough for any sane namespaced name, short
/// enough that a key built from unbounded input (a window title, say) is
/// rejected rather than stored.
pub const MAX_KEY_BYTES: usize = 512;

/// The store itself: names to values, in sorted order so the file it is saved
/// to is stable and diffable rather than reshuffling on every write.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScriptState(BTreeMap<String, ScriptValue>);

impl ScriptState {
    /// The value stored under `key`, if any.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&ScriptValue> {
        self.0.get(key)
    }

    /// Whether the store holds nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Applies `write`, if what it expected to find is what is there.
    ///
    /// # Errors
    ///
    /// If the key is unacceptable or the result would be too large. Neither
    /// leaves the store changed.
    pub fn apply(&mut self, write: &ScriptStateWrite) -> Result<WriteOutcome, String> {
        Self::check_key(&write.key)?;

        if let Expected::Exactly(expected) = &write.expected {
            let current = self.0.get(&write.key);
            if current != expected.as_ref() {
                return Ok(WriteOutcome::Conflict {
                    current: current.cloned(),
                });
            }
        }

        self.check_capacity(write)?;

        let changed = match &write.value {
            Some(value) => match self.0.get(&write.key) {
                Some(existing) if existing == value => false,
                _ => {
                    self.0.insert(write.key.clone(), value.clone());
                    true
                }
            },
            None => self.0.remove(&write.key).is_some(),
        };
        Ok(WriteOutcome::Applied { changed })
    }

    /// Whether `key` is one the store will accept.
    ///
    /// # Errors
    ///
    /// If the key is empty or longer than [`MAX_KEY_BYTES`].
    pub fn check_key(key: &str) -> Result<(), String> {
        if key.is_empty() {
            return Err("key must not be empty".to_string());
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(format!(
                "key is {} bytes, over the {MAX_KEY_BYTES} byte limit",
                key.len()
            ));
        }
        Ok(())
    }

    /// Whether applying `write` would push the store past
    /// [`MAX_SERIALISED_BYTES`]. Checked against a trial copy, so the store
    /// itself is never left over the limit.
    ///
    /// # Errors
    ///
    /// If the result would be too large, or could not be serialised at all.
    pub fn check_capacity(&self, write: &ScriptStateWrite) -> Result<(), String> {
        let Some(value) = &write.value else {
            // Removals only ever shrink it.
            return Ok(());
        };
        let mut trial = self.clone();
        trial.0.insert(write.key.clone(), value.clone());
        // Measured as JSON because that is what the store is saved as; the
        // wire encoding is denser, so this stays the conservative bound.
        let size = serde_json::to_vec(&trial)
            .map_err(|err| format!("value could not be stored: {err}"))?
            .len();
        if size > MAX_SERIALISED_BYTES {
            return Err(format!(
                "store would be {size} bytes, over the {MAX_SERIALISED_BYTES} byte limit"
            ));
        }
        Ok(())
    }
}

/// One write against the store: put `value` under `key`, or take the key out
/// when it is `None`.
///
/// Every write travels as one of these rather than as a replacement map,
/// because there are two writers — a script and a client — and a map would let
/// either clobber what the other just wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptStateWrite {
    pub key: String,
    /// `None` removes the key. This is what makes `set(key, nil)` and a
    /// `mutate` that returns nothing mean the same thing.
    pub value: Option<ScriptValue>,
    pub expected: Expected,
}

impl ScriptStateWrite {
    /// A write that lands whatever is already there.
    #[must_use]
    pub fn set(key: String, value: ScriptValue) -> Self {
        Self {
            key,
            value: Some(value),
            expected: Expected::Anything,
        }
    }

    /// A removal that lands whatever is already there.
    #[must_use]
    pub fn remove(key: String) -> Self {
        Self {
            key,
            value: None,
            expected: Expected::Anything,
        }
    }

    /// A write that lands only if the key still holds `expected` — where `None`
    /// means the key is still absent. The read-modify-write primitive
    /// `paneru.state.mutate` is built on.
    #[must_use]
    pub fn compare_and_set(
        key: String,
        expected: Option<ScriptValue>,
        value: Option<ScriptValue>,
    ) -> Self {
        Self {
            key,
            value,
            expected: Expected::Exactly(expected),
        }
    }
}

/// What a write requires to be true of the key before it lands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Expected {
    /// Land regardless — a plain `set` or `remove`.
    Anything,
    /// Land only if the key holds exactly this, `None` meaning it is absent.
    Exactly(Option<ScriptValue>),
}

/// What became of a write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteOutcome {
    Applied {
        /// Whether the store actually differs now. Writing the value that was
        /// already there is not a change, so nothing downstream re-reads or
        /// re-saves because of it.
        changed: bool,
    },
    /// The key no longer held what the write expected. Carries what it holds
    /// instead, so a caller can re-run its function against the current value
    /// and try again.
    Conflict { current: Option<ScriptValue> },
}

impl WriteOutcome {
    /// The documented `{"outcome": …}` JSON, for a client printing to a
    /// terminal.
    ///
    /// # Errors
    ///
    /// If serialization fails, which should not happen barring a bug in this
    /// type's `Serialize` impl.
    pub fn to_json(&self) -> serde_json::Result<serde_json::Value> {
        Ok(crate::json::flatten_tag(
            serde_json::to_value(self)?,
            "outcome",
        ))
    }
}
