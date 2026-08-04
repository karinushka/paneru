//! What the daemon and its clients say to each other.
//!
//! These two enums are the whole protocol. Every request is a [`Request`] and
//! every answer a [`Response`], which is a change from what came before: the
//! socket transport flattened each request into argv strings on the client and
//! re-parsed them with a chain of `&[&str]` matchers on the daemon, so the shape
//! of a command existed twice and could disagree. Here the types below are the
//! only definition, and the encoding is generated from them.
//!
//! Values travel as postcard — compact, binary, and not self-describing, which
//! is why [`crate::script_value::ScriptValue`] exists in place of
//! `serde_json::Value`. JSON has not gone away; it is what `paneru query` prints
//! to a terminal. It is simply no longer what two processes speak to each other.

use serde::{Deserialize, Serialize};

/// The Mach service name the daemon publishes and clients look up.
///
/// Matches the launchd job's `Label` and its `MachServices` key, which is what
/// lets a service-started daemon check in with a port launchd already holds
/// rather than registering one of its own.
pub const SERVICE_NAME: &str = "com.github.karinushka.paneru";

/// The environment variable that overrides [`SERVICE_NAME`], so a development
/// build can run beside an installed one.
pub const SERVICE_ENV: &str = "PANERU_MACH_SERVICE";

/// The service name to use, honouring [`SERVICE_ENV`].
#[must_use]
pub fn service_name() -> String {
    std::env::var(SERVICE_ENV).unwrap_or_else(|_| SERVICE_NAME.to_string())
}

use crate::commands::Command;
pub use crate::script_state::WriteOutcome;

use crate::script_state::ScriptStateWrite;
use crate::script_value::ScriptValue;
use crate::state::{ActiveState, QueryState, StateQueryKind, VirtualWorkspaceState, WindowState};
use crate::windowset::{LayoutOp, WindowSet};

/// Something a client asks the daemon to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Run a command — the same one a hotkey binds to. Fire-and-forget: the
    /// daemon applies it best-effort against the live world and a client that
    /// wants the result queries for it.
    Command(Command),
    /// Read part of the state document.
    Query(StateQueryKind),
    /// Read the window set — the same layout tree a `paneru.windows` handler is
    /// given inside the daemon, so a client script transforms an identical tree.
    WindowSet,
    /// Replay a transform's recorded operations against the live world.
    /// Fire-and-forget, for the same reason [`Request::Command`] is.
    WindowSetApply(Vec<LayoutOp>),
    /// Read or write the script-state store.
    ScriptState(ScriptStateRequest),
    /// Ask for state events to be pushed as they happen.
    Subscribe,
}

/// What a client wants of the script-state store.
///
/// Lives here rather than in `script_state` because it is a *protocol* shape —
/// the store itself has no notion of a request, only of a write.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScriptStateRequest {
    Get { key: String },
    Write(ScriptStateWrite),
}

/// What the daemon says back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Query(QueryPayload),
    WindowSet(Box<WindowSet>),
    ScriptState(ScriptStateResponse),
    /// The request could not be answered. Carries the message a client should
    /// show — previously this was a `{"error": …}` object clients had to probe
    /// for by key, which meant a legitimate value with an `error` field was
    /// indistinguishable from a failure.
    Error(String),
}

/// The answer to a [`Request::Query`], one variant per [`StateQueryKind`].
///
/// This replaces a `serde_json::Value` that held whichever subtree the kind
/// selected. Typed, the caller knows what it got without inspecting it, and the
/// CLI renders whichever variant arrived — so its JSON output is unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum QueryPayload {
    State(Box<QueryState>),
    VirtualWorkspaces(Vec<VirtualWorkspaceState>),
    Active(Box<ActiveState>),
    OnScreen(Vec<WindowState>),
}

impl QueryPayload {
    /// Renders as JSON, for the CLI and for a Lua client that wants a table.
    ///
    /// The shape is exactly what each kind produced before, so `paneru query`
    /// output does not change.
    ///
    /// # Errors
    ///
    /// If serialization fails, which should not happen barring a bug in one of
    /// these types' `Serialize` impls.
    pub fn to_json(&self) -> serde_json::Result<serde_json::Value> {
        match self {
            Self::State(state) => serde_json::to_value(state),
            Self::VirtualWorkspaces(rows) => serde_json::to_value(rows),
            Self::Active(active) => serde_json::to_value(active),
            Self::OnScreen(windows) => serde_json::to_value(windows),
        }
    }
}

/// The answer to a [`ScriptStateRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScriptStateResponse {
    /// What the key holds, `None` when it holds nothing.
    Value(Option<ScriptValue>),
    /// What became of a write.
    Write(WriteOutcome),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Command, Direction, Operation};
    use crate::state::Frame;
    use std::sync::Arc;

    /// Encode, decode, compare — for every variant. This is the cross-crate
    /// check that did not exist when the codec was duplicated between the daemon
    /// and the Lua module.
    fn round_trip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let bytes = postcard::to_allocvec(value).expect("encodes");
        let decoded: T = postcard::from_bytes(&bytes).expect("decodes");
        assert_eq!(&decoded, value);
    }

    #[test]
    fn every_request_survives_the_wire() {
        round_trip(&Request::Command(Command::Window(Operation::Focus(
            Direction::East,
        ))));
        round_trip(&Request::Query(StateQueryKind::Active));
        round_trip(&Request::WindowSet);
        round_trip(&Request::WindowSetApply(vec![LayoutOp::Focus(7)]));
        round_trip(&Request::Subscribe);
        round_trip(&Request::ScriptState(ScriptStateRequest::Get {
            key: "pads.term".to_string(),
        }));
        round_trip(&Request::ScriptState(ScriptStateRequest::Write(
            ScriptStateWrite::set("count".to_string(), ScriptValue::Int(7)),
        )));
    }

    #[test]
    fn every_response_survives_the_wire() {
        round_trip(&Response::Query(QueryPayload::Active(Box::default())));
        round_trip(&Response::Query(QueryPayload::VirtualWorkspaces(Vec::new())));
        round_trip(&Response::Query(QueryPayload::OnScreen(Vec::new())));
        round_trip(&Response::ScriptState(ScriptStateResponse::Value(Some(
            ScriptValue::Str("hello".to_string()),
        ))));
        round_trip(&Response::ScriptState(ScriptStateResponse::Write(
            WriteOutcome::Applied { changed: true },
        )));
        round_trip(&Response::Error("no such window".to_string()));
    }

    /// The layout tree is the largest thing that crosses the wire, and the one
    /// a client actually transforms, so it gets its own round trip.
    #[test]
    fn the_window_set_survives_the_wire() {
        use crate::windowset::{ColumnSet, DisplaySet, WindowRec, WorkspaceSet};

        let window = |id| WindowRec {
            id,
            app_name: "Test App".to_string(),
            bundle_id: "com.example.test".to_string(),
            title: format!("Window {id}"),
            frame: Some(Frame {
                x: 0,
                y: 0,
                width: 400,
                height: 600,
            }),
            floating: false,
            managed: true,
            visible: true,
            focused: id == 1,
        };
        let set = WindowSet::new(
            vec![DisplaySet {
                id: 1,
                frame: Frame {
                    x: 0,
                    y: 0,
                    width: 1024,
                    height: 768,
                },
                active: true,
                workspaces: Arc::new(vec![WorkspaceSet {
                    number: 1,
                    native_id: 10,
                    active: true,
                    columns: Arc::new(vec![
                        ColumnSet::single(window(1), 0.5),
                        ColumnSet::single(window(2), 0.5),
                    ]),
                    floating: Arc::new(Vec::new()),
                }]),
            }],
            Some(1),
        );

        let bytes = postcard::to_allocvec(&Response::WindowSet(Box::new(set))).expect("encodes");
        let Response::WindowSet(decoded) = postcard::from_bytes(&bytes).expect("decodes") else {
            panic!("expected a window set");
        };

        assert_eq!(decoded.focused(), Some(1));
        assert_eq!(decoded.east(1), Some(2));
        // Ops are deliberately not carried: a set off the wire is one nothing
        // has been asked of yet.
        assert!(decoded.ops().is_empty());
    }

    /// The point of a binary wire format, stated as a test: a query request is a
    /// couple of bytes, where the argv encoding it replaces was a length prefix
    /// plus `"query\0active\0--json\0"`.
    #[test]
    fn a_request_is_small() {
        let bytes = postcard::to_allocvec(&Request::Query(StateQueryKind::Active)).expect("encodes");
        assert!(bytes.len() <= 4, "a query request took {} bytes", bytes.len());
    }
}
