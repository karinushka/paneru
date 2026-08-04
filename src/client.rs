//! The `paneru …` side of the protocol: what a CLI invocation says to a running
//! daemon, and how it prints the answer.
//!
//! This is the only place JSON is produced. Everything between the two processes
//! is a typed value carried as postcard; the JSON exists because a terminal (and
//! `jq`, and a status bar's shell script) needs text, and it is rendered here at
//! the very last step rather than being what the daemon and its clients speak.
//!
//! Every function here is `async` and none of them drives an executor. A CLI
//! invocation is one round trip, so there is exactly one place worth blocking —
//! [`run`], at the entry point — and doing it here instead would mean each
//! function separately parking the same thread.

use futures_lite::StreamExt;
use paneru_mach_ipc::Sender;
use paneru_shared_types::state::{StateEvent, StateQueryKind};
use paneru_shared_types::wire::{
    QueryPayload, Request, Response, ScriptStateRequest, ScriptStateResponse, service_name,
};

use crate::errors::{Error, Result};

/// Connects to the running daemon.
///
/// # Errors
///
/// Reports plainly that Paneru is not running when nothing has claimed the
/// service name — the common case by far, and one that used to surface as a
/// bare `ENOENT` on a socket path.
fn connect() -> Result<Sender<Request>> {
    Sender::connect(&service_name()).map_err(|err| match err {
        paneru_mach_ipc::Error::NotRunning => Error::Generic("paneru is not running".to_string()),
        other => Error::from(other),
    })
}

/// Sends a command and does not wait: the daemon applies it against the live
/// world, and a caller that wants the result queries for it.
///
/// # Errors
///
/// If the daemon cannot be reached.
pub async fn send_command(argv: impl IntoIterator<Item = String>) -> Result<()> {
    let argv = argv.into_iter().collect::<Vec<_>>();
    let borrowed = argv.iter().map(String::as_str).collect::<Vec<_>>();
    let command = paneru_shared_types::argv::parse_command(&borrowed)?;

    connect()?.send(&Request::Command(command)).await?;
    Ok(())
}

/// Asks for part of the state document and prints it as JSON.
///
/// # Errors
///
/// If the daemon cannot be reached or answers with a failure.
pub async fn query(kind: StateQueryKind) -> Result<String> {
    let response: Response = connect()?.call(&Request::Query(kind)).await?;

    match response {
        Response::Query(payload) => render(&payload),
        other => Err(unexpected(&other)),
    }
}

/// Reads or writes the script-state store and prints the answer as JSON.
///
/// # Errors
///
/// If the daemon cannot be reached or answers with a failure.
pub async fn script_state(request: ScriptStateRequest) -> Result<String> {
    let response: Response = connect()?.call(&Request::ScriptState(request)).await?;

    let answer = match response {
        Response::ScriptState(answer) => answer,
        other => return Err(unexpected(&other)),
    };

    let value = match answer {
        // Rendered under `value` rather than bare, so a stored `null` and an
        // absent key stay distinguishable to a caller reading the output.
        ScriptStateResponse::Value(value) => serde_json::json!({
            "value": value.map(serde_json::Value::from),
        }),
        ScriptStateResponse::Write(outcome) => outcome
            .to_json()
            .map_err(|err| Error::Generic(err.to_string()))?,
    };
    Ok(value.to_string())
}

/// Streams state events to stdout, one JSON object per line, until interrupted.
///
/// # Errors
///
/// If the daemon cannot be reached. A daemon that exits ends the stream rather
/// than erroring: that is a normal end to a subscription, not a failure of one.
pub async fn subscribe() -> Result<()> {
    use std::io::Write;

    let events = connect()?
        .subscribe::<StateEvent>(&Request::Subscribe)
        .await?;
    let mut events = std::pin::pin!(events);

    while let Some(delivery) = events.next().await {
        let event = match delivery {
            Ok(delivery) => delivery.value,
            // The daemon is gone; the subscription is over, which is how a
            // `paneru subscribe` ends when the window manager stops.
            Err(paneru_mach_ipc::Error::PeerGone) => break,
            Err(err) => return Err(Error::from(err)),
        };

        let line = event
            .to_json()
            .map_err(|err| Error::Generic(err.to_string()))?
            .to_string();
        let mut stdout = std::io::stdout();
        // Flush per event: a subscriber is usually piped into something reading
        // line by line, and a buffered stream would stall it.
        if writeln!(stdout, "{line}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            // Whatever we were piped into has closed; nothing to report.
            break;
        }
    }
    Ok(())
}

/// Runs one client subcommand to completion.
///
/// The single place the CLI blocks. Everything above is `async` and composes;
/// this is the boundary where a program that must eventually return an exit code
/// meets that.
///
/// # Errors
///
/// Whatever the subcommand reports.
pub fn run(command: ClientCommand) -> Result<()> {
    futures_lite::future::block_on(async move {
        match command {
            ClientCommand::Send(argv) => send_command(argv).await,
            ClientCommand::Query(kind) => {
                println!("{}", query(kind).await?);
                Ok(())
            }
            ClientCommand::ScriptState(request) => {
                println!("{}", script_state(request).await?);
                Ok(())
            }
            ClientCommand::Subscribe => subscribe().await,
        }
    })
}

/// What a CLI invocation wants of the daemon.
///
/// Distinct from [`Request`]: this is what the *command line* asked for,
/// including how to print the answer, where a `Request` is only what crosses to
/// the daemon.
#[derive(Debug)]
pub enum ClientCommand {
    Send(Vec<String>),
    Query(StateQueryKind),
    ScriptState(ScriptStateRequest),
    Subscribe,
}

/// Renders a query answer as the JSON its kind has always produced.
fn render(payload: &QueryPayload) -> Result<String> {
    payload
        .to_json()
        .map(|value| value.to_string())
        .map_err(|err| Error::Generic(err.to_string()))
}

/// The daemon answered something this request never asks for, which means the
/// two ends disagree about the protocol.
fn unexpected(response: &Response) -> Error {
    match response {
        Response::Error(message) => Error::Generic(message.clone()),
        other => Error::Generic(format!("unexpected response: {other:?}")),
    }
}
