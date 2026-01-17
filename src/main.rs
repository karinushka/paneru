use chrono::Local;
use clap::{Parser, Subcommand};
use std::io::Write;

mod commands;
mod config;
mod ecs;
mod errors;
mod events;
mod manager;
mod platform;
mod reader;
mod util;

#[cfg(test)]
mod tests;

embed_plist::embed_info_plist!("../assets/Info.plist");

use events::EventHandler;
use platform::PlatformCallbacks;

use errors::Result;
use platform::service;
use reader::CommandReader;

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
