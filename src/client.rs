//! The `paneru …` CLI side of the protocol: sends requests to the running
//! daemon and prints the answer as JSON. This is the only place JSON is
//! produced; the daemon and its clients otherwise speak typed postcard values.

use futures_lite::StreamExt;
use paneru_mach_ipc::{SendPort, Sender};
use paneru_shared_types::state::{StateEvent, StateQueryKind};
use paneru_shared_types::wire::{
    QueryPayload, Request, Response, ScriptStateRequest, ScriptStateResponse, service_name,
};

use crate::errors::{Error, Result};

/// Connects to the running daemon.
///
/// # Errors
///
/// Returns a plain "paneru is not running" error when no daemon is running.
fn connect() -> Result<Sender<Request>> {
    Sender::connect(&service_name()).map_err(|err| match err {
        paneru_mach_ipc::Error::NotRunning => Error::Generic("paneru is not running".to_string()),
        other => Error::from(other),
    })
}

/// Sends a command without waiting for a reply.
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
        // Nested under `value` so a stored `null` is distinguishable from an
        // absent key.
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
/// If the daemon cannot be reached.
pub async fn subscribe() -> Result<()> {
    use std::io::Write;

    let events = connect()?
        .subscribe::<StateEvent>(&Request::Subscribe)
        .await?;
    let mut events = std::pin::pin!(events);

    while let Some(delivery) = events.next().await {
        let event = match delivery {
            Ok(delivery) => delivery.value,
            // The daemon exiting ends the subscription normally.
            Err(paneru_mach_ipc::Error::PeerGone) => break,
            Err(err) => return Err(Error::from(err)),
        };

        let line = event
            .to_json()
            .map_err(|err| Error::Generic(err.to_string()))?
            .to_string();
        let mut stdout = std::io::stdout();
        // Flush per line: callers pipe this into readers expecting immediate lines.
        if writeln!(stdout, "{line}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

/// Runs one client subcommand to completion.
///
/// The single place the CLI blocks; everything above is `async`.
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

/// What a CLI invocation wants of the daemon, including how to print the
/// answer. Distinct from [`Request`], which is only what crosses to the daemon.
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

/// The daemon answered something this request never asks for.
fn unexpected(response: &Response) -> Error {
    match response {
        Response::Error(message) => Error::Generic(message.clone()),
        other => Error::Generic(format!("unexpected response: {other:?}")),
    }
}
