//! Where requests from other processes come in.
//!
//! Paneru publishes a Mach service under a well-known name; the CLI, the
//! loadable Lua module and anything else that wants to drive the window manager
//! connect to that name and send it a [`Request`]. Each one is turned into an
//! [`Event`] for the world, and the ones that expect an answer carry a reply
//! channel the answering system fills in.
//!
//! This replaced a Unix socket at `/tmp/paneru.socket` carrying argv strings and
//! JSON. The differences that matter:
//!
//! * **Nothing parses argv.** A request is a typed value, decoded once. The
//!   chain of `&[&str]` matchers this file used to be — and the frame encoder
//!   duplicated in the Lua crate — are gone along with the ways they could
//!   disagree.
//! * **There is no file.** The name is owned by the kernel, so there is no stale
//!   socket to unlink at startup, no world-writable path in `/tmp`, and a second
//!   daemon is refused rather than quietly taking the name over.
//! * **Replies ride a send-once port carried in the request**, so the receive
//!   loop never waits for the world and a reply cannot be sent twice.

use bevy::tasks::{IoTaskPool, TaskPool};
use futures_lite::StreamExt;
use paneru_mach_ipc::{Delivery, Receiver, Reply as MachReply};
use paneru_shared_types::wire::{Request, service_name};
use std::sync::Arc;
use std::thread;
use tracing::{error, warn};

use crate::errors::Result;
use crate::events::{Event, EventSender, Reply};

/// `CommandReader` owns the service port and feeds what arrives on it into the
/// world.
pub struct CommandReader {
    events: EventSender,
}

impl CommandReader {
    /// Creates a new `CommandReader`.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to dispatch received requests on.
    #[must_use]
    pub fn new(events: EventSender) -> Self {
        CommandReader { events }
    }

    /// Claims the service name and starts serving it on a thread of its own.
    ///
    /// # Errors
    ///
    /// Returns an error if another Paneru already owns the name. That is a
    /// refusal to start rather than something to override — the socket this
    /// replaced unlinked whatever it found, so a second daemon silently stole
    /// the first one's clients.
    pub fn start(self) -> Result<()> {
        let receiver = Receiver::<Request>::bind(&service_name())?;

        thread::spawn(move || {
            // The requests arrive as a stream; `block_on` here parks exactly one
            // thread for the life of the process, and every request it yields is
            // handed to the IO pool rather than served inline.
            futures_lite::future::block_on(async move {
                let mut requests = std::pin::pin!(receiver);
                while let Some(delivery) = requests.next().await {
                    match delivery {
                        Ok(delivery) => self.dispatch(delivery),
                        // Any process in the session can reach the service port,
                        // so a request that does not decode is a bad client, not
                        // a reason to stop serving.
                        Err(err) => warn!("reading request: {err}"),
                    }
                }
            });
        });

        Ok(())
    }

    /// Turns one request into an event, and arranges for its answer.
    fn dispatch(&self, delivery: Delivery<Request>) {
        let events = self.events.clone();
        let Delivery {
            value,
            reply,
            subscriber,
        } = delivery;

        match value {
            // Fire-and-forget: applied best-effort against the live world, and a
            // client that wants the result asks for it.
            Request::Command(command) => {
                send(&events, Event::Command { command });
            }
            Request::WindowSetApply(ops) => {
                send(
                    &events,
                    Event::Command {
                        command: crate::commands::Command::Layout(ops),
                    },
                );
            }

            Request::Query(kind) => {
                answer(events, reply, "state query", move |respond_to| {
                    Event::StateQuery { kind, respond_to }
                });
            }
            Request::WindowSet => {
                answer(events, reply, "window set query", |respond_to| {
                    Event::WindowSetQuery { respond_to }
                });
            }
            Request::ScriptState(request) => {
                answer(events, reply, "script state request", move |respond_to| {
                    Event::ScriptState {
                        request,
                        respond_to,
                    }
                });
            }

            Request::Subscribe => {
                if let Some(subscriber) = subscriber {
                    send(
                        &events,
                        Event::StateSubscribe {
                            subscriber: Arc::new(subscriber),
                        },
                    );
                } else {
                    // A subscribe that carried no channel has nowhere for events
                    // to go; the sender built the request by hand and got it
                    // wrong.
                    warn!("subscribe request carried no event channel");
                }
            }
        }
    }
}

/// Sends an event that expects no answer.
fn send(events: &EventSender, event: Event) {
    _ = events
        .send(event)
        .inspect_err(|err| error!("sending event: {err}"));
}

/// Sends a request-carrying event to the world and answers the client with
/// whatever comes back.
///
/// Every request that expects a reply has this shape: build the event around a
/// fresh reply channel, wait for the main thread to answer it, forward what came
/// back. `what` only names the request in the log.
///
/// The wait happens on the IO pool rather than here, so the receive loop is free
/// to take the next request immediately — a client waiting on a slow answer
/// blocks nothing but itself. It holds no thread while it waits, only a task.
///
/// There is deliberately no timeout, because none is needed: the reply sender
/// travels inside the event, so if the world never answers it, the event is
/// dropped, the channel closes, and the `recv` resolves.
fn answer(
    events: EventSender,
    reply: Option<MachReply>,
    what: &'static str,
    request: impl FnOnce(Reply) -> Event + Send + 'static,
) {
    let Some(reply) = reply else {
        // The client did not ask for an answer, so there is nothing to wait for
        // and no point building the request.
        warn!("{what} arrived without a reply channel");
        return;
    };

    let (tx, rx) = async_channel::bounded(1);
    if events
        .send(request(tx))
        .inspect_err(|err| error!("sending {what}: {err}"))
        .is_err()
    {
        return;
    }

    // `get_or_init`, not `get`: the reader starts before the Bevy app is built,
    // so an early client would otherwise find no pool at all and panic.
    IoTaskPool::get_or_init(TaskPool::default)
        .spawn(async move {
            match rx.recv().await {
                Ok(response) => {
                    if let Err(err) = reply.send(&response) {
                        // A client that stopped waiting is normal — an
                        // interrupted `paneru query` does exactly this.
                        warn!("answering {what}: {err}");
                    }
                }
                Err(err) => error!("waiting for {what} response: {err}"),
            }
        })
        .detach();
}
