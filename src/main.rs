use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

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

use events::EventSender;

use errors::Result;
use platform::service;
use reader::CommandReader;

use crate::ecs::setup_bevy_app;

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
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(
            fmt::layer()
                .with_level(true)
                .with_line_number(true)
                .with_file(true)
                .with_target(true)
                .with_thread_ids(false)
                .with_writer(std::io::stderr)
                .compact(),
        )
        .init();

    let service = || service::Service::try_new(service::ID);

    match Paneru::parse().subcmd.unwrap_or_default() {
        SubCmd::Launch => {
            let (sender, receiver) = EventSender::new();
            CommandReader::new(sender.clone()).start();
            setup_bevy_app(sender, receiver)?.run();
        }
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
