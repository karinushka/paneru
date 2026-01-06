use chrono::Local;
use clap::{Parser, Subcommand};
use log::{debug, error};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::{fs, thread};
use stdext::function_name;

mod commands;
mod config;
mod ecs;
mod errors;
mod events;
mod manager;
mod platform;
mod service;
mod util;

#[cfg(test)]
mod tests;

embed_plist::embed_info_plist!("../assets/Info.plist");

use events::{Event, EventHandler, EventSender};
use platform::PlatformCallbacks;

use crate::config::parse_command;
use crate::errors::Result;

/// `CommandReader` is responsible for sending and receiving commands via a Unix socket.
/// It acts as an IPC mechanism for the `paneru` application, allowing external processes
/// or the CLI client to communicate with the running daemon.
struct CommandReader {
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
    fn send_command(params: impl IntoIterator<Item = String>) -> Result<()> {
        let output = params
            .into_iter()
            .flat_map(|param| [param.as_bytes(), &[0]].concat())
            .collect::<Vec<_>>();
        let size: u32 = output.len().try_into().unwrap();
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
    fn new(events: EventSender) -> Self {
        CommandReader { events }
    }

    /// Starts the `CommandReader` in a new thread, listening for incoming commands on a Unix socket.
    /// Any errors encountered in the runner thread are logged.
    fn start(mut self) {
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
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).to_string())
                    .collect::<Vec<_>>();
                let argv_ref = argv.iter().map(String::as_str).collect::<Vec<_>>();
                let command = parse_command(&argv_ref)?;
                self.events.send(Event::Command { command })?;
            }
        }
        Ok(())
    }
}

/// `Paneru` is the main command-line interface structure for the window manager.
/// It defines the available subcommands for controlling the Paneru daemon.
#[derive(Clone, Debug, Default, Parser)]
#[command(
    version = clap::crate_version!(),
    author = clap::crate_authors!(),
    about = clap::crate_description!(),
)]
pub struct Paneru {
    /// The subcommand to execute (e.g., `launch`, `install`, `send-cmd`).
    #[clap(subcommand)]
    subcmd: Option<SubCmd>,
}

/// `SubCmd` enumerates the available command-line subcommands for `paneru`.
/// These subcommands allow users to launch the daemon, install/uninstall it as a service,
/// start/stop/restart the service, or send commands to a running daemon.
#[derive(Clone, Debug, Default, Subcommand)]
pub enum SubCmd {
    /// Launches the `paneru` daemon directly in the console (default behavior).
    #[default]
    Launch,

    /// Installs the `paneru` daemon as a background service.
    Install,

    /// Uninstalls the `paneru` background service.
    Uninstall,

    /// Reinstalls the `paneru` background service.
    Reinstall,

    /// Starts the `paneru` background service.
    Start,

    /// Stops the `paneru` background service.
    Stop,

    /// Restarts the `paneru` background service.
    Restart,

    /// Sends a command via a Unix socket to the running `paneru` daemon.
    /// This subcommand is hidden from normal `--help` output.
    #[clap(hide = true)]
    SendCmd {
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },
}

/// The main entry point of the `paneru` application.
/// It sets up logging and dispatches commands accordingly.
///
/// # Returns
///
/// `Ok(())` if the application runs successfully, otherwise `Err(Error)`.
fn main() -> Result<()> {
    // Set up logging (default level is INFO)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
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

    let service = || service::Service::try_new(service::ID);

    match Paneru::parse().subcmd.unwrap_or_default() {
        SubCmd::Launch => launch()?,
        SubCmd::Install => service()?.install()?,
        SubCmd::Uninstall => service()?.uninstall()?,
        SubCmd::Reinstall => service()?.reinstall()?,
        SubCmd::Start => service()?.start()?,
        SubCmd::Stop => service()?.stop()?,
        SubCmd::Restart => service()?.restart()?,
        SubCmd::SendCmd { cmd } => CommandReader::send_command(cmd)?,
    }
    Ok(())
}

/// Launches the `paneru` window manager daemon.
///
/// This function performs initial checks (e.g., for separate spaces),
/// initializes event handling and platform callbacks, and then runs the main event loop.
/// It also sets up a `CommandReader` to listen for IPC commands.
///
/// # Returns
///
/// `Ok(())` if the daemon launches and runs successfully, otherwise `Err(Error)` if setup fails.
fn launch() -> Result<()> {
    let (sender, quit, handle) = EventHandler::run();

    CommandReader::new(sender.clone()).start();

    let mut platform_callbacks = PlatformCallbacks::new(sender)?;
    platform_callbacks.setup_handlers()?;

    platform_callbacks.run(&quit);

    handle.join().expect("Cannot join threads at the end.");
    Ok(())
}
