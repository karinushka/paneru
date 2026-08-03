use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, thread};
use tracing::{debug, error};

use crate::config::parse_command;
use crate::ecs::state::StateQueryKind;
use crate::errors::Result;
use crate::events::{Event, EventSender, ScriptStateRequest};
use paneru_shared_types::script_state::ScriptStateWrite;

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

    /// The main runner function for the `CommandReader` thread. It binds to a Unix socket,
    /// listens for incoming connections, reads command size and data, and dispatches them as `Event::Command`.
    /// This loop continues indefinitely until an unrecoverable error occurs.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the runner completes successfully (though it's typically a long-running loop),
    /// otherwise `Err(Error)` if a binding or I/O error occurs.
    fn runner(&mut self) -> Result<()> {
        _ = fs::remove_file(CommandReader::SOCKET_PATH);
        let listener = UnixListener::bind(CommandReader::SOCKET_PATH)?;

        for stream in listener.incoming() {
            let Ok(mut stream) = stream.inspect_err(|err| error!("reading stream {err}")) else {
                continue;
            };
            let mut buffer = [0u8; 4];

            if !full_read(&mut stream, buffer.len(), &mut buffer) {
                continue;
            }
            let size = u32::from_le_bytes(buffer) as usize;
            let mut buffer = vec![0u8; size];

            if !full_read(&mut stream, buffer.len(), &mut buffer) {
                continue;
            }
            let argv = buffer
                .split(|c| *c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).to_string())
                .collect::<Vec<_>>();
            let argv_ref = argv.iter().map(String::as_str).collect::<Vec<_>>();

            if let Some(kind) = parse_query_request(&argv_ref) {
                let (tx, rx) = channel();
                _ = self
                    .events
                    .send(Event::StateQuery {
                        kind,
                        respond_to: tx,
                    })
                    .inspect_err(|err| {
                        error!("sending state query: {err}");
                    });

                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(response) => {
                        _ = stream.write_all(response.as_bytes());
                        _ = stream.write_all(b"\n");
                    }
                    Err(err) => error!("waiting for state query response: {err}"),
                }
                continue;
            }

            if let Some(request) = parse_script_state_request(&argv_ref) {
                let (tx, rx) = channel();
                _ = self
                    .events
                    .send(Event::ScriptState {
                        request,
                        respond_to: tx,
                    })
                    .inspect_err(|err| {
                        error!("sending script state request: {err}");
                    });

                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(response) => {
                        _ = stream.write_all(response.as_bytes());
                        _ = stream.write_all(b"\n");
                    }
                    Err(err) => error!("waiting for script state response: {err}"),
                }
                continue;
            }

            if is_subscribe_request(&argv_ref) {
                match stream.try_clone() {
                    Ok(clone) => {
                        if let Err(err) = clone.set_nonblocking(true) {
                            error!("configuring state subscriber as nonblocking: {err}");
                            continue;
                        }
                        _ = self
                            .events
                            .send(Event::StateSubscribe {
                                stream: Arc::new(Mutex::new(clone)),
                            })
                            .inspect_err(|err| {
                                error!("registering state subscriber: {err}");
                            });
                    }
                    Err(err) => error!("cloning subscriber stream: {err}"),
                }
                continue;
            }

            if let Ok(command) =
                parse_command(&argv_ref).inspect_err(|err| error!("parsing command: {err}"))
            {
                _ = self
                    .events
                    .send(Event::Command { command })
                    .inspect_err(|err| {
                        error!("sending command: {err}");
                    });
            }
        }
        Ok(())
    }
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
