//! Screen recording permission, needed by anything that reads other apps'
//! window contents (`CGWindowListCreateImage`, `ScreenCaptureKit`). Without
//! it macOS still hands back images, just without the windows' contents, so
//! features that capture should check [`allowed`] and degrade instead.

use std::sync::atomic::{AtomicBool, Ordering};

use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use tracing::info;

static ASKED: AtomicBool = AtomicBool::new(false);

/// Whether paneru may capture other apps' windows. Doesn't prompt.
///
/// macOS caches the answer per process: after the user grants the
/// permission, this can stay `false` until paneru restarts.
pub fn allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Asks for the permission (the system prompt, which macOS only shows the
/// first time), at most once per run.
pub fn request_once() {
    if ASKED.swap(true, Ordering::Relaxed) {
        return;
    }
    info!(
        "screen recording permission is off: window previews are disabled. Grant it in \
         System Settings > Privacy & Security > Screen & System Audio Recording, then restart paneru"
    );
    CGRequestScreenCaptureAccess();
}
