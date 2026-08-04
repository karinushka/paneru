use async_lock::Mutex as AsyncMutex;
use bevy::tasks::{IoTaskPool, TaskPool};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::{fs, thread};
use tracing::{debug, error};

use crate::config::parse_command;
use crate::ecs::state::StateQueryKind;
use crate::errors::Result;
use crate::events::{Event, EventSender, Reply, ScriptStateRequest};
use paneru_shared_types::script_state::ScriptStateWrite;
use paneru_shared_types::windowset::LayoutOp;

/// `CommandReader` is responsible for sending and receiving commands via a Unix socket.
/// It acts as an IPC mechanism for the `paneru` application, allowing external processes
/// or the CLI client to communicate with the running daemon.
pub struct CommandReader {
    events: EventSender,
}

impl CommandReader {
    /// The path to the Unix socket used for inter-process communication.
    const SOCKET_PATH: &str = "/tmp/paneru.socket";

    /// Sends a command and its arguments to the running `paneru` application via a Unix socket.
    /// The arguments are serialized and sent as a byte stream.
    ///
    /// # Arguments
    ///
    /// * `params` - An iterator over command-line arguments, where each `String` is a parameter.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the command is sent successfully, otherwise `Err(Error)` if an I/O error occurs or the connection fails.
    pub fn send_command(params: impl IntoIterator<Item = String>) -> Result<()> {
        let _stream = Self::send_socket_request(params)?;
        Ok(())
    }

    pub fn send_query(kind: StateQueryKind) -> Result<String> {
        let args = ["query", kind.token(), "--json"];
        let mut stream = Self::send_socket_request(args.into_iter().map(str::to_string))?;
        let mut output = String::new();
        stream.read_to_string(&mut output)?;
        Ok(output)
    }

    /// Sends one script-state request and reads the daemon's JSON answer. The
    /// same round-trip as [`send_query`], for the store rather than the world.
    ///
    /// [`send_query`]: CommandReader::send_query
    pub fn send_script_state(request: &[String]) -> Result<String> {
        let frame = std::iter::once("state".to_string()).chain(request.iter().cloned());
        let mut stream = Self::send_socket_request(frame)?;
        let mut output = String::new();
        stream.read_to_string(&mut output)?;
        Ok(output)
    }

    pub fn subscribe_json() -> Result<()> {
        let mut stream =
            Self::send_socket_request(["subscribe", "--json"].into_iter().map(str::to_string))?;
        std::io::copy(&mut stream, &mut std::io::stdout())?;
        Ok(())
    }

    fn send_socket_request(params: impl IntoIterator<Item = String>) -> Result<UnixStream> {
        let output = params
            .into_iter()
            .flat_map(|param| [param.as_bytes(), &[0]].concat())
            .collect::<Vec<_>>();
        let size: u32 = output.len().try_into()?;
        debug!("{:?} {output:?}", size.to_le_bytes());

        let mut stream = UnixStream::connect(CommandReader::SOCKET_PATH)?;
        stream.write_all(&size.to_le_bytes())?;
        stream.write_all(&output)?;
        Ok(stream)
    }

    /// Creates a new `CommandReader` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to dispatch received commands as `Event::Command`.
    ///
    /// # Returns
    ///
    /// A new `CommandReader`.
    pub fn new(events: EventSender) -> Self {
        CommandReader { events }
    }

    /// Starts the `CommandReader` in a new thread, listening for incoming commands on a Unix socket.
    /// Any errors encountered in the runner thread are logged.
    pub fn start(mut self) {
        thread::spawn(move || {
            if let Err(err) = self.runner() {
                error!("{err}");
            }
        });
    }

    /// Accepts connections and hands each one to a task of its own.
    ///
    /// This thread does nothing but accept. Everything a connection needs —
    /// reading its frame, waiting for the world to answer, writing the reply —
    /// happens in [`serve`] on the IO task pool, so connections proceed
    /// independently.
    ///
    /// They used to be served inline, one after another, each one holding this
    /// loop for as long as its answer took (up to a two-second timeout). A
    /// single client waiting on a reply therefore stalled every other client
    /// behind it, and a client that connected without sending stalled them
    /// indefinitely.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the runner completes successfully (though it's typically a long-running loop),
    /// otherwise `Err(Error)` if a binding or I/O error occurs.
    fn runner(&mut self) -> Result<()> {
        _ = fs::remove_file(CommandReader::SOCKET_PATH);
        let listener = UnixListener::bind(CommandReader::SOCKET_PATH)?;

        for stream in listener.incoming() {
            let Ok(stream) = stream.inspect_err(|err| error!("reading stream {err}")) else {
                continue;
            };
            let events = self.events.clone();
            // `get_or_init`, not `get`: this thread is started before the Bevy
            // app is built, so a client connecting early would find no pool at
            // all and `get` would panic. Whichever of the two initialises it
            // first wins and the other reuses it; in practice that is bevy's
            // `TaskPoolPlugin`, with its configured thread counts.
            IoTaskPool::get_or_init(TaskPool::default)
                .spawn(async move { serve(stream, events).await })
                .detach();
        }
        Ok(())
    }
}

/// Serves one connection: read its frame, dispatch it, answer if it wants one.
///
/// The stream lives behind an async `Mutex` rather than being passed around as
/// `&mut`. Both halves matter: the lock is what lets the frame read and the
/// reply write be separate short critical sections instead of one borrow
/// spanning the whole connection, and it being an *async* lock is what lets a
/// guard sit across the `.await` on the world's answer at all — a
/// `std::sync::MutexGuard` is not `Send`, so a future holding one cannot be
/// spawned onto the pool.
///
/// A mutex and not an `RwLock`: every use of a socket is exclusive, so there is
/// no shared read for the second half of an `RwLock` to buy.
async fn serve(stream: UnixStream, events: EventSender) {
    let stream = Arc::new(AsyncMutex::new(stream));

    let Some(argv) = read_frame(&stream).await else {
        return;
    };
    let argv_ref = argv.iter().map(String::as_str).collect::<Vec<_>>();

    if let Some(kind) = parse_query_request(&argv_ref) {
        answer(&stream, &events, "state query", |respond_to| {
            Event::StateQuery { kind, respond_to }
        })
        .await;
        return;
    }

    if is_window_set_request(&argv_ref) {
        answer(&stream, &events, "window set query", |respond_to| {
            Event::WindowSetQuery { respond_to }
        })
        .await;
        return;
    }

    if let Some(ops) = parse_window_set_apply(&argv_ref) {
        // Fire-and-forget like every other command: the layout ops are applied
        // best-effort against the live world, and a client that wants the
        // result reads the next window set.
        _ = events
            .send(Event::Command {
                command: crate::commands::Command::Layout(ops),
            })
            .inspect_err(|err| error!("sending layout ops: {err}"));
        return;
    }

    if let Some(request) = parse_script_state_request(&argv_ref) {
        answer(&stream, &events, "script state request", |respond_to| {
            Event::ScriptState {
                request,
                respond_to,
            }
        })
        .await;
        return;
    }

    if is_subscribe_request(&argv_ref) {
        // The subscriber outlives this task, so it gets a descriptor of its own.
        // Behind the same async lock: broadcasts are written from a task on the
        // IO pool, never from the ECS system that produces them.
        let cloned = stream.lock().await.try_clone();
        match cloned {
            Ok(clone) => {
                if let Err(err) = clone.set_nonblocking(true) {
                    error!("configuring state subscriber as nonblocking: {err}");
                    return;
                }
                _ = events
                    .send(Event::StateSubscribe {
                        stream: Arc::new(AsyncMutex::new(clone)),
                    })
                    .inspect_err(|err| error!("registering state subscriber: {err}"));
            }
            Err(err) => error!("cloning subscriber stream: {err}"),
        }
        return;
    }

    if let Ok(command) =
        parse_command(&argv_ref).inspect_err(|err| error!("parsing command: {err}"))
    {
        _ = events
            .send(Event::Command { command })
            .inspect_err(|err| error!("sending command: {err}"));
    }
}

/// Reads one `<u32 le len><arg\0 arg\0 …>` frame, holding the lock only for the
/// two reads.
async fn read_frame(stream: &Arc<AsyncMutex<UnixStream>>) -> Option<Vec<String>> {
    let mut stream = stream.lock().await;

    let mut header = [0u8; 4];
    if !full_read(&mut stream, header.len(), &mut header) {
        return None;
    }
    let mut buffer = vec![0u8; u32::from_le_bytes(header) as usize];
    if !full_read(&mut stream, buffer.len(), &mut buffer) {
        return None;
    }

    Some(
        buffer
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).to_string())
            .collect(),
    )
}

/// Sends one request-carrying event to the world and writes the answer back down
/// the socket, newline-terminated.
///
/// Every frame that expects a reply — a state query, the window set, a
/// script-state read or write — has this shape: build the event around a fresh
/// reply channel, wait for the main thread to answer it, forward what came back.
/// `what` only names the request in the log.
///
/// The wait is an `.await`, not a `recv_timeout`, so a task waiting here costs
/// nothing but the task: it holds no pool thread, no lock on the stream, and
/// nothing another connection wants. There is no timeout because none is needed
/// — the reply sender travels inside the event, so if the world never answers,
/// the event is dropped, the channel closes, and the `recv` resolves.
async fn answer(
    stream: &Arc<AsyncMutex<UnixStream>>,
    events: &EventSender,
    what: &str,
    request: impl FnOnce(Reply) -> Event,
) {
    let (tx, rx) = async_channel::bounded(1);
    if events
        .send(request(tx))
        .inspect_err(|err| error!("sending {what}: {err}"))
        .is_err()
    {
        return;
    }

    let Ok(response) = rx
        .recv()
        .await
        .inspect_err(|err| error!("waiting for {what} response: {err}"))
    else {
        return;
    };

    let mut stream = stream.lock().await;
    _ = stream.write_all(response.as_bytes());
    _ = stream.write_all(b"\n");
}

fn parse_query_request(argv: &[&str]) -> Option<StateQueryKind> {
    // `--json` is the only supported (and default) output, so accept it or its
    // absence; the kind token is resolved by its single owner.
    match argv {
        ["query", token] | ["query", token, "--json"] => StateQueryKind::parse(token),
        _ => None,
    }
}

/// Reads a script-state frame: `state get <key>`, `state set <key> <json>`,
/// `state remove <key>`, or `state cas <key> <expected> <value>`.
///
/// `cas` is the compare-and-set a client's `mutate` is built on: it lands only
/// if the key still holds `expected`. Both of its values take a bare `-` for
/// "no value" — absent in `expected`, a removal in `value` — which cannot
/// collide with JSON, where a string is quoted.
///
/// A frame whose JSON does not parse is not a script-state request at all, so
/// it falls through to command parsing and is reported there, the same way an
/// unknown query kind is.
fn parse_script_state_request(argv: &[&str]) -> Option<ScriptStateRequest> {
    /// The `-` that stands for "there is no value here".
    const ABSENT: &str = "-";

    let owned = |value: &str| value.to_string();
    let maybe_json = |raw: &str| -> Option<Option<serde_json::Value>> {
        if raw == ABSENT {
            Some(None)
        } else {
            serde_json::from_str(raw).ok().map(Some)
        }
    };

    let write = match argv {
        ["state", "get", key] => return Some(ScriptStateRequest::Get { key: owned(key) }),
        ["state", "set", key, value] => {
            ScriptStateWrite::set(owned(key), serde_json::from_str(value).ok()?)
        }
        ["state", "remove", key] => ScriptStateWrite::remove(owned(key)),
        ["state", "cas", key, expected, value] => {
            ScriptStateWrite::compare_and_set(owned(key), maybe_json(expected)?, maybe_json(value)?)
        }
        _ => return None,
    };
    Some(ScriptStateRequest::Write(write))
}

fn is_subscribe_request(argv: &[&str]) -> bool {
    matches!(argv, ["subscribe", "--json"] | ["subscribe"])
}

/// Reads a `windowset` frame: the layout tree a `paneru.windows` handler is
/// given, for a client that wants the same one.
fn is_window_set_request(argv: &[&str]) -> bool {
    matches!(argv, ["windowset"] | ["windowset", "--json"])
}

/// Reads a `windowset apply <json>` frame: the operations a client's transform
/// recorded, to be replayed against the live world.
///
/// A frame whose JSON does not decode is not a layout request at all, so it
/// falls through to command parsing and is reported there, the same way an
/// unknown query kind is.
fn parse_window_set_apply(argv: &[&str]) -> Option<Vec<LayoutOp>> {
    let ["windowset", "apply", ops] = argv else {
        return None;
    };
    serde_json::from_str(ops).ok()
}

fn full_read(stream: &mut UnixStream, expected: usize, buffer: &mut [u8]) -> bool {
    if let Ok(count) = stream.read(buffer).inspect_err(|err| {
        error!("{err}");
    }) && count == expected
    {
        true
    } else {
        error!("short read, expected {expected}.");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paneru_shared_types::script_state::Expected;
    use serde_json::json;

    #[test]
    fn reads_the_window_set_frames() {
        assert!(is_window_set_request(&["windowset"]));
        assert!(is_window_set_request(&["windowset", "--json"]));
        assert!(!is_window_set_request(&["windowset", "apply", "[]"]));

        assert_eq!(
            parse_window_set_apply(&["windowset", "apply", "[]"]),
            Some(vec![])
        );
        assert_eq!(
            parse_window_set_apply(&["windowset", "apply", r#"[{"focus":7}]"#]),
            Some(vec![LayoutOp::Focus(7)])
        );
        assert_eq!(
            parse_window_set_apply(&[
                "windowset",
                "apply",
                r#"[{"move_to_workspace":{"window":7,"workspace":2,"follow":true}}]"#
            ]),
            Some(vec![LayoutOp::MoveToWorkspace {
                window: 7,
                workspace: 2,
                follow: true
            }])
        );

        // Undecodable ops are not a layout frame at all; they fall through to
        // command parsing, which reports them.
        assert_eq!(
            parse_window_set_apply(&["windowset", "apply", "not json"]),
            None
        );
        assert_eq!(parse_window_set_apply(&["windowset", "apply"]), None);
    }

    /// The layout tree survives the socket round trip: what the daemon
    /// serializes is what a client's `paneru.windows` transforms.
    #[test]
    fn window_set_survives_the_wire() {
        use paneru_shared_types::state::Frame;
        use paneru_shared_types::windowset::{
            ColumnSet, DisplaySet, WindowRec, WindowSet, WorkspaceSet,
        };

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
        let original = WindowSet::new(
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

        let encoded = serde_json::to_string(&original).expect("a window set serializes");
        let decoded: WindowSet = serde_json::from_str(&encoded).expect("and deserializes");

        assert_eq!(decoded, original);
        assert_eq!(decoded.focused(), Some(1));
        assert_eq!(decoded.east(1), Some(2));
        // Ops are deliberately not carried: a set off the wire is one nothing
        // has been asked of yet.
        assert!(decoded.ops().is_empty());
    }

    #[test]
    fn reads_the_script_state_frames() {
        assert_eq!(
            parse_script_state_request(&["state", "get", "pads.term"]),
            Some(ScriptStateRequest::Get {
                key: "pads.term".to_string()
            })
        );
        assert_eq!(
            parse_script_state_request(&["state", "set", "count", "7"]),
            Some(ScriptStateRequest::Write(ScriptStateWrite::set(
                "count".to_string(),
                json!(7)
            )))
        );
        assert_eq!(
            parse_script_state_request(&["state", "remove", "count"]),
            Some(ScriptStateRequest::Write(ScriptStateWrite::remove(
                "count".to_string()
            )))
        );
    }

    #[test]
    fn a_compare_and_set_frame_reads_both_of_its_values() {
        let Some(ScriptStateRequest::Write(write)) =
            parse_script_state_request(&["state", "cas", "count", "7", "8"])
        else {
            panic!("expected a write");
        };
        assert_eq!(write.expected, Expected::Exactly(Some(json!(7))));
        assert_eq!(write.value, Some(json!(8)));

        // `-` is how the wire says "no value": absent before, removed after.
        let Some(ScriptStateRequest::Write(write)) =
            parse_script_state_request(&["state", "cas", "count", "-", "-"])
        else {
            panic!("expected a write");
        };
        assert_eq!(write.expected, Expected::Exactly(None));
        assert_eq!(write.value, None);
    }

    #[test]
    fn a_frame_that_is_not_json_is_not_a_script_state_request() {
        // Falls through to command parsing, which reports it, rather than
        // being accepted here as a write of something unparseable.
        assert_eq!(
            parse_script_state_request(&["state", "set", "count", "not json"]),
            None
        );
        assert_eq!(parse_script_state_request(&["state", "wat", "count"]), None);
        assert_eq!(
            parse_script_state_request(&["window", "focus", "east"]),
            None
        );
    }
}
