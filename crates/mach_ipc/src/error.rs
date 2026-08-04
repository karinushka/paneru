//! Errors this transport can produce, and the handful of Mach codes worth
//! naming rather than passing through as numbers.

use mach2::kern_return::kern_return_t;
use std::fmt;

/// The result of a transport operation.
pub type Result<T> = std::result::Result<T, Error>;

/// `MACH_SEND_INVALID_DEST` — the destination port is gone, which for us always
/// means the process on the other end exited.
pub const MACH_SEND_INVALID_DEST: kern_return_t = 0x1000_0003;
/// `MACH_SEND_TIMED_OUT` — the queue was full and we asked not to wait.
pub const MACH_SEND_TIMED_OUT: kern_return_t = 0x1000_0004;
/// `MACH_RCV_TIMED_OUT` — nothing was queued and we asked not to wait. This is
/// the ordinary "not ready yet" of the async receive loop, not a failure.
pub const MACH_RCV_TIMED_OUT: kern_return_t = 0x1000_4003;
/// `BOOTSTRAP_UNKNOWN_SERVICE` — nobody has registered that name.
pub const BOOTSTRAP_UNKNOWN_SERVICE: kern_return_t = 1102;
/// `BOOTSTRAP_NAME_IN_USE` — somebody already has.
pub const BOOTSTRAP_NAME_IN_USE: kern_return_t = 1101;
/// `BOOTSTRAP_SERVICE_ACTIVE` — the name is registered *and* its owner is
/// alive. This, not `NAME_IN_USE`, is what a second daemon actually gets.
pub const BOOTSTRAP_SERVICE_ACTIVE: kern_return_t = 1103;

#[derive(Debug)]
pub enum Error {
    /// No daemon is registered under the service name. This is the ordinary
    /// "paneru is not running" case, and clients should say exactly that.
    NotRunning,
    /// Another process already owns the service name — a second daemon.
    AlreadyRunning,
    /// The peer is gone. On a reply this means the client stopped waiting; on a
    /// subscriber push it means the subscriber died and should be reaped.
    PeerGone,
    /// The peer's queue is full and we declined to wait for it.
    WouldBlock,
    /// A message arrived that this protocol cannot make sense of.
    Malformed(&'static str),
    /// The service name contained an interior NUL and cannot cross into C.
    InvalidName,
    /// A descriptor-level failure from the kqueue backing an async wait.
    Io(std::io::Error),
    /// A value could not be encoded for the wire.
    Encode,
    /// A message arrived that does not decode as the expected type. Any process
    /// in the session can reach a service port, so this is an expected input
    /// rather than a bug.
    Decode,
    /// Anything else the kernel said, kept as its raw code.
    Mach(kern_return_t),
}

impl Error {
    /// Maps a raw kernel status onto the named cases, so callers match on
    /// meaning instead of remembering hex constants.
    pub(crate) fn from_kern(rc: kern_return_t) -> Self {
        match rc {
            MACH_SEND_INVALID_DEST => Self::PeerGone,
            MACH_SEND_TIMED_OUT | MACH_RCV_TIMED_OUT => Self::WouldBlock,
            BOOTSTRAP_UNKNOWN_SERVICE => Self::NotRunning,
            BOOTSTRAP_NAME_IN_USE | BOOTSTRAP_SERVICE_ACTIVE => Self::AlreadyRunning,
            other => Self::Mach(other),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => write!(f, "paneru is not running"),
            Self::AlreadyRunning => write!(f, "another paneru instance owns the service name"),
            Self::PeerGone => write!(f, "the peer has exited"),
            Self::WouldBlock => write!(f, "the peer's message queue is full"),
            Self::Malformed(what) => write!(f, "malformed message: {what}"),
            Self::InvalidName => write!(f, "the service name contains an interior NUL"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Encode => write!(f, "the value could not be encoded"),
            Self::Decode => write!(f, "the message did not decode as the expected type"),
            Self::Mach(rc) => write!(f, "mach error {rc:#010x}"),
        }
    }
}

impl std::error::Error for Error {}
