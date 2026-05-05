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
use crate::events::{Event, EventSender};

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
        let args = match kind {
            StateQueryKind::State => ["query", "state", "--json"],
            StateQueryKind::VirtualWorkspaces => ["query", "virtual-workspaces", "--json"],
            StateQueryKind::Active => ["query", "active", "--json"],
        };
        let mut stream = Self::send_socket_request(args.into_iter().map(str::to_string))?;
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

            if is_subscribe_request(&argv_ref) {
                match stream.try_clone() {
                    Ok(clone) => {
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
    match argv {
        ["query", "state", "--json"] | ["query", "state"] => Some(StateQueryKind::State),
        ["query", "virtual-workspaces", "--json"] | ["query", "virtual-workspaces"] => {
            Some(StateQueryKind::VirtualWorkspaces)
        }
        ["query", "active", "--json"] | ["query", "active"] => Some(StateQueryKind::Active),
        _ => None,
    }
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
