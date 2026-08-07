//! Typed channels between unrelated processes, over Mach ports.
//!
//! One process binds a well-known service name and holds a [`Receiver<T>`]; any
//! number of unrelated processes connect a [`Sender<T>`] to that name and send
//! it values, mirroring the shape of `std`'s channels:
//!
//! ```no_run
//! # use serde::{Serialize, Deserialize};
//! # #[derive(Serialize, Deserialize)] struct Request;
//! # #[derive(Serialize, Deserialize)] struct Response;
//! # fn main() -> Result<(), paneru_mach_ipc::Error> {
//! use paneru_mach_ipc::{RecvPort, SendPort};
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
//!
//! // The same client with no executor to run on: same names, `_blocking`.
//! let response: Response = sender.call_blocking(&Request)?;
//! # Ok(())
//! # })
//! # }
//! ```
//!
//! Operations come in async/blocking pairs (`send`/`send_blocking`, and so on)
//! on [`SendPort`] and [`RecvPort`]. [`Receiver`] also implements [`Stream`]:
//! `poll_next` attempts a non-blocking receive and, when the port is empty,
//! registers the task's waker against a process-wide kqueue watching that port
//! (`EVFILT_MACHPORT`), rather than parking a thread on `mach_msg`.
//!
//! Values are encoded with `postcard`, which is why `T` is bounded by serde's
//! traits.

pub mod bootstrap;
mod error;
mod msg;
mod reactor;
pub mod rights;

pub use error::{Error, Result};
pub use futures_lite::Stream;
// Named only by the sealed `Inbound` supertrait; not part of the API a caller
// writes against, but it has to be reachable for that bound to typecheck.
#[doc(hidden)]
pub use reactor::Interest;
use rights::{RecvRight, SendOnceRight, SendRight};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

/// The one encode, so every operation reports the same error for the same fault.
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    postcard::to_allocvec(value).map_err(|_| Error::Encode)
}

/// The one decode, likewise.
fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T> {
    postcard::from_bytes(payload).map_err(|_| Error::Decode)
}

/// Attempts a receive and, if the port is empty, arms a one-shot wakeup.
/// Trying first is required for correctness: the kqueue registration only
/// reports messages arriving after it exists, so anything already queued
/// would otherwise never wake anyone.
fn poll_recv(
    port: &RecvRight,
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
    // registration; retrying once keeps it from being stranded.
    match msg::try_recv(port) {
        Err(Error::WouldBlock) => Poll::Pending,
        other => Poll::Ready(other),
    }
}

/// Hands a value to the kernel, yielding to the executor rather than blocking
/// if the destination's queue is momentarily full. There is no "writable"
/// event for a Mach port (`EVFILT_MACHPORT` reports arrivals only), so
/// cooperative yielding is used instead of parking a thread.
async fn send_async(
    service: &SendRight,
    payload: &[u8],
    extra_port: Option<&RecvRight>,
) -> Result<()> {
    loop {
        let message = msg::Outgoing::new(service, payload).without_waiting();
        let message = match extra_port {
            Some(port) => message.carrying(port),
            None => message,
        };
        match message.send() {
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
        let interest = Interest::new(&port);
        Self {
            port,
            interest,
            _value: PhantomData,
        }
    }

    fn poll_delivery(&self, cx: &Context<'_>) -> Poll<Result<Delivery<T>>> {
        poll_recv(&self.port, &self.interest, cx)
            .map(|result| result.and_then(Delivery::from_incoming))
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

impl<T: DeserializeOwned> Delivery<T> {
    /// Decodes one wire message into a delivery. Shared by the polled and the
    /// blocking receive so the two cannot drift on what a message means.
    fn from_incoming(incoming: msg::Incoming) -> Result<Self> {
        Ok(Self {
            value: decode(&incoming.payload)?,
            reply: incoming.reply.map(|right| Reply { right }),
            subscriber: incoming
                .ports
                .into_iter()
                .next()
                .map(|right| Subscriber { right }),
        })
    }
}

/// A one-shot channel back to whoever sent a value.
///
/// Consuming `self` on send matches the kernel: the underlying send-once right
/// is spent when the message goes out, so a second use could not work even if
/// the type allowed it.
#[derive(Debug)]
pub struct Reply {
    right: SendOnceRight,
}

impl Reply {
    /// Answers the sender. Not `async`: a send-once right's queue has never
    /// been used and never will be again, so there is no full-queue case to
    /// wait on.
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
    /// Pushes one event, without ever waiting. Deliberately not `async`: a
    /// subscriber that has stopped reading must not be able to stall the
    /// window manager, so a full queue drops the event rather than applying
    /// backpressure.
    ///
    /// # Errors
    ///
    /// [`Error::PeerGone`] means the subscriber's process is gone and it should
    /// be dropped. [`Error::WouldBlock`] means it is alive but not keeping up;
    /// the event is lost but the subscriber should be kept.
    pub fn try_send<E: Serialize>(&self, value: &E) -> Result<()> {
        let payload = encode(value)?;
        msg::Outgoing::new(&self.right, &payload)
            .without_waiting()
            .send()
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

/// The accessors the port traits are built on. Private, which is what seals
/// [`SendPort`] and [`RecvPort`]: only this crate can name these, so only this
/// crate can add implementations.
mod sealed {
    use crate::reactor::Interest;
    use crate::rights::{RecvRight, SendRight};

    pub trait Outbound {
        fn send_right(&self) -> &SendRight;
    }

    pub trait Inbound {
        fn recv_right(&self) -> &RecvRight;
        fn interest(&self) -> &Interest;
    }
}

/// One end of a channel that can be sent on. Everything below is a provided
/// method: an implementor says which send right it holds and what it carries,
/// and the operations follow — so the async and blocking spellings of the same
/// request cannot drift apart.
pub trait SendPort: sealed::Outbound {
    /// What this end carries.
    type Message: Serialize;

    /// Sends a value and does not wait for an answer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver is gone.
    fn send(&self, value: &Self::Message) -> impl Future<Output = Result<()>> {
        async move {
            let payload = encode(value)?;
            send_async(self.send_right(), &payload, None).await
        }
    }

    /// Sends a value and waits for the answer. A fresh reply port is allocated
    /// per call, rather than one reused for the life of the sender, so
    /// concurrent calls cannot collect each other's answers. There is
    /// deliberately no timeout; a caller that does not want to wait can drop
    /// the future.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver exits before answering, and
    /// [`Error::Decode`] if the answer is not an `R`.
    fn call<R: DeserializeOwned>(&self, value: &Self::Message) -> impl Future<Output = Result<R>> {
        async move {
            let port = self.request(value, Carried::AsReply)?;
            let interest = Interest::new(&port);
            let incoming =
                futures_lite::future::poll_fn(|cx| poll_recv(&port, &interest, cx)).await?;
            decode(&incoming.payload)
        }
    }

    /// Sends a value that asks for a lasting event channel, and returns the
    /// receiving end of it. The receive right stays here; the service only
    /// gets a send right to it, so dropping the returned [`Receiver`] is what
    /// tells the service we are gone.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver is gone.
    fn subscribe<E: DeserializeOwned>(
        &self,
        value: &Self::Message,
    ) -> impl Future<Output = Result<Receiver<E>>> {
        async move {
            let payload = encode(value)?;
            let port = RecvRight::alloc()?;
            send_async(self.send_right(), &payload, Some(&port)).await?;
            Ok(Receiver::from_port(port))
        }
    }

    /// [`Self::send`] without an executor.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver is gone.
    fn send_blocking(&self, value: &Self::Message) -> Result<()> {
        msg::Outgoing::new(self.send_right(), &encode(value)?).send()
    }

    /// [`Self::call`] without an executor.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver exits before answering, and
    /// [`Error::Decode`] if the answer is not an `R`.
    fn call_blocking<R: DeserializeOwned>(&self, value: &Self::Message) -> Result<R> {
        let port = self.request(value, Carried::AsReply)?;
        decode(&msg::recv(&port)?.payload)
    }

    /// [`Self::subscribe`] without an executor.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PeerGone`] if the receiver is gone.
    fn subscribe_blocking<E: DeserializeOwned>(
        &self,
        value: &Self::Message,
    ) -> Result<Receiver<E>> {
        Ok(Receiver::from_port(
            self.request(value, Carried::AsChannel)?,
        ))
    }

    /// Everything a request does except wait for the answer: encode, allocate
    /// somewhere for the peer to answer on, and hand the message over. The
    /// returned right is the only thing the two spellings of `call` disagree
    /// about — one polls it, the other blocks on it.
    #[doc(hidden)]
    fn request(&self, value: &Self::Message, carried: Carried) -> Result<RecvRight> {
        let payload = encode(value)?;
        let port = RecvRight::alloc()?;
        let message = msg::Outgoing::new(self.send_right(), &payload);
        match carried {
            Carried::AsReply => message.replying_to(&port),
            Carried::AsChannel => message.carrying(&port),
        }
        .send()?;
        Ok(port)
    }
}

/// Which way the port travelling with a request is meant to be used.
#[derive(Clone, Copy)]
#[doc(hidden)]
pub enum Carried {
    /// A one-shot answer to this request.
    AsReply,
    /// A lasting channel the peer keeps pushing to.
    AsChannel,
}

/// One end of a channel that can be received on.
///
/// The mirror of [`SendPort`]: state which receive right you hold, and both
/// spellings of `recv` follow.
pub trait RecvPort: sealed::Inbound {
    /// What this end carries.
    type Message: DeserializeOwned;

    /// Waits for the next value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] for a value that does not match the message
    /// type. The service port is reachable by any process in the session, so
    /// that is an expected input, not a fatal condition — log it and call
    /// `recv` again.
    fn recv(&self) -> impl Future<Output = Result<Delivery<Self::Message>>> {
        futures_lite::future::poll_fn(|cx| {
            poll_recv(self.recv_right(), self.interest(), cx)
                .map(|r| r.and_then(Delivery::from_incoming))
        })
    }

    /// [`Self::recv`] without an executor: parks the calling thread in the
    /// kernel until a message arrives.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] on the same terms as [`Self::recv`].
    fn recv_blocking(&self) -> Result<Delivery<Self::Message>> {
        Delivery::from_incoming(msg::recv(self.recv_right())?)
    }
}

impl<T> sealed::Outbound for Sender<T> {
    fn send_right(&self) -> &SendRight {
        &self.service
    }
}

impl<T: Serialize> SendPort for Sender<T> {
    type Message = T;
}

impl<T> sealed::Inbound for Receiver<T> {
    fn recv_right(&self) -> &RecvRight {
        &self.port
    }

    fn interest(&self) -> &Interest {
        &self.interest
    }
}

impl<T: DeserializeOwned> RecvPort for Receiver<T> {
    type Message = T;
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
