use log::{debug, error};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::{fs, thread};
use stdext::function_name;

use crate::config::parse_command;
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
        let output = params
            .into_iter()
            .flat_map(|param| [param.as_bytes(), &[0]].concat())
            .collect::<Vec<_>>();
        let size: u32 = output.len().try_into()?;
        debug!("{}: {:?} {output:?}", function_name!(), size.to_le_bytes());

        let mut stream = UnixStream::connect(CommandReader::SOCKET_PATH)?;
        stream.write_all(&size.to_le_bytes())?;
        stream.write_all(&output)?;
        Ok(())
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
                error!("{}: {err}", function_name!());
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
            let Ok(mut stream) =
                stream.inspect_err(|err| error!("{}: reading stream {err}", function_name!()))
            else {
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

            if let Ok(command) = parse_command(&argv_ref)
                .inspect_err(|err| error!("{}: parsing command: {err}", function_name!()))
            {
                _ = self
                    .events
                    .send(Event::Command { command })
                    .inspect_err(|err| {
                        error!("{}: sending command: {err}", function_name!());
                    });
            }
        }
        Ok(())
    }
}

fn full_read(stream: &mut UnixStream, expected: usize, buffer: &mut [u8]) -> bool {
    if let Ok(count) = stream.read(buffer).inspect_err(|err| {
        error!("{}: {err}", function_name!());
    }) && count == expected
    {
        true
    } else {
        error!("{}: short read, expected {expected}.", function_name!());
        false
    }
}
