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

use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bevy::math::IRect;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSFloatingWindowLevel, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBufferGetIOSurface;
use objc2_foundation::{NSError, NSPoint, NSRect, NSSize};
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
}

impl Drop for PinMirror {
    fn drop(&mut self) {
        self.output.stop();
        self.panel.orderOut(None::<&AnyObject>);
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
                if !mirror.shown {
                    mirror.panel.orderFront(None::<&AnyObject>);
                }
            } else if mirror.shown {
                mirror.panel.orderOut(None::<&AnyObject>);
            }
            mirror.shown = show;
        }
        failed
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
        }
    }
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

    #[test]
    fn pixel_size_scales_and_never_hits_zero() {
        assert_eq!(pixel_size(NSSize::new(400.0, 300.0), 2.0), (800, 600));
        assert_eq!(pixel_size(NSSize::new(0.0, 0.0), 2.0), (1, 1));
    }
}
