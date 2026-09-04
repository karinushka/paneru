//! The general pasteboard, wrapped so ECS systems never touch `objc2` directly.

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use tracing::warn;

/// Replaces the general pasteboard's contents with `text`. Returns `false` if
/// `AppKit` refused the write, in which case nothing was copied.
pub fn copy_to_clipboard(text: &str) -> bool {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // SAFETY: reading an AppKit constant that lives for the process lifetime.
    let string_type = unsafe { NSPasteboardTypeString };
    let copied = pasteboard.setString_forType(&NSString::from_str(text), string_type);
    if !copied {
        warn!("unable to write to the general pasteboard");
    }
    copied
}
