// resize.rs — easy-resize, Paneru'ya entegre edilmiş hali
// Orijinal: easy-resize v0.3.0
//
// Kullanım: .paneru.toml içinde:
//   [options]
//   resize_modifier = "cmd+ctrl"   # alt, ctrl, cmd, shift, kombinasyonlar: "cmd+alt" vs.
//
// Cmd + Ctrl + Sağ tık + Sürükle (default)
//   Sol %50  → sol kenarı resize et
//   Sağ %50  → sağ kenarı resize et

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use core_foundation::base::{CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use core_graphics::geometry::{CGPoint, CGSize};

// ─── AXUIElement C API ─────────────────────────────────────────────────────

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> AXRef;
    fn AXUIElementCopyElementAtPosition(app: AXRef, x: f32, y: f32, out: *mut AXRef) -> i32;
    fn AXUIElementCopyAttributeValue(el: AXRef, attr: CFStringRef, out: *mut CFTypeRef) -> i32;
    fn AXUIElementSetAttributeValue(el: AXRef, attr: CFStringRef, val: CFTypeRef) -> i32;
    fn AXValueCreate(ty: u32, ptr: *const std::ffi::c_void) -> CFTypeRef;
    fn AXValueGetValue(val: CFTypeRef, ty: u32, ptr: *mut std::ffi::c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
}

type AXRef = *mut std::ffi::c_void;

const AX_OK: i32 = 0;
const AX_CGPOINT: u32 = 1;
const AX_CGSIZE: u32 = 2;

// ─── Global state ──────────────────────────────────────────────────────────

/// Modifier down swallowed edildi, bir sonraki up da swallow edilecek
static SWALLOW_UP: AtomicBool = AtomicBool::new(false);

/// Applier thread çalışıyor
static DRAGGING: AtomicBool = AtomicBool::new(false);

/// Init thread bitti, drag state hazır
static STATE_READY: AtomicBool = AtomicBool::new(false);

/// Hedef frame (AtomicF64 olmadığı için i64 bit-cast)
static TARGET_X: AtomicI64 = AtomicI64::new(0);
static TARGET_Y: AtomicI64 = AtomicI64::new(0);
static TARGET_W: AtomicI64 = AtomicI64::new(0);
static TARGET_H: AtomicI64 = AtomicI64::new(0);

fn f2i(v: f64) -> i64 { i64::from_ne_bytes(v.to_ne_bytes()) }
fn i2f(v: i64) -> f64 { f64::from_ne_bytes(v.to_ne_bytes()) }

fn store_target(x: f64, y: f64, w: f64, h: f64) {
    TARGET_X.store(f2i(x), Ordering::Relaxed);
    TARGET_Y.store(f2i(y), Ordering::Relaxed);
    TARGET_W.store(f2i(w), Ordering::Relaxed);
    TARGET_H.store(f2i(h), Ordering::Relaxed);
}

fn load_target() -> (f64, f64, f64, f64) {
    (
        i2f(TARGET_X.load(Ordering::Relaxed)),
        i2f(TARGET_Y.load(Ordering::Relaxed)),
        i2f(TARGET_W.load(Ordering::Relaxed)),
        i2f(TARGET_H.load(Ordering::Relaxed)),
    )
}

struct DragState {
    win_origin: CGPoint,
    win_size: CGSize,
    start_mouse: CGPoint,
    resize_left: bool,
}

static STATE: Mutex<Option<DragState>> = Mutex::new(None);

struct SendablePoint { x: f64, y: f64 }
unsafe impl Send for SendablePoint {}

// ─── Modifier config ───────────────────────────────────────────────────────

/// .paneru.toml'dan okunan modifier string'i tutar
/// Örnek: "cmd+ctrl", "alt", "cmd+alt+shift"
static RESIZE_MODIFIER: Mutex<Option<String>> = Mutex::new(None);

/// Paneru başlarken config'den çağrılır.
/// modifier: "cmd+ctrl" | "alt" | "ctrl+shift" vs.
pub fn set_resize_modifier(modifier: &str) {
    *RESIZE_MODIFIER.lock().unwrap() = Some(modifier.to_lowercase());
}
pub fn is_dragging() -> bool {
    DRAGGING.load(Ordering::Relaxed)
}

fn mods_ok(flags: CGEventFlags) -> bool {
    let guard = RESIZE_MODIFIER.lock().unwrap();
    let modifier = guard.as_deref().unwrap_or("ctrl+alt");
    let parts: Vec<&str> = modifier.split('+').collect();

    let needs_cmd   = parts.contains(&"cmd");
    let needs_ctrl  = parts.contains(&"ctrl");
    let needs_alt   = parts.contains(&"alt");
    let needs_shift = parts.contains(&"shift");

    (!needs_cmd   || flags.contains(CGEventFlags::CGEventFlagCommand))
        && (!needs_ctrl  || flags.contains(CGEventFlags::CGEventFlagControl))
        && (!needs_alt   || flags.contains(CGEventFlags::CGEventFlagAlternate))
        && (!needs_shift || flags.contains(CGEventFlags::CGEventFlagShift))
}

// ─── AX helpers ────────────────────────────────────────────────────────────

unsafe fn window_at(x: f32, y: f32) -> Option<AXRef> {
    unsafe {
        let sys = AXUIElementCreateSystemWide();
        if sys.is_null() { return None; }
        let mut el: AXRef = std::ptr::null_mut();
        let err = AXUIElementCopyElementAtPosition(sys, x, y, &mut el);
        CFRelease(sys as CFTypeRef);
        if err != AX_OK || el.is_null() { return None; }
        let win = climb_to_window(el);
        CFRelease(el as CFTypeRef);
        win
    }
}

unsafe fn climb_to_window(start: AXRef) -> Option<AXRef> {
    unsafe {
        let mut cur: AXRef = CFRetain(start as CFTypeRef) as AXRef;
        for _ in 0..20 {
            if cur.is_null() { return None; }
            let role_attr = CFString::new("AXRole");
            let mut role_val: CFTypeRef = std::ptr::null_mut();
            let err = AXUIElementCopyAttributeValue(cur, role_attr.as_concrete_TypeRef(), &mut role_val);
            if err == AX_OK && !role_val.is_null() {
                let role = CFString::wrap_under_create_rule(role_val as CFStringRef).to_string();
                if role == "AXWindow" { return Some(cur); }
            }
            let parent_attr = CFString::new("AXParent");
            let mut parent_val: CFTypeRef = std::ptr::null_mut();
            let err2 = AXUIElementCopyAttributeValue(cur, parent_attr.as_concrete_TypeRef(), &mut parent_val);
            CFRelease(cur as CFTypeRef);
            if err2 != AX_OK || parent_val.is_null() { return None; }
            cur = parent_val as AXRef;
        }
        CFRelease(cur as CFTypeRef);
        None
    }
}

unsafe fn get_frame(win: AXRef) -> Option<(CGPoint, CGSize)> {
    unsafe {
        let pos_attr = CFString::new("AXPosition");
        let mut pos_val: CFTypeRef = std::ptr::null_mut();
        if AXUIElementCopyAttributeValue(win, pos_attr.as_concrete_TypeRef(), &mut pos_val) != AX_OK
            || pos_val.is_null() { return None; }
        let mut origin = CGPoint { x: 0.0, y: 0.0 };
        AXValueGetValue(pos_val, AX_CGPOINT, &mut origin as *mut _ as *mut _);
        CFRelease(pos_val);

        let sz_attr = CFString::new("AXSize");
        let mut sz_val: CFTypeRef = std::ptr::null_mut();
        if AXUIElementCopyAttributeValue(win, sz_attr.as_concrete_TypeRef(), &mut sz_val) != AX_OK
            || sz_val.is_null() { return None; }
        let mut size = CGSize { width: 0.0, height: 0.0 };
        AXValueGetValue(sz_val, AX_CGSIZE, &mut size as *mut _ as *mut _);
        CFRelease(sz_val);

        Some((origin, size))
    }
}

unsafe fn ax_set_pos(win: AXRef, pt: CGPoint) {
    unsafe {
        let attr = CFString::new("AXPosition");
        let val = AXValueCreate(AX_CGPOINT, &pt as *const _ as *const _);
        if !val.is_null() {
            AXUIElementSetAttributeValue(win, attr.as_concrete_TypeRef(), val);
            CFRelease(val);
        }
    }
}

unsafe fn ax_set_sz(win: AXRef, sz: CGSize) {
    unsafe {
        let attr = CFString::new("AXSize");
        let val = AXValueCreate(AX_CGSIZE, &sz as *const _ as *const _);
        if !val.is_null() {
            AXUIElementSetAttributeValue(win, attr.as_concrete_TypeRef(), val);
            CFRelease(val);
        }
    }
}

// ─── Init thread ───────────────────────────────────────────────────────────

fn spawn_init(mouse: SendablePoint) {
    std::thread::spawn(move || {
        let x = mouse.x;
        let y = mouse.y;

        let win = unsafe { window_at(x as f32, y as f32) };
        let win = match win {
            Some(w) => w,
            None => {
                STATE_READY.store(true, Ordering::SeqCst);
                return;
            }
        };

        let frame = unsafe { get_frame(win) };
        let (origin, size) = match frame {
            Some(f) => f,
            None => {
                unsafe { CFRelease(win as CFTypeRef) };
                STATE_READY.store(true, Ordering::SeqCst);
                return;
            }
        };

        let rel_x = x - origin.x;
        let resize_left = rel_x < size.width / 2.0;

        store_target(origin.x, origin.y, size.width, size.height);

        *STATE.lock().unwrap() = Some(DragState {
            win_origin: origin,
            win_size: size,
            start_mouse: CGPoint { x, y },
            resize_left,
        });

        STATE_READY.store(true, Ordering::SeqCst);
        DRAGGING.store(true, Ordering::SeqCst);

        // Applier: en son hedefi uygular, DRAGGING=false olunca durur
        while DRAGGING.load(Ordering::Relaxed) {
            let (tx, ty, tw, th) = load_target();
            unsafe {
                ax_set_pos(win, CGPoint { x: tx, y: ty });
                ax_set_sz(win, CGSize { width: tw, height: th });
            }
            std::thread::sleep(Duration::from_millis(16));
        }

        unsafe { CFRelease(win as CFTypeRef) };
    });
}

// ─── Event callback ────────────────────────────────────────────────────────

fn on_event(_proxy: CGEventTapProxy, kind: CGEventType, event: &CGEvent) -> Option<CGEvent> {
    match kind {
        CGEventType::RightMouseDown => {
            if !mods_ok(event.get_flags()) {
                SWALLOW_UP.store(false, Ordering::SeqCst);
                return Some(event.clone());
            }
            SWALLOW_UP.store(true, Ordering::SeqCst);
            STATE_READY.store(false, Ordering::SeqCst);
            let loc = event.location();
            spawn_init(SendablePoint { x: loc.x, y: loc.y });
            None
        }

        CGEventType::RightMouseDragged => {
            if !STATE_READY.load(Ordering::Relaxed) { return None; }
            if !DRAGGING.load(Ordering::Relaxed) { return None; }
            let loc = event.location();
            let guard = STATE.lock().unwrap();
            if let Some(ref s) = *guard {
                let dx = loc.x - s.start_mouse.x;
                const MIN_W: f64 = 80.0;
                if s.resize_left {
                    let new_w = s.win_size.width - dx;
                    if new_w >= MIN_W {
                        store_target(s.win_origin.x + dx, s.win_origin.y, new_w, s.win_size.height);
                    }
                } else {
                    let new_w = s.win_size.width + dx;
                    if new_w >= MIN_W {
                        store_target(s.win_origin.x, s.win_origin.y, new_w, s.win_size.height);
                    }
                }
            }
            None
        }

        CGEventType::RightMouseUp => {
            if SWALLOW_UP.swap(false, Ordering::SeqCst) {
                DRAGGING.store(false, Ordering::SeqCst);
                *STATE.lock().unwrap() = None;
                return None;
            }
            Some(event.clone())
        }

        _ => Some(event.clone()),
    }
}

// ─── Dışarıya açılan fonksiyonlar ─────────────────────────────────────────

/// Paneru'nun main.rs'inden çağrılır. Event tap'i ayrı bir thread'de başlatır.
/// Paneru'nun kendi RunLoop'unu bloklamaz.
pub fn start_resize_listener() {
    std::thread::spawn(|| {
        let tap = CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            vec![
                CGEventType::RightMouseDown,
                CGEventType::RightMouseDragged,
                CGEventType::RightMouseUp,
            ],
            on_event,
        );

        let tap = match tap {
            Ok(t) => t,
            Err(_) => {
                eprintln!(
                    "[resize] CGEventTap oluşturulamadı!\n\
                     System Settings → Privacy & Security → Accessibility\n\
                     → Bu binary'yi listeye ekle ve tekrar dene."
                );
                return;
            }
        };

        let loop_src = tap
            .mach_port
            .create_runloop_source(0)
            .expect("[resize] RunLoop source oluşturulamadı");

        unsafe {
            use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
            let rl = CFRunLoop::get_current();
            rl.add_source(&loop_src, kCFRunLoopCommonModes);
            tap.enable();
            CFRunLoop::run_current();
        }
    });
    pub fn is_dragging() -> bool {
    DRAGGING.load(Ordering::Relaxed)
}
}