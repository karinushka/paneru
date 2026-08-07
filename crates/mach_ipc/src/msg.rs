//! Building and parsing the two message shapes this protocol uses.
//!
//! # Why `packed(4)`
//!
//! A complex Mach message header is 24 bytes, then a 4-byte body count, so the
//! first descriptor begins at offset 28. A plain `#[repr(C)]` struct would pad
//! the OOL descriptor's 64-bit `address` to offset 32, describing a message the
//! kernel does not recognise — hence `packed(4)`, matching Apple's own
//! `#pragma pack(4)`. The offsets are asserted below rather than trusted.
//!
//! Because fields of a packed struct are not necessarily aligned, they are read
//! and written through raw pointers rather than by reference: taking a `&` to a
//! misaligned field is undefined behaviour even if it is never dereferenced.

use mach2::kern_return::KERN_SUCCESS;
use mach2::message::{
    MACH_MSG_OOL_DESCRIPTOR, MACH_MSG_PORT_DESCRIPTOR, MACH_MSG_TYPE_COPY_SEND,
    MACH_MSG_TYPE_MAKE_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE, MACH_MSG_TYPE_MOVE_SEND,
    MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSG_VIRTUAL_COPY, MACH_MSGH_BITS_COMPLEX,
    MACH_MSGH_BITS_REMOTE_MASK, MACH_RCV_MSG, MACH_RCV_TIMEOUT, MACH_SEND_MSG, MACH_SEND_TIMEOUT,
    mach_msg, mach_msg_body_t, mach_msg_header_t, mach_msg_ool_descriptor_t,
    mach_msg_port_descriptor_t, mach_msg_size_t,
};
use mach2::port::{MACH_PORT_NULL, mach_port_t};
use mach2::traps::mach_task_self;
use mach2::vm::mach_vm_deallocate;
use mach2::vm_types::mach_vm_address_t;

use crate::error::{Error, Result};
use crate::rights::{RecvRight, SendOnceRight, SendRight};

/// A request carrying its payload out of line, and nothing else.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct OolMsg {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
    payload: mach_msg_ool_descriptor_t,
}

/// The same, plus one port the sender is handing over — a subscriber's send
/// right.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct OolPortMsg {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
    payload: mach_msg_ool_descriptor_t,
    port: mach_msg_port_descriptor_t,
}

/// The kernel's layout, not the compiler's preference — checked at compile
/// time since a mismatch here means the transport silently talks nonsense.
const _: () = {
    assert!(size_of::<mach_msg_header_t>() == 24);
    assert!(std::mem::offset_of!(OolMsg, body) == 24);
    assert!(std::mem::offset_of!(OolMsg, payload) == 28);
    assert!(std::mem::offset_of!(OolPortMsg, payload) == 28);
    assert!(std::mem::offset_of!(OolPortMsg, port) == 44);
};

/// The dispositions a *received* right carries. Apple's headers define
/// `MACH_MSG_TYPE_PORT_SEND` and `..._SEND_ONCE` as aliases for the `MOVE`
/// variants, and mach2 does not re-export the aliases.
const RECEIVED_SEND: u32 = MACH_MSG_TYPE_MOVE_SEND;
const RECEIVED_SEND_ONCE: u32 = MACH_MSG_TYPE_MOVE_SEND_ONCE;

/// Where the `type_` byte sits inside every descriptor kind — Apple places it
/// at the same offset in all of them so a parser can read it before it knows
/// which layout it is looking at.
const DESC_TYPE_OFFSET: usize = 11;
/// A port descriptor is 12 bytes; the out-of-line kinds are 16 on 64-bit.
const PORT_DESC_SIZE: usize = 12;
const OOL_DESC_SIZE: usize = 16;
/// Header, body, two descriptors and the largest trailer the kernel appends,
/// rounded up. Our protocol never sends inline data, so nothing legitimate is
/// bigger than this and anything that is gets rejected rather than allocated
/// for.
const RECV_BUFFER_BYTES: usize = 256;

/// How the destination port right should be treated when sending.
#[derive(Clone, Copy)]
pub(crate) enum Dest {
    /// Keep our send right; the kernel takes a reference of its own. Used for
    /// the service port, which a client talks to repeatedly.
    CopySend,
    /// Hand over a send-once right, which the send consumes. Used for replies.
    MoveSendOnce,
}

/// An 8-byte aligned receive buffer.
///
/// Alignment matters: the header is read through a `*const mach_msg_header_t`,
/// and while the descriptors within are deliberately under-aligned, the buffer
/// they sit in must not be.
#[repr(C, align(8))]
struct RecvBuffer([u8; RECV_BUFFER_BYTES]);

/// One message off the wire, with its payload copied out and its rights owned.
pub struct Incoming {
    /// The request bytes, already copied out of the kernel's mapping.
    pub payload: Vec<u8>,
    /// Where the answer goes, if the sender asked for one.
    pub reply: Option<SendOnceRight>,
    /// Send rights the peer handed over — a subscriber registering itself.
    pub ports: Vec<SendRight>,
}

/// One outgoing message.
pub(crate) struct Outgoing<'a> {
    /// The port to send to. Raw only at this boundary: the constructors take
    /// the owned right, so a caller never hands over a name whose right has
    /// already been released.
    dest: mach_port_t,
    /// What the destination right is, and so what the kernel does with it.
    dest_kind: Dest,
    payload: &'a [u8],
    /// A receive right to answer on, turning this into a request.
    reply_port: Option<mach_port_t>,
    /// A further right to hand over, which is how a subscription is set up.
    extra_port: Option<mach_port_t>,
    /// `Some(0)` fails rather than waits when the peer's queue is full — what
    /// broadcasting needs, so a stalled subscriber cannot stall the daemon.
    /// `None` waits for room.
    timeout: Option<u32>,
}

impl Outgoing<'_> {
    /// The ordinary request/command: just a payload for the service.
    pub(crate) fn new<'a>(service: &SendRight, payload: &'a [u8]) -> Outgoing<'a> {
        Outgoing {
            dest: service.as_raw(),
            dest_kind: Dest::CopySend,
            payload,
            reply_port: None,
            extra_port: None,
            timeout: None,
        }
    }

    /// Asks for an answer on `port`, a right we own and keep.
    #[must_use]
    pub(crate) fn replying_to(mut self, port: &RecvRight) -> Self {
        self.reply_port = Some(port.as_raw());
        self
    }

    /// Carries a further right across, handing the peer a channel back.
    #[must_use]
    pub(crate) fn carrying(mut self, port: &RecvRight) -> Self {
        self.extra_port = Some(port.as_raw());
        self
    }

    /// Fails rather than waits when the peer's queue is full.
    #[must_use]
    pub(crate) fn without_waiting(mut self) -> Self {
        self.timeout = Some(0);
        self
    }

    /// An answer travelling back down a send-once right, which the send
    /// consumes — hence taking it by value rather than by reference.
    pub(crate) fn answering(right: SendOnceRight, payload: &[u8]) -> Outgoing<'_> {
        Outgoing {
            // `into_raw` because a successful send consumes the right; letting
            // `Drop` release it as well would be a double free.
            dest: right.into_raw(),
            dest_kind: Dest::MoveSendOnce,
            payload,
            reply_port: None,
            extra_port: None,
            timeout: None,
        }
    }

    /// Hands this message to the kernel, carrying its payload out of line.
    pub(crate) fn send(self) -> Result<()> {
        let Outgoing {
            dest,
            dest_kind,
            payload,
            reply_port,
            extra_port,
            timeout,
        } = self;
        let remote = match dest_kind {
            Dest::CopySend => MACH_MSG_TYPE_COPY_SEND,
            Dest::MoveSendOnce => MACH_MSG_TYPE_MOVE_SEND_ONCE,
        };
        let local = if reply_port.is_some() {
            MACH_MSG_TYPE_MAKE_SEND_ONCE
        } else {
            0
        };

        let size = if extra_port.is_some() {
            size_of::<OolPortMsg>()
        } else {
            size_of::<OolMsg>()
        };

        let header = mach_msg_header_t {
            msgh_bits: (remote | (local << 8)) | MACH_MSGH_BITS_COMPLEX,
            msgh_size: u32::try_from(size).map_err(|_| Error::Malformed("message too large"))?,
            msgh_remote_port: dest,
            msgh_local_port: reply_port.unwrap_or(MACH_PORT_NULL),
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: 0,
        };
        let body = mach_msg_body_t {
            msgh_descriptor_count: if extra_port.is_some() { 2 } else { 1 },
        };
        // `deallocate: false` — the kernel copies out of our buffer and we keep it.
        // Virtual copy so it is copy-on-write rather than an eager bulk copy, which
        // is what makes a large window set cheap to send.
        let descriptor = mach_msg_ool_descriptor_t::new(
            payload.as_ptr() as *mut _,
            false,
            MACH_MSG_VIRTUAL_COPY,
            mach_msg_size_t::try_from(payload.len())
                .map_err(|_| Error::Malformed("payload too large"))?,
        );

        let mut with_port;
        let mut without_port;
        let (buffer, len): (*mut mach_msg_header_t, usize) = if let Some(port) = extra_port {
            with_port = OolPortMsg {
                header,
                body,
                payload: descriptor,
                port: mach_msg_port_descriptor_t::new(port, MACH_MSG_TYPE_MAKE_SEND),
            };
            (std::ptr::addr_of_mut!(with_port).cast(), size)
        } else {
            without_port = OolMsg {
                header,
                body,
                payload: descriptor,
            };
            (std::ptr::addr_of_mut!(without_port).cast(), size)
        };

        let (options, timeout_ms) = match timeout {
            Some(ms) => (MACH_SEND_MSG | MACH_SEND_TIMEOUT, ms),
            None => (MACH_SEND_MSG, 0),
        };

        // SAFETY: `buffer` points at a fully initialised message of `len` bytes
        // whose header describes it, and `payload` outlives this call.
        let rc = unsafe {
            mach_msg(
                buffer,
                options,
                u32::try_from(len).map_err(|_| Error::Malformed("message too large"))?,
                0,
                MACH_PORT_NULL,
                timeout_ms,
                MACH_PORT_NULL,
            )
        };

        if rc == KERN_SUCCESS {
            Ok(())
        } else {
            Err(Error::from_kern(rc))
        }
    }
}

/// Receives without blocking, reporting [`Error::WouldBlock`] when the port is
/// empty.
pub(crate) fn try_recv(port: &RecvRight) -> Result<Incoming> {
    recv_with(port.as_raw(), MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0)
}

/// Receives, parking the calling thread in the kernel until a message arrives.
/// The same call as [`try_recv`] without `MACH_RCV_TIMEOUT`, which is what
/// turns the immediate `WouldBlock` return into an indefinite wait.
pub(crate) fn recv(port: &RecvRight) -> Result<Incoming> {
    recv_with(port.as_raw(), MACH_RCV_MSG, 0)
}

/// A message this protocol does not understand is reported as
/// [`Error::Malformed`] rather than panicking: the service port is reachable by
/// any process in the session, so a bad frame is an expected input, not a bug.
fn recv_with(port: mach_port_t, options: i32, timeout: u32) -> Result<Incoming> {
    let mut buffer = RecvBuffer([0u8; RECV_BUFFER_BYTES]);

    // SAFETY: `buffer` is 8-aligned and `RECV_BUFFER_BYTES` long, which is what
    // the size argument claims.
    let rc = unsafe {
        mach_msg(
            std::ptr::addr_of_mut!(buffer).cast::<mach_msg_header_t>(),
            options,
            0,
            u32::try_from(RECV_BUFFER_BYTES).expect("the buffer size fits in a u32"),
            port,
            timeout,
            MACH_PORT_NULL,
        )
    };
    if rc != KERN_SUCCESS {
        return Err(Error::from_kern(rc));
    }

    parse(&buffer.0)
}

/// Pulls the payload and any rights out of a received message.
///
/// `cast_ptr_alignment` is allowed throughout: `bytes` is always the interior
/// of a [`RecvBuffer`], which is `repr(align(8))` precisely so these reads are
/// aligned, but clippy cannot see that through the slice. The descriptor reads
/// that genuinely *are* misaligned use `read_unaligned` and are marked as such.
#[allow(clippy::cast_ptr_alignment)]
fn parse(bytes: &[u8]) -> Result<Incoming> {
    // SAFETY: `bytes` is the 8-aligned receive buffer and is longer than a
    // header, which `mach_msg` has just filled in.
    let header = unsafe { std::ptr::read(bytes.as_ptr().cast::<mach_msg_header_t>()) };

    // The sender's local port arrives as our *remote* port — the fields swap on
    // receive, because the reply destination is now the far end from our side.
    // Its disposition is therefore in the remote bits, not the local ones.
    let reply = match header.msgh_bits & MACH_MSGH_BITS_REMOTE_MASK {
        RECEIVED_SEND_ONCE if header.msgh_remote_port != MACH_PORT_NULL => {
            // SAFETY: the kernel just gave this task the right.
            Some(unsafe { SendOnceRight::from_raw(header.msgh_remote_port) })
        }
        _ => None,
    };

    if header.msgh_bits & MACH_MSGH_BITS_COMPLEX == 0 {
        return Err(Error::Malformed("not a complex message"));
    }

    // SAFETY: a complex message has a body immediately after the header, and
    // the buffer is long enough for both.
    let body = unsafe {
        std::ptr::read(
            bytes
                .as_ptr()
                .add(size_of::<mach_msg_header_t>())
                .cast::<mach_msg_body_t>(),
        )
    };

    let mut offset = size_of::<mach_msg_header_t>() + size_of::<mach_msg_body_t>();
    let mut payload = None;
    let mut ports = Vec::new();

    for _ in 0..body.msgh_descriptor_count {
        if offset + DESC_TYPE_OFFSET >= bytes.len() {
            return Err(Error::Malformed("descriptor runs past the message"));
        }
        let kind = u32::from(bytes[offset + DESC_TYPE_OFFSET]);

        match kind {
            MACH_MSG_OOL_DESCRIPTOR => {
                if offset + OOL_DESC_SIZE > bytes.len() {
                    return Err(Error::Malformed("out-of-line descriptor is truncated"));
                }
                // SAFETY: read unaligned — the descriptor sits at a 4-byte
                // offset but leads with a 64-bit address, so it may not meet
                // the type's alignment. This is precisely the case where a
                // reference would be undefined behaviour.
                let desc = unsafe {
                    std::ptr::read_unaligned(
                        bytes
                            .as_ptr()
                            .add(offset)
                            .cast::<mach_msg_ool_descriptor_t>(),
                    )
                };
                let address = desc.address;
                let size = desc.size as usize;

                if !address.is_null() && size > 0 {
                    // SAFETY: the kernel mapped `size` readable bytes at
                    // `address` into this task for exactly this purpose.
                    let mapped = unsafe { std::slice::from_raw_parts(address.cast::<u8>(), size) };
                    payload = Some(mapped.to_vec());

                    // The mapping is ours now and nothing else will release it.
                    // SAFETY: `address`/`size` name the region just received.
                    unsafe {
                        mach_vm_deallocate(
                            mach_task_self(),
                            address as mach_vm_address_t,
                            size as u64,
                        );
                    }
                } else {
                    payload = Some(Vec::new());
                }
                offset += OOL_DESC_SIZE;
            }
            MACH_MSG_PORT_DESCRIPTOR => {
                if offset + PORT_DESC_SIZE > bytes.len() {
                    return Err(Error::Malformed("port descriptor is truncated"));
                }
                // SAFETY: as above; 12 bytes of descriptor at a 4-byte offset.
                let desc = unsafe {
                    std::ptr::read_unaligned(
                        bytes
                            .as_ptr()
                            .add(offset)
                            .cast::<mach_msg_port_descriptor_t>(),
                    )
                };
                if u32::from(desc.disposition) == RECEIVED_SEND && desc.name != MACH_PORT_NULL {
                    // SAFETY: the kernel just transferred this send right to us.
                    ports.push(unsafe { SendRight::from_raw(desc.name) });
                }
                offset += PORT_DESC_SIZE;
            }
            _ => return Err(Error::Malformed("unsupported descriptor kind")),
        }
    }

    Ok(Incoming {
        payload: payload.ok_or(Error::Malformed("no payload descriptor"))?,
        reply,
        ports,
    })
}

/// Sends `payload` on a send-once right, consuming it.
pub(crate) fn reply(right: SendOnceRight, payload: &[u8]) -> Result<()> {
    Outgoing::answering(right, payload).send()
}
