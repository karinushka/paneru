//! Owned Mach port rights.
//!
//! A Mach port name may hold several separately reference-counted rights;
//! releasing the wrong kind (or the same kind twice) corrupts the task's IPC
//! space. These types make that reference counting an ownership question the
//! compiler enforces.

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_port::{mach_port_deallocate, mach_port_mod_refs};
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use mach2::traps::mach_task_self;

use crate::error::{Error, Result};

/// The receive end of a port: the side that gets messages.
///
/// A port has exactly one receive right; holding it is what it means to be
/// the server for that name.
#[derive(Debug)]
pub struct RecvRight(mach_port_t);

impl RecvRight {
    /// Allocates a fresh port and takes its receive right.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Mach`] if the kernel refuses the allocation, which in
    /// practice means the task's IPC space is exhausted.
    pub fn alloc() -> Result<Self> {
        let mut name: mach_port_t = MACH_PORT_NULL;
        // SAFETY: `name` is a valid out-parameter for the duration of the call.
        let rc = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach_task_self(),
                MACH_PORT_RIGHT_RECEIVE,
                &raw mut name,
            )
        };
        if rc == KERN_SUCCESS {
            Ok(Self(name))
        } else {
            Err(Error::Mach(rc))
        }
    }

    /// Adopts a receive right the kernel or launchd has already handed us.
    ///
    /// # Safety
    ///
    /// `name` must name a receive right this task owns and that nothing else
    /// will release — ownership passes to the returned value.
    #[must_use]
    pub unsafe fn from_raw(name: mach_port_t) -> Self {
        Self(name)
    }

    /// Manufactures a send right for this port. Callers may make as many as
    /// they like; every one of them can reach this receive right.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Mach`] if the kernel refuses to insert the right.
    pub fn make_send(&self) -> Result<SendRight> {
        // SAFETY: `self.0` names a receive right this value owns.
        let rc = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach_task_self(),
                self.0,
                self.0,
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        if rc == KERN_SUCCESS {
            Ok(SendRight(self.0))
        } else {
            Err(Error::Mach(rc))
        }
    }

    /// The underlying port name, for the few places that must speak to
    /// `mach_msg` or bootstrap directly.
    #[must_use]
    pub fn as_raw(&self) -> mach_port_t {
        self.0
    }
}

impl Drop for RecvRight {
    fn drop(&mut self) {
        // SAFETY: `self.0` names a receive right this value owns, released
        // exactly once because `Drop` runs once.
        unsafe {
            mach_port_mod_refs(mach_task_self(), self.0, MACH_PORT_RIGHT_RECEIVE, -1);
        }
    }
}

/// A right to send messages to some port, held for as long as we like.
///
/// This is what `bootstrap_look_up` returns to a client, and what a subscriber
/// hands the daemon so it can push events back.
#[derive(Debug)]
pub struct SendRight(mach_port_t);

impl SendRight {
    /// Adopts a send right obtained elsewhere — from bootstrap, or parsed out
    /// of a received message.
    ///
    /// # Safety
    ///
    /// `name` must name a send right this task owns; ownership passes to the
    /// returned value.
    #[must_use]
    pub unsafe fn from_raw(name: mach_port_t) -> Self {
        Self(name)
    }

    /// The underlying port name.
    #[must_use]
    pub fn as_raw(&self) -> mach_port_t {
        self.0
    }

    /// Takes another reference to the same send right.
    ///
    /// Not `Clone`: incrementing the kernel's per-name reference count can
    /// fail. On failure the copy is still returned; its own send will report
    /// [`crate::Error::PeerGone`].
    #[must_use]
    pub fn duplicate(&self) -> Self {
        // SAFETY: `self.0` names a send right this value owns; this takes one
        // more reference to it, which the returned value releases on drop.
        unsafe {
            mach2::mach_port::mach_port_mod_refs(
                mach_task_self(),
                self.0,
                mach2::port::MACH_PORT_RIGHT_SEND,
                1,
            );
        }
        Self(self.0)
    }
}

impl Drop for SendRight {
    fn drop(&mut self) {
        // SAFETY: `self.0` names a send right this value owns. Send rights are
        // released with `deallocate`, not `mod_refs`.
        unsafe {
            mach_port_deallocate(mach_task_self(), self.0);
        }
    }
}

/// A right to send exactly one message, then nothing.
///
/// This is the reply channel: created by the client, sent inside the request,
/// and consumed by the answer.
#[derive(Debug)]
pub struct SendOnceRight(mach_port_t);

impl SendOnceRight {
    /// Adopts a send-once right parsed out of a received message.
    ///
    /// # Safety
    ///
    /// `name` must name a send-once right this task owns; ownership passes to
    /// the returned value.
    #[must_use]
    pub unsafe fn from_raw(name: mach_port_t) -> Self {
        Self(name)
    }

    /// The underlying port name.
    #[must_use]
    pub fn as_raw(&self) -> mach_port_t {
        self.0
    }

    /// Gives up ownership without releasing the right, for when `mach_msg` is
    /// about to consume it. Sending a message consumes the send-once right, so
    /// letting `Drop` also release it would be a double free.
    #[must_use]
    pub(crate) fn into_raw(self) -> mach_port_t {
        let name = self.0;
        std::mem::forget(self);
        name
    }
}

impl Drop for SendOnceRight {
    fn drop(&mut self) {
        // SAFETY: `self.0` names an unused send-once right this value owns.
        // A *used* one was consumed by `mach_msg` and reached `into_raw`
        // instead, so this cannot double-release.
        unsafe {
            mach_port_deallocate(mach_task_self(), self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_can_be_allocated_and_released() {
        let right = RecvRight::alloc().expect("allocate a port");
        assert_ne!(right.as_raw(), MACH_PORT_NULL);
        drop(right);
    }

    #[test]
    fn one_receive_right_backs_many_send_rights() {
        let recv = RecvRight::alloc().expect("allocate a port");
        let first = recv.make_send().expect("make a send right");
        let second = recv.make_send().expect("make another send right");
        assert_eq!(first.as_raw(), recv.as_raw());
        assert_eq!(second.as_raw(), recv.as_raw());
    }
}
