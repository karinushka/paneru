//! Turning "a Mach port has a message" into "a waker fires".
//!
//! `EVFILT_MACHPORT` is the only kernel mechanism that lets an executor poll a
//! Mach port. One process-wide kqueue holds every registered port, serviced by
//! a single thread blocked in `kevent`.
//!
//! Registrations are `EV_ONESHOT` and must be re-armed by the next
//! `Poll::Pending`: messages are drained with `mach_msg`, not through
//! `kevent`, so a level-triggered registration would stay ready forever and
//! spin.
//!
//! Every caller must try to receive *before* it waits, and must re-arm before
//! returning `Pending` — a message that arrived before the registration
//! existed produces no event at all, so a wait-first loop would hang on a
//! message already sitting in the queue.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Mutex, OnceLock};
use std::task::Waker;

use mach2::port::mach_port_t;

use crate::error::{Error, Result};
use crate::rights::RecvRight;

/// The process-wide kqueue and the wakers waiting on it.
struct Reactor {
    kqueue: OwnedFd,
    /// One waker per port. A port is only ever awaited by one task, so
    /// replacing an existing entry means the previous future was dropped.
    wakers: Mutex<HashMap<mach_port_t, Waker>>,
}

static REACTOR: OnceLock<&'static Reactor> = OnceLock::new();

impl Reactor {
    /// The one reactor, started on first use.
    fn global() -> Result<&'static Reactor> {
        // `OnceLock::get_or_init` cannot fail, so the fallible setup is done
        // first and only a successful reactor is ever published.
        if let Some(reactor) = REACTOR.get() {
            return Ok(reactor);
        }

        // SAFETY: `kqueue` takes no arguments and returns a descriptor or -1.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        let kqueue = unsafe { OwnedFd::from_raw_fd(raw) };

        let reactor: &'static Reactor = Box::leak(Box::new(Reactor {
            kqueue,
            wakers: Mutex::new(HashMap::new()),
        }));

        match REACTOR.set(reactor) {
            Ok(()) => {
                std::thread::Builder::new()
                    .name("mach-ipc-reactor".into())
                    .spawn(|| reactor.run())
                    .map_err(Error::Io)?;
                Ok(reactor)
            }
            // Another thread won the race; the reactor built here is simply
            // leaked (this happens at most once in the process's life).
            Err(_) => Ok(REACTOR.get().expect("the winner published its reactor")),
        }
    }

    /// Blocks in `kevent` forever, waking whoever asked about each port.
    ///
    /// The casts are bounded by the 16-entry array below, so none can
    /// truncate; `count` is only cast after being checked non-negative.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn run(&self) {
        let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: `events` is a valid array of the stated length; a null
            // timeout blocks until something arrives.
            let count = unsafe {
                libc::kevent(
                    self.kqueue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    std::ptr::null(),
                )
            };

            if count < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // The kqueue is unusable. Wake everyone so their `try_recv`
                // reports the real error rather than hanging forever.
                let drained: Vec<Waker> = self
                    .wakers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .drain()
                    .map(|(_, waker)| waker)
                    .collect();
                for waker in drained {
                    waker.wake();
                }
                return;
            }

            for event in &events[..usize::try_from(count).expect("count is non-negative")] {
                // `ident` holds the port name this registration was made with,
                // so the narrowing is exact by construction.
                let port = event.ident as mach_port_t;
                let waker = self
                    .wakers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&port);
                if let Some(waker) = waker {
                    waker.wake();
                }
            }
        }
    }

    /// Arms a one-shot registration for `port` and records `waker`.
    ///
    /// The waker is stored *before* the registration is armed. The reverse
    /// order races: the reactor thread could see the event and look for a waker
    /// that has not been stored yet, and the wakeup would be lost.
    fn arm(&self, port: mach_port_t, waker: &Waker) -> Result<()> {
        self.wakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(port, waker.clone());

        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = port as usize;
        change.filter = libc::EVFILT_MACHPORT;
        change.flags = libc::EV_ADD | libc::EV_ONESHOT;

        // SAFETY: `change` is one initialised kevent and no events are read
        // back, so the output pointer may be null.
        let rc = unsafe {
            libc::kevent(
                self.kqueue.as_raw_fd(),
                &raw const change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };

        if rc < 0 {
            self.wakers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&port);
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Forgets any pending interest in `port`, so a dropped future leaves
    /// nothing behind that could wake into a freed task.
    fn disarm(&self, port: mach_port_t) {
        self.wakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&port);

        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = port as usize;
        change.filter = libc::EVFILT_MACHPORT;
        change.flags = libc::EV_DELETE;

        // SAFETY: as in `arm`. A failure here means the registration was
        // already consumed or never existed, which is the desired end state
        // anyway, so the result is deliberately ignored.
        unsafe {
            libc::kevent(
                self.kqueue.as_raw_fd(),
                &raw const change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            );
        }
    }
}

/// A port's registration with the reactor, torn down when the owner is dropped.
#[derive(Debug)]
/// Public only so the sealed `Inbound` supertrait can name it; nothing outside
/// this crate can construct or use one.
#[doc(hidden)]
pub struct Interest {
    port: mach_port_t,
    /// Whether a one-shot registration is currently armed, so `Drop` only
    /// bothers the kqueue when there is something to remove.
    armed: std::cell::Cell<bool>,
}

impl Interest {
    pub(crate) fn new(port: &RecvRight) -> Self {
        Self {
            port: port.as_raw(),
            armed: std::cell::Cell::new(false),
        }
    }

    /// Asks to be woken when `port` next has a message.
    ///
    /// Call this only after a receive has reported [`Error::WouldBlock`], and
    /// only immediately before returning [`std::task::Poll::Pending`].
    pub(crate) fn arm(&self, waker: &Waker) -> Result<()> {
        Reactor::global()?.arm(self.port, waker)?;
        self.armed.set(true);
        Ok(())
    }
}

impl Drop for Interest {
    fn drop(&mut self) {
        if self.armed.get()
            && let Some(reactor) = REACTOR.get()
        {
            reactor.disarm(self.port);
        }
    }
}
