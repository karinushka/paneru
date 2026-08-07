//! Publishing and finding the daemon under a well-known name.
//!
//! A bootstrap name is owned by the kernel rather than the filesystem, so
//! there is no stale entry to clean up after a crash and no window where the
//! name exists but nothing is listening.

use mach2::bootstrap::{bootstrap_check_in, bootstrap_look_up, bootstrap_port, bootstrap_register};
use mach2::kern_return::KERN_SUCCESS;
use mach2::port::{MACH_PORT_NULL, mach_port_t};
use std::ffi::CString;

use crate::error::{Error, Result};
use crate::rights::{RecvRight, SendRight};

/// Claims a name launchd is holding for us.
///
/// Prefer this when running as a service: launchd creates the port at load
/// time and holds it across restarts, so clients that look up the name while
/// the daemon restarts get queued rather than refused.
///
/// # Errors
///
/// Returns [`Error::NotRunning`] if launchd has no such service — which, when
/// this is called by the daemon, means it was started outside launchd and
/// should fall back to [`register`].
pub fn check_in(name: &str) -> Result<RecvRight> {
    let cname = CString::new(name).map_err(|_| Error::InvalidName)?;
    let mut port: mach_port_t = MACH_PORT_NULL;

    // SAFETY: `cname` outlives the call and `port` is a valid out-parameter.
    let rc = unsafe { bootstrap_check_in(bootstrap_port, cname.as_ptr(), &raw mut port) };

    if rc == KERN_SUCCESS {
        // SAFETY: launchd just handed this task the receive right.
        Ok(unsafe { RecvRight::from_raw(port) })
    } else {
        Err(Error::from_kern(rc))
    }
}

/// Publishes a fresh port under `name`, for a daemon started outside launchd.
///
/// `bootstrap_register` is deprecated by Apple in favour of XPC but remains
/// functional; this is why [`check_in`] is tried first.
///
/// # Errors
///
/// Returns [`Error::AlreadyRunning`] when the name is taken, which means
/// another daemon is already live. This is a refusal to start, not something
/// to override.
pub fn register(name: &str) -> Result<RecvRight> {
    let cname = CString::new(name).map_err(|_| Error::InvalidName)?;
    let right = RecvRight::alloc()?;
    let send = right.make_send()?;

    // SAFETY: `cname` outlives the call and `send` names a live send right,
    // of which bootstrap takes a reference of its own.
    let rc =
        unsafe { bootstrap_register(bootstrap_port, cname.as_ptr().cast_mut(), send.as_raw()) };

    if rc == KERN_SUCCESS {
        Ok(right)
    } else {
        Err(Error::from_kern(rc))
    }
}

/// Finds the daemon's port.
///
/// # Errors
///
/// Returns [`Error::NotRunning`] when nothing has registered the name, which is
/// the ordinary "the daemon isn't up" case every client has to report.
pub fn look_up(name: &str) -> Result<SendRight> {
    let cname = CString::new(name).map_err(|_| Error::InvalidName)?;
    let mut port: mach_port_t = MACH_PORT_NULL;

    // SAFETY: `cname` outlives the call and `port` is a valid out-parameter.
    let rc = unsafe { bootstrap_look_up(bootstrap_port, cname.as_ptr(), &raw mut port) };

    if rc == KERN_SUCCESS {
        // SAFETY: bootstrap just handed this task the send right.
        Ok(unsafe { SendRight::from_raw(port) })
    } else {
        Err(Error::from_kern(rc))
    }
}
