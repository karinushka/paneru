//! Typed, async channels between unrelated processes, over Mach ports.
//!
//! One process binds a well-known service name and holds a [`Receiver<T>`]; any
//! number of unrelated processes — each `paneru` CLI invocation, the loadable
//! Lua module — connect a [`Sender<T>`] to that name and send it values. The
//! shape is deliberately the one `std`'s channels have, because that is what
//! callers already know:
//!
//! ```no_run
//! # use serde::{Serialize, Deserialize};
//! # #[derive(Serialize, Deserialize)] struct Request;
//! # #[derive(Serialize, Deserialize)] struct Response;
//! # fn main() -> Result<(), paneru_mach_ipc::Error> {
//! # futures_lite::future::block_on(async {
//! // The daemon.
//! let receiver = paneru_mach_ipc::Receiver::<Request>::bind("com.example.service")?;
//! let delivery = receiver.recv().await?;
//! if let Some(reply) = delivery.reply {
//!     reply.send(&Response)?;
//! }
//!
//! // A client, in some other process.
//! let sender = paneru_mach_ipc::Sender::<Request>::connect("com.example.service")?;
//! let response: Response = sender.call(&Request).await?;
//! # Ok(())
//! # })
//! # }
//! ```
//!
//! # What this buys over a Unix socket
//!
//! * **The name is kernel-owned.** There is no socket file to go stale, to be
//!   left behind by a crash, or to sit world-writable in `/tmp`. A second daemon
//!   is refused rather than silently taking the name over.
//! * **Replies ride a send-once right carried in the request.** The receive loop
//!   never holds a connection open waiting for an answer, and the [`Reply`] can
//!   be moved to whichever task eventually produces one. It cannot be used
//!   twice, because the kernel enforces that rather than a convention.
//! * **Death is observable.** Pushing to a [`Subscriber`] whose process has
//!   exited fails with [`Error::PeerGone`] instead of succeeding into a void, so
//!   dead subscribers are reaped for the right reason rather than inferred from
//!   a write error that a merely slow reader would also produce.
//!
//! # Async
//!
//! Nothing here parks a thread on `mach_msg`. [`Receiver`] is a [`Stream`]
//! implemented directly over the Mach API: `poll_next` attempts a non-blocking
//! receive and, when the port is empty, registers the task's waker against a
//! process-wide kqueue watching that port. See [`reactor`](mod@reactor) for why
//! `EVFILT_MACHPORT` is the only mechanism the platform offers and why the
//! registration is one-shot.
//!
//! # Values, not bytes
//!
//! Mach is message-oriented, and so is this: one value in, one value out, with
//! no framing for a caller to get wrong. Values are encoded with `postcard`,
//! which is why `T` is bounded by serde's traits.

pub mod bootstrap;
mod error;
mod msg;
mod reactor;
pub mod rights;

pub use error::{Error, Result};
pub use futures_lite::Stream;

use mach2::port::mach_port_t;
use msg::Dest;
use reactor::Interest;
use rights::{RecvRight, SendOnceRight, SendRight};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

/// The one place the try-then-register dance lives.
///
/// Attempts a receive and, when the port is empty, arms a one-shot wakeup and
/// yields. Trying *first* is what makes this correct: the kqueue registration
/// only reports messages arriving after it exists, so anything already queued
/// would otherwise never wake anyone.
fn poll_recv(
    port: mach_port_t,
    interest: &Interest,
    cx: &Context<'_>,
) -> Poll<Result<msg::Incoming>> {
    match msg::try_recv(port) {
        Err(Error::WouldBlock) => {}
        other => return Poll::Ready(other),
    }

    if let Err(err) = interest.arm(cx.waker()) {
        return Poll::Ready(Err(err));
    }

    // A message may have arrived between the failed receive and the
    // registration, producing an event nobody was listening for yet. Retrying
    // once is what keeps it from being stranded until another arrives behind it.
    match msg::try_recv(port) {
        Err(Error::WouldBlock) => Poll::Pending,
        other => Poll::Ready(other),
    }
}

/// Hands a value to the kernel, yielding to the executor rather than blocking if
/// the destination's queue is momentarily full.
///
/// A Mach port has no "writable" event — `EVFILT_MACHPORT` reports arrivals only
/// — so there is nothing to register a waker against, and cooperative yielding
/// is the honest way to wait without parking the thread. In practice this never
/// loops: a full queue means the peer has stopped draining, which for a request
/// means the daemon is wedged, and for an event means the subscriber is asleep
/// (where [`Subscriber::try_send`], which drops rather than waits, is used).
async fn send_async(
    dest: mach_port_t,
    payload: &[u8],
    extra_port: Option<mach_port_t>,
) -> Result<()> {
    loop {
        match msg::send(dest, Dest::CopySend, payload, None, extra_port, Some(0)) {
            Err(Error::WouldBlock) => futures_lite::future::yield_now().await,
            other => return other,
        }
    }
}

/// The receiving end of a service: owns the name and yields what is sent to it.
///
/// Only one process can hold this for a given name — that is what a receive
/// right means — so binding is also how a daemon claims singleton status.
#[derive(Debug)]
pub struct Receiver<T> {
    port: RecvRight,
    interest: Interest,
    _value: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Receiver<T> {
    /// Takes ownership of the service name.
    ///
    /// Tries launchd's `MachServices` handover first and falls back to
    /// registering the name directly, so the same call works whether the process
    /// was started by `launchctl` or from a shell.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AlreadyRunning`] if another process holds the name.
    pub fn bind(service: &str) -> Result<Self> {
        let port = match bootstrap::check_in(service) {
            Ok(port) => port,
            // Not a launchd job, so publish the name ourselves.
            Err(Error::NotRunning) => bootstrap::register(service)?,
            Err(err) => return Err(err),
        };
        Ok(Self::from_port(port))
    }

    fn from_port(port: RecvRight) -> Self {
        let interest = Interest::new(port.as_raw());
        Self {
            port,
            interest,
            _value: PhantomData,
        }
    }

    /// Waits for the next value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] for a value that does not match `T`. The
    /// service port is reachable by any process in the session, so that is an
    /// expected input rather than a fatal condition — log it and call `recv`
    /// again.
    pub async fn recv(&self) -> Result<Delivery<T>> {
        futures_lite::future::poll_fn(|cx| self.poll_delivery(cx)).await
    }

    fn poll_delivery(&self, cx: &Context<'_>) -> Poll<Result<Delivery<T>>> {
        poll_recv(self.port.as_raw(), &self.interest, cx).map(|result| {
            result.and_then(|incoming| {
                Ok(Delivery {
                    value: postcard::from_bytes(&incoming.payload).map_err(|_| Error::Decode)?,
                    reply: incoming.reply.map(|right| Reply { right }),
                    subscriber: incoming
                        .ports
                        .into_iter()
                        .next()
                        .map(|right| Subscriber { right }),
                })
            })
        })
    }
}

/// The values sent to a service, as they arrive.
///
/// The stream does not end on its own: a [`Error::Decode`] item is one bad
/// client, not the end of the service, so callers should log such an item and
/// keep polling rather than break.
impl<T: DeserializeOwned> Stream for Receiver<T> {
    type Item = Result<Delivery<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_delivery(cx).map(Some)
    }
}

/// One value off the wire, with whatever the sender attached to it.
#[derive(Debug)]
pub struct Delivery<T> {
    /// The decoded value.
    pub value: T,
    /// Where the answer goes, when the sender used [`Sender::call`]. A
    /// [`Sender::send`] leaves this `None`.
    pub reply: Option<Reply>,
    /// The channel a [`Sender::subscribe`] asked for events on.
    pub subscriber: Option<Subscriber>,
}

/// A one-shot channel back to whoever sent a value.
///
/// Consuming `self` on send is not a stylistic choice: the underlying right is
/// spent by the kernel when the message goes out, so a second use could not work
/// even if the type allowed it. It is `Send`, which is the property a daemon
/// depends on — the answer is usually produced somewhere else entirely from
/// where the request was read.
#[derive(Debug)]
pub struct Reply {
    right: SendOnceRight,
}

impl Reply {
    /// Answers the sender.
    ///
    /// Not `async`, because it cannot wait: a send-once right's queue has never
    /// been used and never will be again, so there is no full-queue case for a
    /// suspension point to handle.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the sender stopped waiting and exited,
    /// which is normal — an interrupted `paneru query` does exactly this.
    pub fn send<R: Serialize>(self, value: &R) -> Result<()> {
        let payload = postcard::to_allocvec(value).map_err(|_| Error::Encode)?;
        msg::reply(self.right, &payload)
    }
}

/// A lasting channel to a process that asked for events.
///
/// Unlike [`Reply`] this survives the value that delivered it: the daemon keeps
/// it and pushes to it for as long as the subscriber lives.
#[derive(Debug)]
pub struct Subscriber {
    right: SendRight,
}

impl Subscriber {
    /// Pushes one event, without ever waiting.
    ///
    /// Deliberately not `async`: a subscriber that has stopped reading must not
    /// be able to stall the window manager, so a full queue drops the event
    /// rather than applying backpressure.
    ///
    /// # Errors
    ///
    /// [`Error::PeerGone`] means the subscriber's process is gone and it should
    /// be dropped. [`Error::WouldBlock`] means it is alive but not keeping up;
    /// the event is lost but the subscriber should be kept.
    pub fn try_send<E: Serialize>(&self, value: &E) -> Result<()> {
        let payload = postcard::to_allocvec(value).map_err(|_| Error::Encode)?;
        msg::send(
            self.right.as_raw(),
            Dest::CopySend,
            &payload,
            None,
            None,
            Some(0),
        )
    }
}

/// The sending end of a service, in some other process.
///
/// Cheap to hold and reusable across any number of values, so a long-lived
/// client looks the name up once.
#[derive(Debug)]
pub struct Sender<T> {
    service: SendRight,
    _value: PhantomData<fn(T)>,
}

impl<T: Serialize> Sender<T> {
    /// Finds the service.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotRunning`] when nothing has bound the name — the
    /// ordinary "paneru isn't running" that clients should report as such.
    pub fn connect(service: &str) -> Result<Self> {
        Ok(Self {
            service: bootstrap::look_up(service)?,
            _value: PhantomData,
        })
    }

    /// Sends a value and does not wait for an answer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver is gone.
    pub async fn send(&self, value: &T) -> Result<()> {
        let payload = postcard::to_allocvec(value).map_err(|_| Error::Encode)?;
        send_async(self.service.as_raw(), &payload, None).await
    }

    /// Sends a value and waits for the answer.
    ///
    /// A fresh reply port per call, rather than one reused for the life of the
    /// sender, so concurrent calls cannot collect each other's answers.
    ///
    /// There is deliberately no timeout: the daemon answers when the world
    /// reaches the request, and a caller that does not want to wait can drop the
    /// future.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver exits before answering, and
    /// [`Error::Decode`] if the answer is not an `R`.
    pub async fn call<R: DeserializeOwned>(&self, value: &T) -> Result<R> {
        let payload = postcard::to_allocvec(value).map_err(|_| Error::Encode)?;
        let port = RecvRight::alloc()?;
        let interest = Interest::new(port.as_raw());
        let raw = port.as_raw();

        msg::send(
            self.service.as_raw(),
            Dest::CopySend,
            &payload,
            Some(raw),
            None,
            None,
        )?;

        let incoming = futures_lite::future::poll_fn(|cx| poll_recv(raw, &interest, cx)).await?;
        postcard::from_bytes(&incoming.payload).map_err(|_| Error::Decode)
    }

    /// Sends a value that asks for a lasting event channel, and returns the
    /// receiving end of it.
    ///
    /// The receive right stays here; the service only gets a send right to it,
    /// so dropping the returned [`Receiver`] is what tells the service we are
    /// gone.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver is gone.
    pub async fn subscribe<E: DeserializeOwned>(&self, value: &T) -> Result<Receiver<E>> {
        let payload = postcard::to_allocvec(value).map_err(|_| Error::Encode)?;
        let port = RecvRight::alloc()?;
        send_async(self.service.as_raw(), &payload, Some(port.as_raw())).await?;
        Ok(Receiver::from_port(port))
    }
}

impl<T> Clone for Sender<T> {
    /// Cloning duplicates the port right, so every clone reaches the same
    /// service and the last one dropped releases it.
    fn clone(&self) -> Self {
        Self {
            service: self.service.duplicate(),
            _value: PhantomData,
        }
    }
}
