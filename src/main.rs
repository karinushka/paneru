use log::{debug, error, LevelFilter};
use std::io::{Error, ErrorKind, Result};
use std::io::{Read, Write};
use std::fs::File;
use chrono::Local;
use std::os::unix::net::{UnixListener, UnixStream};
use std::{fs, thread};
use stdext::function_name;

mod app;
mod config;
mod events;
mod manager;
mod platform;
mod process;
mod service;
mod skylight;
mod util;
mod windows;

embed_plist::embed_info_plist!("../assets/Info.plist");

use events::{Event, EventHandler, EventSender};
use platform::PlatformCallbacks;
use skylight::{SLSGetSpaceManagementMode, SLSMainConnectionID};

struct CommandReader {
    events: EventSender,
}

impl CommandReader {
    const SOCKET_PATH: &str = "/tmp/paneru.socket";

    /// Sends a command and its arguments to the running `paneru` application via a Unix socket.
    ///
    /// # Arguments
    ///
    /// * `params` - An iterator over command-line arguments, where the first element is expected to be the program name.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the command is sent successfully, otherwise `Err(Error)`.
    fn send_command(mut params: std::env::Args) -> Result<()> {
        params.next();

        let output = params
            .flat_map(|param| [param.as_bytes(), &[0]].concat())
            .collect::<Vec<_>>();
        let size: u32 = output.len().try_into().unwrap();
        debug!("{}: {:?} {output:?}", function_name!(), size.to_le_bytes());

        let mut stream = UnixStream::connect(CommandReader::SOCKET_PATH)?;
        stream.write_all(&size.to_le_bytes())?;
        stream.write_all(&output)
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
    fn new(events: EventSender) -> Self {
        CommandReader { events }
    }

    /// Starts the `CommandReader` in a new thread, listening for incoming commands on a Unix socket.
    fn start(mut self) {
        thread::spawn(move || {
            if let Err(err) = self.runner() {
                error!("{}: {err}", function_name!());
            }
        });
    }

    /// The main runner function for the `CommandReader` thread. It binds to a Unix socket,
    /// listens for incoming connections, reads command size and data, and dispatches them as events.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the runner completes successfully, otherwise `Err(Error)`.
    fn runner(&mut self) -> Result<()> {
        _ = fs::remove_file(CommandReader::SOCKET_PATH);
        let listener = UnixListener::bind(CommandReader::SOCKET_PATH)?;
        for mut stream in listener.incoming().flatten() {
            let mut buffer = [0u8; 4];
            if 4 != stream.read(&mut buffer)? {
                error!("{}: Did not read size header.", function_name!());
                break;
            }
            let size = u32::from_le_bytes(buffer) as usize;
            let mut buffer = vec![0u8; size];
            loop {
                if size != stream.read(&mut buffer)? {
                    break;
                }
                let argv = buffer
                    .split(|c| *c == 0)
                    .flat_map(|s| (!s.is_empty()).then(|| String::from_utf8_lossy(s).to_string()))
                    .collect::<Vec<_>>();
                self.events.send(Event::Command { argv })?
            }
        }
        Ok(())
    }
}

/// Checks if the macOS "Displays have separate Spaces" option is enabled.
/// This is crucial for the window manager's functionality.
///
/// # Returns
///
/// `true` if separate spaces are enabled, `false` otherwise.
fn check_separate_spaces() -> bool {
    unsafe {
        let cid = SLSMainConnectionID();
        SLSGetSpaceManagementMode(cid) == 1
    }
}

/// The main entry point of the `paneru` application.
/// It sets up logging, handles command-line arguments for sending commands,
/// checks for separate spaces, initializes event handling and platform callbacks,
/// and runs the main event loop.
///
/// # Returns
///
/// `Ok(())` if the application runs successfully, otherwise `Err(Error)`.
fn main() -> Result<()> {
    // Set up logging (default level is INFO)
    // Log piping credit: https://github.com/rust-cli/env_logger/issues/125
    let target = Box::new(File::create("/tmp/paneru-log.txt").expect("Can't create file"));

    env_logger::Builder::new()
        .target(env_logger::Target::Pipe(target))
        .filter(None, LevelFilter::Info)
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {} {}:{}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                record.args()
            )
        })
        .init();

    if std::env::args().len() > 1 {
        return CommandReader::send_command(std::env::args());
    }

    if !check_separate_spaces() {
        error!(
            "{}: Option 'display has separate spaces' disabled.",
            function_name!()
        );
        return Err(Error::new(
            ErrorKind::Unsupported,
            "Option 'display has separate spaces' disabled.",
        ));
    }

    let event_handler = EventHandler::new()?;

    CommandReader::new(event_handler.sender()).start();

    let mut platform_callbacks = PlatformCallbacks::new(event_handler.sender())?;
    platform_callbacks.setup_handlers()?;

    let (quit, handle) = event_handler.start();

    platform_callbacks.run(quit);

    handle.join().expect("Can not joing threads at the end.");
    Ok(())
}
