//! The general pasteboard, wrapped so ECS systems never touch `objc2` directly.

#[cfg(not(test))]
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
#[cfg(not(test))]
use objc2_foundation::NSString;
#[cfg(not(test))]
use tracing::warn;

#[cfg(test)]
static TEST_CLIPBOARD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub fn get_test_clipboard() -> Option<String> {
    TEST_CLIPBOARD.lock().unwrap().clone()
}

/// Replaces the general pasteboard's contents with `text`. Returns `false` if
/// `AppKit` refused the write, in which case nothing was copied.
pub fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(test)]
    {
        *TEST_CLIPBOARD.lock().unwrap() = Some(text.to_owned());
        true
    }
    #[cfg(not(test))]
    {
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
}
