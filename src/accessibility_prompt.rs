use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSApplication};
use objc2_foundation::NSString;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccessibilitySetupAction {
    Continue,
    NotNow,
}

pub(crate) fn show_accessibility_setup(
    main_thread_marker: MainThreadMarker,
) -> AccessibilitySetupAction {
    let app = NSApplication::sharedApplication(main_thread_marker);
    if objc2::available!(macos = 14.0) {
        app.activate();
    } else {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }

    let alert = NSAlert::new(main_thread_marker);
    alert.setMessageText(&NSString::from_str("Allow Paneru to Control Windows"));
    alert.setInformativeText(&NSString::from_str(
        "Paneru needs Accessibility access to move, resize, and arrange windows.\n\n\
         In System Settings, open Privacy & Security → Accessibility, then turn on Paneru.\n\n\
         If Paneru is already listed but access still does not work, remove the old entry with \
         the – button, add Paneru.app again with the + button, and turn it on.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Continue"));
    alert.addButtonWithTitle(&NSString::from_str("Not Now"));

    if alert.runModal() == NSAlertFirstButtonReturn {
        AccessibilitySetupAction::Continue
    } else {
        AccessibilitySetupAction::NotNow
    }
}
