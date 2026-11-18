//! Live window previews: one `ScreenCaptureKit` stream per card, feeding
//! each frame's `IOSurface` straight into the card's content layer.
//!
//! The overview still takes a static snapshot of every window (see
//! `view.rs`): that is what shows the instant it opens. Streams take a
//! moment to start (tens of milliseconds) and then replace the snapshot
//! with the live window.
//!
//! A window captured on its own keeps drawing as long as part of it is on
//! screen, which paneru guarantees: parked windows keep a sliver visible, so
//! even windows on hidden rows stream. Capturing each window by its own
//! `WindowServer` id keeps every card isolated from neighbouring windows,
//! desktop wallpaper, and overlays even while the real strip is animating.
//!
//! Streams only run while the overview is up; [`Live::stop_all`] ends them.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBufferGetIOSurface;
use objc2_foundation::NSError;
use objc2_quartz_core::{CALayer, CATransaction};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};
use tracing::{debug, warn};

use crate::events::{Event, EventSender};

/// Frames per second of a live preview.
const FPS: i32 = 30;

/// One card to stream into.
pub struct Wanted {
    pub window_id: i32,
    pub layer: Retained<CALayer>,
    /// Pixels per point to stream at (the card's sharpness when fully open
    /// matters more for the focused window than for far-away ones).
    pub scale: f64,
}

struct OutputIvars {
    layer: RefCell<Retained<CALayer>>,
    /// Whether a frame has been shown yet.
    started: Cell<bool>,
    /// Shared with [`Live`]: windows whose stream already shows frames, so
    /// a snapshot landing late doesn't cover them.
    showing: Rc<RefCell<HashSet<i32>>>,
    window_id: i32,
    since: Instant,
    /// Set once the stream is stopped: frames still queued are dropped.
    stopped: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruLiveOutput"]
    #[ivars = OutputIvars]
    struct LiveOutput;

    unsafe impl NSObjectProtocol for LiveOutput {}

    unsafe impl SCStreamOutput for LiveOutput {
        // Delivered on the main queue (see `addStreamOutput` below).
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.show(sample);
            }
        }
    }
);

impl LiveOutput {
    fn new(
        mtm: MainThreadMarker,
        window_id: i32,
        layer: Retained<CALayer>,
        showing: Rc<RefCell<HashSet<i32>>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(OutputIvars {
            layer: RefCell::new(layer),
            started: Cell::new(false),
            showing,
            window_id,
            since: Instant::now(),
            stopped: Cell::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn show(&self, sample: &CMSampleBuffer) {
        if self.ivars().stopped.get() {
            return;
        }
        // Frames where nothing changed carry no image.
        let Some(buffer) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        let Some(surface) = CVPixelBufferGetIOSurface(Some(&buffer)) else {
            return;
        };
        let ivars = self.ivars();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let layer = ivars.layer.borrow();
        // SAFETY: an IOSurface is a CF object, usable as `id` layer contents.
        let object = unsafe { &*(&raw const *surface).cast::<AnyObject>() };
        unsafe { layer.setContents(Some(object)) };
        layer.setBackgroundColor(None);
        CATransaction::commit();
        if !ivars.started.replace(true) {
            ivars.showing.borrow_mut().insert(ivars.window_id);
            debug!(
                "overview: live window {} after {:?}",
                ivars.window_id,
                ivars.since.elapsed()
            );
        }
    }

    fn retarget(&self, layer: Retained<CALayer>) {
        if std::ptr::eq(
            Retained::as_ptr(&self.ivars().layer.borrow()),
            Retained::as_ptr(&layer),
        ) {
            // Same card: keep showing frames as before.
            return;
        }
        *self.ivars().layer.borrow_mut() = layer;
        // The new layer starts with the snapshot; the next frame replaces it.
        self.ivars().started.set(false);
        self.ivars()
            .showing
            .borrow_mut()
            .remove(&self.ivars().window_id);
    }
}

struct Stream {
    capture: Retained<SCStream>,
    output: Retained<LiveOutput>,
}

impl Stream {
    fn stop(&self) {
        self.output.ivars().stopped.set(true);
        unsafe { self.capture.stopCaptureWithCompletionHandler(None) };
    }
}

/// How long a stopped stream is kept alive, so frames already queued for
/// the main thread never reach a freed output.
const STOP_GRACE: Duration = Duration::from_secs(2);

struct SendShareableContent(Retained<SCShareableContent>);

// SAFETY: `SCShareableContent` is an immutable snapshot produced on a
// ScreenCaptureKit callback queue and safe to transfer to the main thread.
unsafe impl Send for SendShareableContent {}

/// The overview's live streams, keyed by window id.
pub struct Live {
    mtm: MainThreadMarker,
    sender: EventSender,
    streams: HashMap<i32, Stream>,
    /// Stopped streams, released after [`STOP_GRACE`].
    stopping: Vec<(Instant, Stream)>,
    /// Cards waiting for the shareable-content lookup to start streaming.
    pending: HashMap<i32, Wanted>,
    /// Identifies the current lookup; older ones are ignored.
    request: u64,
    /// Whether `request` is still awaiting `SCShareableContent` completion, so
    /// rapid relayouts of existing windows update `pending` in place instead
    /// of cancelling and restarting the XPC lookup on every keypress.
    in_flight: bool,
    tx: Sender<(u64, Option<SendShareableContent>)>,
    rx: Receiver<(u64, Option<SendShareableContent>)>,
    showing: Rc<RefCell<HashSet<i32>>>,
}

impl Live {
    pub fn new(mtm: MainThreadMarker, sender: EventSender) -> Self {
        let (tx, rx) = channel();
        Self {
            mtm,
            sender,
            streams: HashMap::new(),
            stopping: Vec::new(),
            pending: HashMap::new(),
            request: 0,
            in_flight: false,
            tx,
            rx,
            showing: Rc::new(RefCell::new(HashSet::new())),
        }
    }

    /// Forces the next [`Self::set`] with unstreamed windows to query
    /// `SCShareableContent` afresh (called when windows were added or rebuilt).
    pub fn invalidate_lookup(&mut self) {
        self.in_flight = false;
        self.request += 1;
    }

    /// Whether `window_id` already shows live frames.
    pub fn is_showing(&self, window_id: i32) -> bool {
        self.showing.borrow().contains(&window_id)
    }

    /// Whether stopped streams are still inside [`STOP_GRACE`] waiting to drop.
    pub fn has_stopping(&self) -> bool {
        !self.stopping.is_empty()
    }

    fn prune_stopped(&mut self) {
        let now = Instant::now();
        self.stopping
            .retain(|(since, _)| now.saturating_duration_since(*since) < STOP_GRACE);
    }

    /// Streams exactly `wanted`: running streams move to their new layer,
    /// the rest stop, and new ones start once `ScreenCaptureKit` has
    /// listed the windows (asynchronously; see [`Live::poll`]).
    pub fn set(&mut self, wanted: Vec<Wanted>) {
        let keep: HashSet<i32> = wanted.iter().map(|w| w.window_id).collect();
        self.prune_stopped();
        let now = Instant::now();
        self.stopping
            .extend(
                self.streams
                    .extract_if(|id, _| !keep.contains(id))
                    .map(|(_, stream)| {
                        stream.stop();
                        (now, stream)
                    }),
            );
        self.showing
            .borrow_mut()
            .retain(|id| self.streams.contains_key(id));
        self.pending.clear();
        for want in wanted {
            match self.streams.get(&want.window_id) {
                Some(stream) => stream.output.retarget(want.layer),
                None => {
                    self.pending.insert(want.window_id, want);
                }
            }
        }
        if self.pending.is_empty() {
            self.in_flight = false;
            self.request += 1;
            return;
        }
        if self.in_flight {
            return;
        }
        self.request += 1;
        self.in_flight = true;
        let (request, tx, wake) = (self.request, self.tx.clone(), self.sender.clone());
        let handler = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                if let Some(error) = unsafe { error.as_ref() } {
                    warn!("overview: listing windows for live previews: {error}");
                }
                let content = (!content.is_null())
                    .then(|| unsafe { Retained::retain(content) })
                    .flatten()
                    .map(SendShareableContent);
                if tx.send((request, content)).is_err() {
                    return;
                }
                _ = wake.send(Event::OverviewCaptureLanded);
            },
        );
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, false, &handler,
            );
        }
    }

    /// Prunes expired stopped streams and starts the streams of a window
    /// listing that came back.
    pub fn poll(&mut self) {
        self.prune_stopped();
        while let Ok((request, content)) = self.rx.try_recv() {
            if request != self.request {
                continue;
            }
            self.in_flight = false;
            let mut pending = std::mem::take(&mut self.pending);
            let Some(SendShareableContent(content)) = content else {
                continue;
            };
            let windows = unsafe { content.windows() };
            for window in &windows {
                let Some((id, want)) = i32::try_from(unsafe { window.windowID() })
                    .ok()
                    .and_then(|id| pending.remove(&id).map(|want| (id, want)))
                else {
                    continue;
                };
                let frame = unsafe { window.frame() };
                let filter = unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    )
                };
                if let Some(stream) =
                    self.start(&filter, frame.size.width, frame.size.height, &want)
                {
                    self.streams.insert(id, stream);
                }
            }
            for id in pending.keys() {
                debug!("overview: window {id} can't stream live");
            }
        }
    }

    /// Starts streaming `filter` at `width` × `height` points into `want`'s
    /// layer.
    fn start(
        &self,
        filter: &SCContentFilter,
        width: f64,
        height: f64,
        want: &Wanted,
    ) -> Option<Stream> {
        let config = unsafe { SCStreamConfiguration::new() };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        unsafe {
            config.setWidth((width * want.scale).round().max(1.0) as usize);
            config.setHeight((height * want.scale).round().max(1.0) as usize);
            config.setMinimumFrameInterval(CMTime::new(1, FPS));
            config.setQueueDepth(4);
            config.setShowsCursor(false);
            config.setIgnoreShadowsSingleWindow(true);
        }
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                filter,
                &config,
                None,
            )
        };
        let output = LiveOutput::new(
            self.mtm,
            want.window_id,
            want.layer.clone(),
            self.showing.clone(),
        );
        let protocol = ProtocolObject::from_ref(&*output);
        if let Err(err) = unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                protocol,
                SCStreamOutputType::Screen,
                Some(DispatchQueue::main()),
            )
        } {
            warn!("overview: live window {}: {err}", want.window_id);
            return None;
        }
        let id = want.window_id;
        let started = RcBlock::new(move |error: *mut NSError| {
            if let Some(error) = unsafe { error.as_ref() } {
                warn!("overview: live window {id} didn't start: {error}");
            }
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
        Some(Stream {
            capture: stream,
            output,
        })
    }

    /// Ends every stream (the overview closed).
    pub fn stop_all(&mut self) {
        let now = Instant::now();
        for (_, stream) in self.streams.drain() {
            stream.stop();
            self.stopping.push((now, stream));
        }
        self.pending.clear();
        self.showing.borrow_mut().clear();
        self.in_flight = false;
        self.request += 1;
    }
}
