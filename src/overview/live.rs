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
//! even windows on hidden rows stream. On its own, though, a translucent
//! window's blurred backdrop comes out flat grey, so a window fully on
//! screen streams its region of the display instead (everything but
//! paneru's own windows), which shows the real blur.
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
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBufferGetIOSurface;
use objc2_foundation::{NSArray, NSError};
use objc2_quartz_core::{CALayer, CATransaction};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};
use tracing::{debug, warn};

use super::Rect;
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
    /// Stream this part of the screen (absolute CG) instead of the window
    /// alone; see the module docs.
    pub region: Option<Rect>,
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
    region: Option<Rect>,
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
    tx: Sender<(u64, usize)>,
    rx: Receiver<(u64, usize)>,
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
            tx,
            rx,
            showing: Rc::new(RefCell::new(HashSet::new())),
        }
    }

    /// Whether `window_id` already shows live frames.
    pub fn is_showing(&self, window_id: i32) -> bool {
        self.showing.borrow().contains(&window_id)
    }

    /// Streams exactly `wanted`: running streams move to their new layer,
    /// the rest stop, and new ones start once `ScreenCaptureKit` has
    /// listed the windows (asynchronously; see [`Live::poll`]).
    pub fn set(&mut self, wanted: Vec<Wanted>) {
        let keep: HashMap<i32, Option<Rect>> =
            wanted.iter().map(|w| (w.window_id, w.region)).collect();
        let now = Instant::now();
        self.stopping
            .retain(|(since, _)| now.duration_since(*since) < STOP_GRACE);
        for (id, stream) in std::mem::take(&mut self.streams) {
            if keep.get(&id) == Some(&stream.region) {
                self.streams.insert(id, stream);
            } else {
                stream.stop();
                self.stopping.push((now, stream));
            }
        }
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
            return;
        }
        self.request += 1;
        let (request, tx, wake) = (self.request, self.tx.clone(), self.sender.clone());
        let handler = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                if let Some(error) = unsafe { error.as_ref() } {
                    warn!("overview: listing windows for live previews: {error}");
                }
                // Retained here, released on the main thread in `poll`.
                let addr = if content.is_null() {
                    0
                } else {
                    let content: Option<Retained<SCShareableContent>> =
                        unsafe { Retained::retain(content) };
                    content.map_or(0, |c| Retained::into_raw(c) as usize)
                };
                if let Err(err) = tx.send((request, addr)) {
                    release(err.0.1);
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

    /// Whether a window listing is still in flight.
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Starts the streams of a window listing that came back.
    pub fn poll(&mut self) {
        while let Ok((request, addr)) = self.rx.try_recv() {
            let content = release(addr);
            if request != self.request {
                continue;
            }
            let pending = std::mem::take(&mut self.pending);
            let Some(content) = content else {
                continue;
            };
            // Region streams leave out paneru's own windows: the overview
            // panels on top, and the borders.
            let pid = i32::try_from(std::process::id()).unwrap_or_default();
            let ours: Vec<_> = unsafe { content.applications() }
                .iter()
                .filter(|app| unsafe { app.processID() } == pid)
                .collect();
            let ours = NSArray::from_retained_slice(&ours);
            let displays = unsafe { content.displays() };
            let windows = unsafe { content.windows() };
            for want in pending.values().filter(|want| want.region.is_some()) {
                let Some(region) = want.region else { continue };
                let display = displays.iter().find(|display| {
                    let f = unsafe { display.frame() };
                    Rect::new(f.origin.x, f.origin.y, f.size.width, f.size.height).encloses(&region)
                });
                let Some(display) = display else { continue };
                let filter = unsafe {
                    SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                        SCContentFilter::alloc(),
                        &display,
                        &ours,
                        &NSArray::new(),
                    )
                };
                let origin = unsafe { display.frame() }.origin;
                let source = CGRect::new(
                    CGPoint::new(region.x - origin.x, region.y - origin.y),
                    CGSize::new(region.w, region.h),
                );
                if let Some(stream) = self.start(&filter, region.w, region.h, Some(source), want) {
                    self.streams.insert(want.window_id, stream);
                }
            }
            for window in &windows {
                let Ok(id) = i32::try_from(unsafe { window.windowID() }) else {
                    continue;
                };
                let Some(want) = pending.get(&id).filter(|want| want.region.is_none()) else {
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
                    self.start(&filter, frame.size.width, frame.size.height, None, want)
                {
                    self.streams.insert(id, stream);
                }
            }
            for id in pending.keys().filter(|id| !self.streams.contains_key(id)) {
                debug!("overview: window {id} can't stream live");
            }
        }
    }

    /// Starts streaming `filter` (cut to `source`, in display points, if
    /// given) at `width` × `height` points into `want`'s layer.
    fn start(
        &self,
        filter: &SCContentFilter,
        width: f64,
        height: f64,
        source: Option<CGRect>,
        want: &Wanted,
    ) -> Option<Stream> {
        let config = unsafe { SCStreamConfiguration::new() };
        if let Some(source) = source {
            unsafe { config.setSourceRect(source) };
        }
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
            region: want.region,
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
        self.request += 1;
    }
}

fn release(addr: usize) -> Option<Retained<SCShareableContent>> {
    unsafe { Retained::from_raw(addr as *mut SCShareableContent) }
}
