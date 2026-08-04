// Bevy's `IntoSystem` and mlua's `create_function` both take their parameters
// by value: a system's `Res`/`Query`/`On`/`Single`/`Populated` arguments and a
// Lua callback's arguments are fetched and handed over by the framework, and a
// signature taking them by reference does not implement the trait at all. That
// makes this lint unactionable across every system and callback in the crate
// (212 sites at the time of writing), so it is turned off once here rather than
// re-suppressed, undocumented, on each one.
#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system and mlua callback signatures are by-value by contract"
)]

use std::sync::mpsc::{Receiver, TryRecvError};

use clap::{Parser, Subcommand};
use tracing::{error, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod accessibility_prompt;
mod commands;
mod config;
mod ecs;
mod errors;
mod events;
#[cfg(feature = "lua")]
mod lua;
mod manager;
mod menubar;
mod overlay;
mod platform;
mod reader;
mod util;

#[cfg(test)]
mod tests;

embed_plist::embed_info_plist!("../assets/Info.plist");

use events::{Event, EventSender};

use ecs::state::StateQueryKind;
use errors::Result;
use platform::service;
use reader::CommandReader;

use crate::ecs::setup_bevy_app;
use crate::manager::{check_ax_privilege, request_ax_privilege};
use crate::menubar::MenuBarManager;
use crate::platform::PlatformCallbacks;
use accessibility_prompt::{AccessibilitySetupAction, show_accessibility_setup};

#[cfg(feature = "lua")]
pub const VERSION_STRING: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("PANERU_LUA_VERSION"),
    ")"
);
#[cfg(not(feature = "lua"))]
pub const VERSION_STRING: &'static str = concat!(env!("CARGO_PKG_VERSION"));

/// `Paneru` is the main command-line interface structure for the window manager.
/// It defines the available subcommands for controlling the Paneru daemon.
#[derive(Clone, Debug, Default, Parser)]
#[command(
    version = VERSION_STRING,
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
    SendCmd {
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },

    /// Queries structured state from the running daemon.
    Query {
        #[clap(subcommand)]
        query: QueryCmd,
    },

    /// Subscribes to structured state events from the running daemon.
    Subscribe {
        #[arg(long)]
        json: bool,
    },

    /// Reads and writes the script state store, the same one `paneru.state`
    /// gives a Lua script.
    State {
        #[clap(subcommand)]
        state: StateCmd,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum StateCmd {
    /// Prints the value stored under a key, or `null`.
    Get { key: String },
    /// Stores a value, given as JSON.
    Set { key: String, value: String },
    /// Removes a key.
    Remove { key: String },
    /// Stores a value only if the key still holds what it was read as. Both
    /// values are JSON, or `-` for "no value": absent in `expected`, a removal
    /// in `value`.
    Cas {
        key: String,
        expected: String,
        value: String,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum QueryCmd {
    /// Prints the complete state document.
    State {
        #[arg(long)]
        json: bool,
    },
    /// Prints the virtual workspace list.
    VirtualWorkspaces {
        #[arg(long)]
        json: bool,
    },
    /// Prints the active focus/workspace state.
    Active {
        #[arg(long)]
        json: bool,
    },
    /// Prints the windows currently visible on screen, slivers excluded.
    OnScreen {
        #[arg(long)]
        json: bool,
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

    let subcmd = Paneru::parse().subcmd.unwrap_or_default();
    maybe_warn_deprecated_options_for_service(&subcmd);

    match subcmd {
        SubCmd::Launch => {
            let (sender, receiver) = EventSender::new();
            let sender_c = sender.clone();
            // bevy's `TerminalCtrlCHandlerPlugin` was not fast enough. maybe because of its use of `Relaxed` atomic variable?
            ctrlc::set_handler(move || {
                let _ = sender_c.send(events::Event::Exit); // just drop the err. we are exiting anyway.
            })
            .expect("setting Ctrl-C handler should succeed");
            CommandReader::new(sender.clone()).start();
            if !check_ax_privilege() && !wait_for_accessibility(sender.clone(), &receiver) {
                return Ok(());
            }
            match setup_bevy_app(sender, receiver) {
                Ok(mut app) => {
                    app.run();
                }
                Err(err) => {
                    error!(
                        "Error launching Paneru: {err}.\nStopping the service for now. You can restart it again with 'paneru restart'."
                    );
                    service()?.stop()?;
                }
            }
        }
        SubCmd::Install => service()?.install()?,
        SubCmd::Uninstall => service()?.uninstall()?,
        SubCmd::Reinstall => service()?.reinstall()?,
        SubCmd::Start => service()?.start()?,
        SubCmd::Stop => service()?.stop()?,
        SubCmd::Restart => service()?.restart()?,
        SubCmd::SendCmd { cmd } => CommandReader::send_command(cmd)?,
        SubCmd::Query { query } => {
            let output = CommandReader::send_query(query.kind())?;
            print!("{output}");
        }
        SubCmd::Subscribe { json: _ } => CommandReader::subscribe_json()?,
        SubCmd::State { state } => {
            let output = CommandReader::send_script_state(&state.argv())?;
            print!("{output}");
        }
    }
    Ok(())
}

fn wait_for_accessibility(sender: EventSender, receiver: &Receiver<Event>) -> bool {
    let mut platform_callbacks = PlatformCallbacks::new(sender.clone());
    let _menu_bar =
        MenuBarManager::new_accessibility_required(platform_callbacks.main_thread_marker, sender);

    if show_accessibility_setup(platform_callbacks.main_thread_marker)
        == AccessibilitySetupAction::Continue
    {
        request_ax_privilege();
    }

    warn!(
        "Accessibility access is required. Paneru will remain in the menu bar and start automatically once access is granted."
    );

    loop {
        platform_callbacks.pump_cocoa_event_loop(1.0);

        if check_ax_privilege() {
            return true;
        }

        match receiver.try_recv() {
            Ok(
                Event::Exit
                | Event::Command {
                    command: commands::Command::Quit,
                },
            )
            | Err(TryRecvError::Disconnected) => return false,
            Ok(event) => warn!(
                ?event,
                "ignoring event while waiting for Accessibility access"
            ),
            Err(TryRecvError::Empty) => {}
        }
    }
}

impl QueryCmd {
    fn kind(&self) -> StateQueryKind {
        match self {
            QueryCmd::State { json: _ } => StateQueryKind::State,
            QueryCmd::VirtualWorkspaces { json: _ } => StateQueryKind::VirtualWorkspaces,
            QueryCmd::Active { json: _ } => StateQueryKind::Active,
            QueryCmd::OnScreen { json: _ } => StateQueryKind::OnScreen,
        }
    }
}

impl StateCmd {
    /// The argv the daemon parses this back out of, minus the leading `state`.
    fn argv(&self) -> Vec<String> {
        let owned = |value: &str| value.to_string();
        match self {
            StateCmd::Get { key } => vec![owned("get"), key.clone()],
            StateCmd::Set { key, value } => vec![owned("set"), key.clone(), value.clone()],
            StateCmd::Remove { key } => vec![owned("remove"), key.clone()],
            StateCmd::Cas {
                key,
                expected,
                value,
            } => vec![owned("cas"), key.clone(), expected.clone(), value.clone()],
        }
    }
}

fn should_check_deprecated_options(subcmd: &SubCmd) -> bool {
    matches!(
        subcmd,
        SubCmd::Install | SubCmd::Uninstall | SubCmd::Start | SubCmd::Stop | SubCmd::Restart
    )
}

fn maybe_warn_deprecated_options_for_service(subcmd: &SubCmd) {
    if !should_check_deprecated_options(subcmd) {
        return;
    }

    // An init.lua disables the TOML entirely, so its contents — deprecated keys
    // included — are never read. Warning about them would be noise.
    #[cfg(feature = "lua")]
    if config::discover_lua_file().is_some() {
        return;
    }

    let Some(path) = config::discover_configuration_file() else {
        return;
    };

    match config::deprecated_options_in_file(&path) {
        Ok(keys) if !keys.is_empty() => {
            warn!(
                "detected deprecated [options] keys in `{}` while running a service command: {}. \
                 Please migrate to `[padding]`, `[swipe]`, and `[decorations.*]`.",
                path.display(),
                keys.join(", ")
            );
        }
        Ok(_) => {}
        Err(err) => {
            warn!(
                "could not inspect `{}` for deprecated options: {err}",
                path.display()
            );
        }
    }
}
