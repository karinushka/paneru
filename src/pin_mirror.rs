//! Window pinning (spike): keeps a window visible above all others by showing
//! a live `ScreenCaptureKit` copy of it in a floating panel Paneru owns.
//!
//! macOS gives no public way to change another process's window level, so the
//! real window stays at its normal level. While it is unfocused, a borderless
//! panel at `NSFloatingWindowLevel` sits exactly over it and shows its live
//! frames; clicking the panel focuses and raises the real window, and the ECS
//! side hides the panel while the pinned window has focus.
//!
//! Frames arrive on the main dispatch queue, which the Cocoa pump in
//! `PlatformCallbacks::pump_cocoa_event_loop` services while it waits, so a
//! running stream does not wake the Bevy loop once per frame.

use std::cell::Cell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bevy::math::IRect;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSFloatingWindowLevel, NSImage, NSImageScaling,
    NSImageView, NSNormalWindowLevel, NSScreen, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_core_graphics::CGColor;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBufferGetIOSurface;
use objc2_foundation::{NSError, NSPoint, NSRect, NSSize, NSString, NSTimer};
use objc2_quartz_core::{CALayer, kCAGravityResize};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};
use tracing::{debug, warn};

use crate::events::{Event, EventSender};
use crate::platform::WinID;

unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// `kCVPixelFormatType_32BGRA`.
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// Mirrors don't need display-rate frames; 30fps keeps the capture cheap.
const MIRROR_FPS: i32 = 30;

/// What the ECS side wants on screen for one pinned window.
#[derive(Clone, Debug, PartialEq)]
pub struct PinTarget {
    pub window_id: WinID,
    /// The real window's content rect, CG absolute (y-down), padding stripped.
    pub frame: IRect,
    /// Whether the mirror should cover the real window right now.
    pub visible: bool,
    /// The focused pinned window this mirror overlaps, if any: the mirror is
    /// stacked directly beneath it instead of floating above everything.
    pub under: Option<WinID>,
    /// Whether the pin badge should show on the window's corner right now.
    /// Independent of `visible`: the badge stays while the window has focus.
    pub badge: bool,
    /// Whether `WindowServer` lists the real window as on screen. A badge
    /// needs something under it: the real window or a showing mirror.
    pub drawn: bool,
}

// ── Stream output: receives frames, owns the running stream ─────────────

struct PinStreamIvars {
    window_id: WinID,
    events: EventSender,
    /// The mirror view's backing layer; frames are set as its contents.
    layer: Retained<CALayer>,
    /// Set by the async start once the stream exists, taken by `stop`.
    stream: Mutex<Option<Retained<SCStream>>>,
    /// Set by `stop` so a start still in flight never leaves a stream running.
    cancelled: AtomicBool,
    has_frame: AtomicBool,
    /// Spike diagnostics: frames since the last rate log.
    frames: AtomicU32,
    last_rate_log: Mutex<Instant>,
}

impl PinStreamIvars {
    /// Logs the delivered frame rate about once a second, to tell "no frames
    /// while the pump idles" apart from "frames but the panel is hidden".
    fn log_frame_rate(&self) {
        let frames = self.frames.fetch_add(1, Ordering::Relaxed) + 1;
        let Ok(mut last) = self.last_rate_log.lock() else {
            return;
        };
        let elapsed = last.elapsed();
        if elapsed >= Duration::from_secs(1) {
            debug!(
                window_id = self.window_id,
                fps = f64::from(frames) / elapsed.as_secs_f64(),
                "pin mirror frames"
            );
            self.frames.store(0, Ordering::Relaxed);
            *last = Instant::now();
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "PaneruPinStreamOutput"]
    #[ivars = PinStreamIvars]
    struct PinStreamOutput;

    unsafe impl NSObjectProtocol for PinStreamOutput {}

    unsafe impl SCStreamOutput for PinStreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Idle/blank status frames carry no image buffer.
            let Some(image) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            let Some(surface) = CVPixelBufferGetIOSurface(Some(&image)) else {
                return;
            };
            // `IOSurfaceRef` is toll-free bridged to `IOSurface`, which
            // `CALayer.contents` accepts directly (no copy).
            let surface_ptr: NonNull<AnyObject> =
                objc2_core_foundation::CFRetained::as_ptr(&surface).cast();
            let ivars = self.ivars();
            unsafe { ivars.layer.setContents(Some(surface_ptr.as_ref())) };
            ivars.log_frame_rate();

            if !ivars.has_frame.swap(true, Ordering::AcqRel)
                && let Err(error) = ivars.events.send(Event::PinMirrorReady {
                    window_id: ivars.window_id,
                })
            {
                warn!(%error, "unable to send pin mirror ready");
            }
        }
    }

    unsafe impl SCStreamDelegate for PinStreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            let ivars = self.ivars();
            warn!(
                window_id = ivars.window_id,
                "pin mirror stream stopped: {}",
                error.localizedDescription()
            );
            ivars.stream.lock().ok().and_then(|mut slot| slot.take());
            if let Err(error) = ivars.events.send(Event::PinMirrorStopped {
                window_id: ivars.window_id,
            }) {
                warn!(%error, "unable to send pin mirror stopped");
            }
        }
    }
);

impl PinStreamOutput {
    fn new(window_id: WinID, events: EventSender, layer: Retained<CALayer>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(PinStreamIvars {
            window_id,
            events,
            layer,
            stream: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            has_frame: AtomicBool::new(false),
            frames: AtomicU32::new(0),
            last_rate_log: Mutex::new(Instant::now()),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn has_frame(&self) -> bool {
        self.ivars().has_frame.load(Ordering::Acquire)
    }

    fn send_stopped(&self) {
        let ivars = self.ivars();
        if let Err(error) = ivars.events.send(Event::PinMirrorStopped {
            window_id: ivars.window_id,
        }) {
            warn!(%error, "unable to send pin mirror stopped");
        }
    }

    /// Looks the window up in `ScreenCaptureKit` and starts streaming it.
    /// Asynchronous: the shareable-content callback runs on a private queue.
    fn start(this: &Retained<Self>, pixel_size: (usize, usize)) {
        let output = this.clone();
        let window_id = this.ivars().window_id;
        let handler = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                if output.ivars().cancelled.load(Ordering::Acquire) {
                    return;
                }
                let Some(content) = (unsafe { content.as_ref() }) else {
                    let reason = unsafe { error.as_ref() }
                        .map(|error| error.localizedDescription().to_string());
                    warn!(window_id, ?reason, "pin mirror: no shareable content");
                    output.send_stopped();
                    return;
                };
                let windows = unsafe { content.windows() };
                let Some(sc_window) = windows.iter().find(|window| {
                    WinID::try_from(unsafe { window.windowID() }).ok() == Some(window_id)
                }) else {
                    warn!(window_id, "pin mirror: window not capturable");
                    output.send_stopped();
                    return;
                };

                let filter = unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &sc_window,
                    )
                };
                let config = stream_configuration(pixel_size);
                let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
                let stream = unsafe {
                    SCStream::initWithFilter_configuration_delegate(
                        SCStream::alloc(),
                        &filter,
                        &config,
                        Some(delegate),
                    )
                };
                let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
                if let Err(error) = unsafe {
                    stream.addStreamOutput_type_sampleHandlerQueue_error(
                        sink,
                        SCStreamOutputType::Screen,
                        Some(DispatchQueue::main()),
                    )
                } {
                    warn!(
                        window_id,
                        "pin mirror: adding stream output: {}",
                        error.localizedDescription()
                    );
                    output.send_stopped();
                    return;
                }
                unsafe { stream.startCaptureWithCompletionHandler(None) };

                if let Ok(mut slot) = output.ivars().stream.lock() {
                    *slot = Some(stream);
                }
                // `stop` may have run between the check above and now.
                if output.ivars().cancelled.load(Ordering::Acquire) {
                    output.stop();
                }
                debug!(window_id, "pin mirror: capture started");
            },
        );
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, false, &handler,
            );
        }
    }

    fn resize(&self, pixel_size: (usize, usize)) {
        let Ok(slot) = self.ivars().stream.lock() else {
            return;
        };
        if let Some(stream) = slot.as_ref() {
            unsafe {
                stream
                    .updateConfiguration_completionHandler(&stream_configuration(pixel_size), None);
            }
        }
    }

    fn stop(&self) {
        self.ivars().cancelled.store(true, Ordering::Release);
        let stream = self
            .ivars()
            .stream
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(stream) = stream {
            // Keep the stream alive until `ScreenCaptureKit` confirms the stop.
            let keep_alive = stream.clone();
            let done = RcBlock::new(move |_error: *mut NSError| {
                let _ = &keep_alive;
            });
            unsafe { stream.stopCaptureWithCompletionHandler(Some(&done)) };
        }
    }
}

fn stream_configuration((width, height): (usize, usize)) -> Retained<SCStreamConfiguration> {
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(width.max(1));
        config.setHeight(height.max(1));
        config.setMinimumFrameInterval(CMTime::new(1, MIRROR_FPS));
        config.setPixelFormat(PIXEL_FORMAT_BGRA);
        config.setQueueDepth(3);
        config.setShowsCursor(false);
        config
    }
}

// ── Mirror view: shows frames, turns a click into a focus request ───────

struct MirrorViewIvars {
    window_id: WinID,
    events: EventSender,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruPinMirrorView"]
    #[ivars = MirrorViewIvars]
    struct MirrorView;

    impl MirrorView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: &NSEvent) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            let ivars = self.ivars();
            if let Err(error) = ivars.events.send(Event::PinMirrorClicked {
                window_id: ivars.window_id,
            }) {
                warn!(%error, "unable to send pin mirror click");
            }
        }
    }
);

impl MirrorView {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        window_id: WinID,
        events: EventSender,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MirrorViewIvars { window_id, events });
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.setWantsLayer(true);
        view
    }
}

// ── Manager ─────────────────────────────────────────────────────────────

struct PinMirror {
    panel: Retained<NSWindow>,
    output: Retained<PinStreamOutput>,
    pixel_size: (usize, usize),
    shown: bool,
    /// The window the panel was last stacked beneath (see `PinTarget::under`).
    under: Option<WinID>,
    /// Bumped whenever the panel is restacked or hidden, so settling timers
    /// scheduled before that do nothing.
    stack_epoch: Rc<Cell<u64>>,
    /// Small click-through panel carrying the pin glyph. Separate from
    /// `panel` because that one hides while the pinned window has focus.
    badge: Retained<NSWindow>,
    /// Whether `badge` is currently a child window of `panel`.
    badge_attached: bool,
}

impl PinMirror {
    /// Shows, moves or hides the badge.
    ///
    /// While the mirror is showing, the badge is its child window: it stacks
    /// directly above the mirror, so another pin's mirror that covers this
    /// one covers the badge too. With the mirror hidden (the real window is
    /// what is on screen) the badge stands alone, one level above the mirrors.
    fn sync_badge(&mut self, visible: bool, frame: NSRect) {
        let attach = visible && self.shown;
        if !attach {
            self.detach_badge();
        }
        if !visible {
            self.badge.orderOut(None::<&AnyObject>);
            return;
        }
        self.badge.setFrame_display(frame, false);
        if !attach {
            self.badge.setLevel(NSFloatingWindowLevel + 1);
            self.badge.orderFront(None::<&AnyObject>);
        } else if !self.badge_attached {
            self.badge.setLevel(self.panel.level());
            // Both windows are ours and outlive the relationship: `Drop`
            // detaches the badge before either goes away.
            unsafe {
                self.panel
                    .addChildWindow_ordered(&self.badge, NSWindowOrderingMode::Above);
            }
            self.badge_attached = true;
        }
    }

    fn detach_badge(&mut self) {
        if self.badge_attached {
            self.panel.removeChildWindow(&self.badge);
            self.badge_attached = false;
        }
    }

    /// Puts the mirror on screen at the right depth: floating above every
    /// real window, or (`under`) at normal level directly beneath the focused
    /// pinned window, so it neither buries that window nor drops behind the
    /// tiled ones. The badge is detached first and re-attached by
    /// `sync_badge`, so it follows the mirror to its new level.
    ///
    /// `settle` is set when the mirror's place changed (as opposed to being
    /// re-asserted): a tucked mirror then also re-asserts itself a few times
    /// over the next half second, see `schedule_settling`.
    fn restack(&mut self, under: Option<WinID>, settle: bool) {
        self.detach_badge();
        if settle {
            self.cancel_settling();
        }
        if let Some(window_number) = under.and_then(|window_id| isize::try_from(window_id).ok()) {
            self.panel.setLevel(NSNormalWindowLevel);
            self.panel
                .orderWindow_relativeTo(NSWindowOrderingMode::Below, window_number);
            if settle {
                self.schedule_settling(window_number);
            }
        } else {
            self.panel.setLevel(NSFloatingWindowLevel);
            self.panel.orderFront(None::<&AnyObject>);
        }
    }

    fn hide(&mut self) {
        self.cancel_settling();
        self.panel.orderOut(None::<&AnyObject>);
    }

    /// Makes every pending settling timer a no-op.
    fn cancel_settling(&self) {
        self.stack_epoch.set(self.stack_epoch.get().wrapping_add(1));
    }

    /// Re-asserts "directly beneath `window_number`" at `SETTLE_DELAYS`.
    ///
    /// Focusing a window can bring its app's other windows forward a moment
    /// later (observed: a tiled window of the same app rising above the
    /// tucked mirror), and nothing reports that to Paneru. The retries are
    /// bounded and one-shot, and each is a no-op once the mirror has been
    /// restacked or hidden since (`stack_epoch`).
    fn schedule_settling(&self, window_number: isize) {
        let epoch = self.stack_epoch.get();
        for delay in SETTLE_DELAYS {
            let panel = self.panel.clone();
            let current = Rc::clone(&self.stack_epoch);
            let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
                if current.get() == epoch {
                    panel.orderWindow_relativeTo(NSWindowOrderingMode::Below, window_number);
                }
            });
            unsafe {
                NSTimer::scheduledTimerWithTimeInterval_repeats_block(delay, false, &block);
            }
        }
    }
}

/// When a freshly tucked mirror re-asserts its stacking, in seconds.
const SETTLE_DELAYS: [f64; 3] = [0.06, 0.2, 0.5];

impl Drop for PinMirror {
    fn drop(&mut self) {
        self.output.stop();
        self.detach_badge();
        self.hide();
        self.badge.orderOut(None::<&AnyObject>);
    }
}

/// One mirror panel and capture stream per pinned window.
pub struct PinMirrorManager {
    mtm: MainThreadMarker,
    events: EventSender,
    mirrors: HashMap<WinID, PinMirror>,
    permission_requested: bool,
}

impl PinMirrorManager {
    pub fn new(mtm: MainThreadMarker, events: EventSender) -> Self {
        Self {
            mtm,
            events,
            mirrors: HashMap::new(),
            permission_requested: false,
        }
    }

    /// Brings the mirrors in line with `targets`: starts capture for new pins,
    /// tears down mirrors whose pin is gone, and moves/shows/hides the rest.
    ///
    /// Returns the windows that could not be mirrored (no Screen Recording
    /// permission), so the caller can drop their pin instead of retrying.
    pub fn sync(&mut self, targets: &[PinTarget]) -> Vec<WinID> {
        self.mirrors
            .retain(|id, _| targets.iter().any(|target| target.window_id == *id));

        let mut failed = Vec::new();
        if targets.is_empty() {
            return failed;
        }
        let screen_h = primary_screen_height(self.mtm);
        let scale = backing_scale(self.mtm);

        for target in targets {
            let frame = cg_abs_to_cocoa(target.frame, screen_h);
            let pixel_size = pixel_size(frame.size, scale);

            if !self.mirrors.contains_key(&target.window_id) {
                if !self.screen_capture_allowed() {
                    failed.push(target.window_id);
                    continue;
                }
                let mirror = self.create_mirror(target.window_id, frame, pixel_size);
                self.mirrors.insert(target.window_id, mirror);
            }
            let Some(mirror) = self.mirrors.get_mut(&target.window_id) else {
                continue;
            };

            if mirror.pixel_size != pixel_size {
                mirror.pixel_size = pixel_size;
                mirror.output.resize(pixel_size);
            }
            // Never show an empty panel: at floating level it would swallow
            // clicks meant for the windows underneath.
            let show = target.visible && mirror.output.has_frame();
            if show {
                mirror.panel.setFrame_display(frame, false);
                // A tucked mirror is re-stacked on every sync: its place is
                // relative to another app's window, which may have been
                // raised since the last one.
                let moved = !mirror.shown || mirror.under != target.under;
                if moved || target.under.is_some() {
                    mirror.restack(target.under, moved);
                }
            } else if mirror.shown {
                mirror.hide();
            }
            mirror.shown = show;
            mirror.under = target.under;

            mirror.sync_badge(badge_visible(target, show), badge_frame(frame));
        }
        failed
    }

    /// Whether any mirror (and so any badge) is still alive.
    pub fn has_mirrors(&self) -> bool {
        !self.mirrors.is_empty()
    }

    fn screen_capture_allowed(&mut self) -> bool {
        if unsafe { CGPreflightScreenCaptureAccess() } {
            return true;
        }
        if !self.permission_requested {
            self.permission_requested = true;
            // Shows the system prompt once; TCC needs a relaunch to apply it.
            unsafe { CGRequestScreenCaptureAccess() };
        }
        warn!("pinning needs Screen Recording permission");
        false
    }

    fn create_mirror(
        &self,
        window_id: WinID,
        frame: NSRect,
        pixel_size: (usize, usize),
    ) -> PinMirror {
        let panel = make_mirror_panel(self.mtm, frame);
        let view = MirrorView::new(
            self.mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), frame.size),
            window_id,
            self.events.clone(),
        );
        panel.setContentView(Some(&view));
        let layer = view.layer().unwrap_or_else(|| {
            let layer = CALayer::new();
            view.setLayer(Some(&layer));
            layer
        });
        unsafe { layer.setContentsGravity(kCAGravityResize) };

        let output = PinStreamOutput::new(window_id, self.events.clone(), layer);
        PinStreamOutput::start(&output, pixel_size);
        PinMirror {
            panel,
            output,
            pixel_size,
            shown: false,
            under: None,
            stack_epoch: Rc::new(Cell::new(0)),
            badge: make_badge_panel(self.mtm, badge_frame(frame)),
            badge_attached: false,
        }
    }
}

/// Side of the square pin badge, in points.
const BADGE_SIZE: f64 = 20.0;
/// How far the badge hangs outside the window's top-right corner, so it
/// covers as little of the app's own toolbar as possible.
const BADGE_OVERHANG: f64 = 6.0;
/// Padding between the badge's edge and its glyph.
const BADGE_GLYPH_INSET: f64 = 4.0;

/// A wanted badge shows only over something: the real window as
/// `WindowServer` draws it, or its mirror when that is what is on screen
/// (e.g. the window lives on another Space). Never over a closed window.
fn badge_visible(target: &PinTarget, mirror_shown: bool) -> bool {
    target.badge && (target.drawn || mirror_shown)
}

/// The badge's Cocoa frame for a window at `window` (Cocoa, y-up): pinned to
/// the top-right corner, overhanging it by `BADGE_OVERHANG`.
fn badge_frame(window: NSRect) -> NSRect {
    NSRect::new(
        NSPoint::new(
            window.origin.x + window.size.width - BADGE_SIZE + BADGE_OVERHANG,
            window.origin.y + window.size.height - BADGE_SIZE + BADGE_OVERHANG,
        ),
        NSSize::new(BADGE_SIZE, BADGE_SIZE),
    )
}

/// A round, click-through badge with a pin glyph. It sits one level above the
/// mirror panels so a mirror never covers its own badge.
fn make_badge_panel(mtm: MainThreadMarker, cocoa_frame: NSRect) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            cocoa_frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setIgnoresMouseEvents(true);
    window.setHasShadow(false);
    window.setLevel(NSFloatingWindowLevel + 1);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );

    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), cocoa_frame.size);
    let view = NSView::initWithFrame(NSView::alloc(mtm), bounds);
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setBackgroundColor(Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 0.65)));
        layer.setCornerRadius(BADGE_SIZE / 2.0);
    }

    // SF Symbols need macOS 11; without the glyph the dark dot still marks it.
    if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str("pin.fill"),
        None,
    ) {
        image.setTemplate(true);
        let glyph = NSImageView::new(mtm);
        glyph.setImage(Some(&image));
        glyph.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        glyph.setContentTintColor(Some(&*NSColor::whiteColor()));
        let side = BADGE_SIZE - 2.0 * BADGE_GLYPH_INSET;
        glyph.setFrame(NSRect::new(
            NSPoint::new(BADGE_GLYPH_INSET, BADGE_GLYPH_INSET),
            NSSize::new(side, side),
        ));
        view.addSubview(&glyph);
    }
    window.setContentView(Some(&view));
    window
}

fn make_mirror_panel(mtm: MainThreadMarker, cocoa_frame: NSRect) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            cocoa_frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setIgnoresMouseEvents(false);
    // The captured frame already includes the window's own shadow-free
    // content; a panel shadow would double up with the real one.
    window.setHasShadow(false);
    window.setLevel(NSFloatingWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    window
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "window sizes are small positive pixel counts"
)]
fn pixel_size(size: NSSize, scale: f64) -> (usize, usize) {
    (
        (size.width * scale).round().max(1.0) as usize,
        (size.height * scale).round().max(1.0) as usize,
    )
}

fn backing_scale(mtm: MainThreadMarker) -> f64 {
    NSScreen::mainScreen(mtm).map_or(2.0, |screen| screen.backingScaleFactor())
}

fn cg_abs_to_cocoa(frame: IRect, primary_screen_height: f64) -> NSRect {
    let x = f64::from(frame.min.x);
    let y = f64::from(frame.min.y);
    let width = f64::from(frame.width());
    let height = f64::from(frame.height());
    NSRect::new(
        NSPoint::new(x, primary_screen_height - y - height),
        NSSize::new(width, height),
    )
}

/// Height of the display at the global origin, which anchors the CG↔Cocoa
/// Y-flip (see `overlay::primary_screen_height`).
fn primary_screen_height(mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    let mut fallback = None;
    for screen in &screens {
        let frame = screen.frame();
        fallback.get_or_insert(frame.size.height);
        if frame.origin.x == 0.0 && frame.origin.y == 0.0 {
            return frame.size.height;
        }
    }
    fallback.unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cg_to_cocoa_flips_y_against_primary_height() {
        let frame = IRect::new(100, 50, 500, 350);
        let cocoa = cg_abs_to_cocoa(frame, 1000.0);
        assert_eq!(cocoa.origin.x, 100.0);
        assert_eq!(cocoa.origin.y, 1000.0 - 50.0 - 300.0);
        assert_eq!(cocoa.size.width, 400.0);
        assert_eq!(cocoa.size.height, 300.0);
    }

    /// objc2 checks method type encodings when a class registers (debug
    /// builds); a mismatch would otherwise panic the daemon on first pin.
    #[test]
    fn mirror_classes_register_with_valid_encodings() {
        let (events, _receiver) = EventSender::new();
        let output = PinStreamOutput::new(0, events, CALayer::new());
        assert!(!output.has_frame());
        output.stop();
        let _ = <MirrorView as objc2::ClassType>::class();
    }

    /// The badge hugs the top-right corner (Cocoa is y-up, so "top" is the
    /// max-y edge) and overhangs it by `BADGE_OVERHANG` on both axes.
    #[test]
    fn badge_sits_on_the_top_right_corner() {
        let window = NSRect::new(NSPoint::new(100.0, 200.0), NSSize::new(400.0, 300.0));
        let badge = badge_frame(window);
        assert_eq!(badge.size.width, BADGE_SIZE);
        assert_eq!(badge.size.height, BADGE_SIZE);
        assert_eq!(badge.origin.x + BADGE_SIZE, 500.0 + BADGE_OVERHANG);
        assert_eq!(badge.origin.y + BADGE_SIZE, 500.0 + BADGE_OVERHANG);
    }

    /// A badge never floats over nothing: it needs the real window drawn or
    /// its mirror showing, and is never shown when it was not asked for.
    #[test]
    fn badge_needs_a_window_or_mirror_under_it() {
        let target = |badge, drawn| PinTarget {
            window_id: 1,
            frame: IRect::new(0, 0, 10, 10),
            visible: false,
            under: None,
            badge,
            drawn,
        };
        assert!(badge_visible(&target(true, true), false));
        assert!(badge_visible(&target(true, false), true));
        assert!(
            !badge_visible(&target(true, false), false),
            "closed or ordered-out window: no badge"
        );
        assert!(!badge_visible(&target(false, true), true));
    }

    #[test]
    fn pixel_size_scales_and_never_hits_zero() {
        assert_eq!(pixel_size(NSSize::new(400.0, 300.0), 2.0), (800, 600));
        assert_eq!(pixel_size(NSSize::new(0.0, 0.0), 2.0), (1, 1));
    }
}
