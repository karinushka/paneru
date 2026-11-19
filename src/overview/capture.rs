//! `WindowServer` metadata queries, background window/region/wallpaper snapshot
//! captures, and Core Image backdrop blurring for the overview.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{AllocAnyThread, MainThreadMarker, class, msg_send};
use objc2_app_kit::{NSImage, NSScreen};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGImage, CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID,
};
use objc2_foundation::{NSArray, NSRect, NSSize, NSString, ns_string};

use super::Rect;
use super::layers::{ICON_SIZE, ns_number};
use crate::events::{Event, EventSender};
use crate::platform::Pid;

/// Blur of the wallpaper backdrop, in points.
pub(super) const BACKDROP_BLUR: f64 = 40.0;

unsafe extern "C" {
    // Deprecated (and hidden from the SDK headers) since macOS 14 in favour of
    // ScreenCaptureKit, but still exported and working.
    fn CGWindowListCreateImage(
        screen_bounds: CGRect,
        list_option: u32,
        window_id: u32,
        image_option: u32,
    ) -> *mut CGImage;
}

const LIST_ON_SCREEN_ONLY: u32 = 1 << 0;
const LIST_ON_SCREEN_BELOW_WINDOW: u32 = 1 << 2;
const LIST_INCLUDING_WINDOW: u32 = 1 << 3;
const IMAGE_BOUNDS_IGNORE_FRAMING: u32 = 1 << 0;
const IMAGE_BEST_RESOLUTION: u32 = 1 << 3;
const IMAGE_NOMINAL_RESOLUTION: u32 = 1 << 4;

fn snapshot(window_id: i32, best: bool) -> Option<CFRetained<CGImage>> {
    let null = CGRect::new(
        CGPoint::new(f64::INFINITY, f64::INFINITY),
        CGSize::new(0.0, 0.0),
    );
    let resolution = if best {
        IMAGE_BEST_RESOLUTION
    } else {
        IMAGE_NOMINAL_RESOLUTION
    };
    let id = u32::try_from(window_id).ok()?;
    let raw = unsafe {
        CGWindowListCreateImage(
            null,
            LIST_INCLUDING_WINDOW,
            id,
            IMAGE_BOUNDS_IGNORE_FRAMING | resolution,
        )
    };
    std::ptr::NonNull::new(raw).map(|ptr| unsafe { CFRetained::from_raw(ptr) })
}

/// Everything on screen inside `rect` (absolute CG), composited — e.g. the
/// menu bar together with every status item, which are separate windows.
/// With `below`, only what sits under that window (our own panel).
fn snapshot_region(rect: &Rect, below: Option<i32>) -> Option<CFRetained<CGImage>> {
    let bounds = CGRect::new(CGPoint::new(rect.x, rect.y), CGSize::new(rect.w, rect.h));
    let (option, window) = match below.and_then(|id| u32::try_from(id).ok()) {
        Some(id) => (LIST_ON_SCREEN_BELOW_WINDOW, id),
        None => (LIST_ON_SCREEN_ONLY, 0),
    };
    let raw = unsafe { CGWindowListCreateImage(bounds, option, window, IMAGE_BEST_RESOLUTION) };
    std::ptr::NonNull::new(raw).map(|ptr| unsafe { CFRetained::from_raw(ptr) })
}

/// Decodes an image file (any format `NSImage` reads, HEIC included).
/// Runs on a capture worker: wallpapers are often 6K and slow to decode.
fn image_file(path: &str) -> Option<CFRetained<CGImage>> {
    let path = NSString::from_str(path);
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &path)?;
    cg_image(&image)
}

/// The best representation of `image` as a `CGImage`.
fn cg_image(image: &NSImage) -> Option<CFRetained<CGImage>> {
    let raw: *mut CGImage = unsafe {
        msg_send![
            image,
            CGImageForProposedRect: std::ptr::null_mut::<NSRect>(),
            context: std::ptr::null::<AnyObject>(),
            hints: std::ptr::null::<AnyObject>()
        ]
    };
    // Autoreleased: retain it for ourselves.
    std::ptr::NonNull::new(raw).map(|ptr| unsafe { CFRetained::retain(ptr) })
}

/// The desktop picture file of display `display_id`, if it has one (a
/// solid colour or a dynamic wallpaper may not).
pub(super) fn desktop_picture(mtm: MainThreadMarker, display_id: u32) -> Option<String> {
    let key = ns_string!("NSScreenNumber");
    let screen = NSScreen::screens(mtm).into_iter().find(|screen| {
        let description = screen.deviceDescription();
        let number: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*description, objectForKey: key] };
        number.is_some_and(|n| {
            let id: u32 = unsafe { msg_send![&*n, unsignedIntValue] };
            id == display_id
        })
    })?;
    let workspace: Retained<AnyObject> = unsafe { msg_send![class!(NSWorkspace), sharedWorkspace] };
    let url: Option<Retained<AnyObject>> =
        unsafe { msg_send![&*workspace, desktopImageURLForScreen: &*screen] };
    let path: Option<Retained<NSString>> = unsafe { msg_send![&*url?, path] };
    Some(path?.to_string())
}

/// The icon of the app with process id `pid`.
pub(super) fn app_icon(pid: Pid) -> Option<CFRetained<CGImage>> {
    let app: Option<Retained<AnyObject>> = unsafe {
        msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid
        ]
    };
    let icon: Option<Retained<NSImage>> = unsafe { msg_send![&*app?, icon] };
    let icon = icon?;
    // Ask for a large representation; the default is 32 points.
    let size = NSSize::new(ICON_SIZE * 4.0, ICON_SIZE * 4.0);
    icon.setSize(size);
    cg_image(&icon)
}

/// One capture to run off the main thread.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(super) enum Shot {
    Window {
        id: i32,
        best: bool,
    },
    /// A composited screen region, optionally from below one of our panels.
    Region {
        rect: Rect,
        below: Option<i32>,
    },
    /// An image file (the desktop picture, when windows can't be captured),
    /// blurred by `(width, radius)` like [`Shot::Blurred`] if given.
    File {
        path: String,
        blur: Option<(f64, f64)>,
    },
    /// The wallpaper window, blurred by `radius` points on a display
    /// `width` points wide.
    Blurred {
        id: i32,
        width: f64,
        radius: f64,
    },
}

impl Shot {
    fn take(self) -> Option<CFRetained<CGImage>> {
        match self {
            Self::File { path, blur } => {
                let image = image_file(&path)?;
                match blur {
                    Some((width, radius)) => blurred(&image, width, radius),
                    None => Some(image),
                }
            }
            Self::Window { id, best } => snapshot(id, best),
            Self::Region { rect, below } => snapshot_region(&rect, below),
            Self::Blurred { id, width, radius } => {
                snapshot(id, true).and_then(|image| blurred(&image, width, radius))
            }
        }
    }
}

pub(super) struct SendImage(pub(super) CFRetained<CGImage>);

// SAFETY: `CGImage` is immutable and safe to send across threads.
unsafe impl Send for SendImage {}

/// A capture result: (batch, target index, captured `CGImage`).
pub(super) type Landed = (u64, usize, Option<SendImage>);

/// How many captures run at once. Each is a `WindowServer` round trip; a
/// few in parallel overlap well, dozens just queue up inside it and land
/// all together.
const CAPTURE_WORKERS: usize = 4;

/// Runs `shots` in order on [`AsyncComputeTaskPool`](bevy::tasks::AsyncComputeTaskPool)
/// with at most [`CAPTURE_WORKERS`] concurrent workers. Each result is sent
/// back on `tx`, and `wake` gets an event so the main loop picks it up right
/// away. Workers stop early once `current` moves past `batch`.
pub(super) fn spawn_shots(
    tx: &Sender<Landed>,
    wake: &EventSender,
    current: &Arc<AtomicU64>,
    batch: u64,
    shots: Vec<(usize, Shot)>,
) {
    let workers = shots.len().min(CAPTURE_WORKERS);
    if workers == 0 {
        return;
    }
    let pool = bevy::tasks::AsyncComputeTaskPool::get_or_init(bevy::tasks::TaskPool::new);
    let queue = Arc::new(Mutex::new(VecDeque::from(shots)));
    for _ in 0..workers {
        let (tx, wake, current, queue) = (tx.clone(), wake.clone(), current.clone(), queue.clone());
        pool.spawn(async move {
            loop {
                if current.load(Ordering::Relaxed) != batch {
                    return;
                }
                let job = queue.lock().ok().and_then(|mut queue| queue.pop_front());
                let Some((index, shot)) = job else {
                    return;
                };
                // Task-pool threads have no CFRunLoop to drain autoreleased
                // CoreImage/AppKit temporaries (`CIImage`, `CIFilter`, `NSImage`).
                let image = objc2::rc::autoreleasepool(|_| shot.take()).map(SendImage);
                if tx.send((batch, index, image)).is_err() {
                    return;
                }
                _ = wake.send(Event::OverviewCaptureLanded);
            }
        })
        .detach();
    }
}

/// What the `WindowServer` says is on screen right now, in absolute CG
/// coordinates. Its frames are what the snapshots are cut to, so they are
/// the exact place a card has to start from to cover its window.
#[derive(Default)]
pub struct WindowServerInfo {
    pub frames: HashMap<i32, Rect>,
    /// Windows listed as on screen that draw nothing: fully transparent
    /// (apps such as Chromium park popovers this way) or with no area.
    pub invisible: HashSet<i32>,
    /// Dock's per-display wallpaper windows.
    pub(super) wallpapers: Vec<(i32, Rect)>,
    /// The `WindowServer`'s per-display menu bars.
    pub(super) menubars: Vec<(i32, Rect)>,
}

fn get(dict: &AnyObject, key: &NSString) -> Option<Retained<AnyObject>> {
    unsafe { msg_send![dict, objectForKey: key] }
}

fn number(object: &AnyObject) -> f64 {
    unsafe { msg_send![object, doubleValue] }
}

pub fn window_server_info() -> WindowServerInfo {
    let mut info = WindowServerInfo::default();
    let Some(list) =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, kCGNullWindowID)
    else {
        return info;
    };
    // SAFETY: a CFArray of CFDictionary is toll-free bridged to NSArray.
    let array = unsafe { &*(&raw const *list).cast::<NSArray<AnyObject>>() };
    for entry in array {
        let Some(id) = get(&entry, ns_string!("kCGWindowNumber")).map(|n| number(&n)) else {
            continue;
        };
        let Some(bounds) = get(&entry, ns_string!("kCGWindowBounds")) else {
            continue;
        };
        let field = |key| get(&bounds, key).map_or(0.0, |n| number(&n));
        let frame = Rect::new(
            field(ns_string!("X")),
            field(ns_string!("Y")),
            field(ns_string!("Width")),
            field(ns_string!("Height")),
        );
        #[allow(clippy::cast_possible_truncation)]
        let id = id as i32;
        let text = |key| get(&entry, key).and_then(|s| s.downcast::<NSString>().ok());
        let owner = text(ns_string!("kCGWindowOwnerName"));
        if owner.as_deref() == Some(ns_string!("Dock"))
            && text(ns_string!("kCGWindowName")).is_some_and(|name| unsafe {
                msg_send![&*name, hasPrefix: ns_string!("Wallpaper-")]
            })
        {
            info.wallpapers.push((id, frame));
        } else if owner.as_deref() == Some(ns_string!("Window Server"))
            && text(ns_string!("kCGWindowName")).as_deref() == Some(ns_string!("Menubar"))
        {
            info.menubars.push((id, frame));
        } else {
            let alpha = get(&entry, ns_string!("kCGWindowAlpha")).map_or(1.0, |n| number(&n));
            if alpha <= 0.0 || frame.w < 1.0 || frame.h < 1.0 {
                info.invisible.insert(id);
            }
            info.frames.insert(id, frame);
        }
    }
    info
}

/// A Core Image filter by name with its inputs set.
fn ci_filter(name: &NSString, inputs: &[(&NSString, &AnyObject)]) -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"CIFilter")?;
    let filter: Option<Retained<AnyObject>> = unsafe { msg_send![class, filterWithName: name] };
    let filter = filter?;
    for (key, value) in inputs {
        let _: () = unsafe { msg_send![&*filter, setValue: *value, forKey: *key] };
    }
    Some(filter)
}

struct SendCiContext(Retained<AnyObject>);

// SAFETY: Apple documents `CIContext` as immutable and thread-safe across
// concurrent `createCGImage:fromRect:` evaluations.
unsafe impl Send for SendCiContext {}
unsafe impl Sync for SendCiContext {}

fn shared_ci_context() -> Option<&'static AnyObject> {
    static CONTEXT: std::sync::LazyLock<Option<SendCiContext>> = std::sync::LazyLock::new(|| {
        objc2::rc::autoreleasepool(|_| {
            let ci_context = AnyClass::get(c"CIContext")?;
            let context: Option<Retained<AnyObject>> =
                unsafe { msg_send![ci_context, contextWithOptions: std::ptr::null::<AnyObject>()] };
            context.map(SendCiContext)
        })
    });
    CONTEXT.as_ref().map(|c| &*c.0)
}

/// Blurs `image` (a display-wide capture, `width` points across) by
/// `radius` points, for the backdrop. Rendered once on the CPU side at a quarter of the
/// size: a blurred image has no detail to lose, and the result is cheap to
/// show in many layers. Edges are clamped so they don't darken.
fn blurred(image: &CGImage, width: f64, radius: f64) -> Option<CFRetained<CGImage>> {
    const SCALE: f64 = 0.25;
    objc2::rc::autoreleasepool(|_| {
        #[allow(clippy::cast_precision_loss)]
        let pixels_per_point = CGImage::width(Some(image)) as f64 / width.max(1.0);
        let ci_image = AnyClass::get(c"CIImage")?;
        let input: Option<Retained<AnyObject>> =
            unsafe { msg_send![ci_image, imageWithCGImage: image] };
        let input = input?;
        let scale = ns_number(SCALE);
        let aspect = ns_number(1.0);
        let small = ci_filter(
            ns_string!("CILanczosScaleTransform"),
            &[
                (ns_string!("inputImage"), &input),
                (ns_string!("inputScale"), &scale),
                (ns_string!("inputAspectRatio"), &aspect),
            ],
        )?;
        let small: Option<Retained<AnyObject>> = unsafe { msg_send![&*small, outputImage] };
        let small = small?;
        let extent: CGRect = unsafe { msg_send![&*small, extent] };
        let clamped: Retained<AnyObject> = unsafe { msg_send![&*small, imageByClampingToExtent] };
        let radius = ns_number(radius * pixels_per_point * SCALE);
        let blur = ci_filter(
            ns_string!("CIGaussianBlur"),
            &[
                (ns_string!("inputImage"), &clamped),
                (ns_string!("inputRadius"), &radius),
            ],
        )?;
        let output: Option<Retained<AnyObject>> = unsafe { msg_send![&*blur, outputImage] };
        let output = output?;
        let context = shared_ci_context()?;
        let raw: *mut CGImage =
            unsafe { msg_send![context, createCGImage: &*output, fromRect: extent] };
        std::ptr::NonNull::new(raw).map(|ptr| unsafe { CFRetained::from_raw(ptr) })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_graphics::{CGBitmapContextCreateImage, CGColorSpace, CGContext};

    unsafe extern "C" {
        fn CGBitmapContextCreate(
            data: *mut std::ffi::c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: &CGColorSpace,
            bitmap_info: u32,
        ) -> *mut CGContext;
    }

    #[test]
    fn wallpaper_blur_renders_a_quarter_size_image() {
        // 8 bits per channel, premultiplied alpha first.
        const PREMULTIPLIED_FIRST: u32 = 2;
        let space = CGColorSpace::new_device_rgb().expect("colour space");
        let raw = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                400,
                200,
                8,
                0,
                &space,
                PREMULTIPLIED_FIRST,
            )
        };
        let context = std::ptr::NonNull::new(raw).map(|ptr| unsafe { CFRetained::from_raw(ptr) });
        let image = CGBitmapContextCreateImage(context.as_deref()).expect("image");
        // A 400 px image of a 200 pt wide display (2x).
        let out = blurred(&image, 200.0, BACKDROP_BLUR).expect("blurred image");
        assert_eq!(CGImage::width(Some(&out)), 100);
        assert_eq!(CGImage::height(Some(&out)), 50);
    }
}
