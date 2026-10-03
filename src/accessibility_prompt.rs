use std::path::Path;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication};
use objc2_foundation::NSString;

use crate::manager::{request_ax_privilege, reset_ax_privilege};
use crate::util::exe_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccessibilitySetupAction {
    Continue,
    ResetAndRetry,
    NotNow,
}

#[must_use]
pub(crate) const fn permission_pane_name_for_macos(is_macos_27_or_newer: bool) -> &'static str {
    if is_macos_27_or_newer {
        "Device Control and Data Access"
    } else {
        "Accessibility"
    }
}

#[must_use]
pub(crate) fn permission_pane_name() -> &'static str {
    permission_pane_name_for_macos(objc2::available!(macos = 27.0))
}

fn accessibility_informative_text(
    is_macos_27_or_newer: bool,
    binary_path: Option<&Path>,
) -> String {
    let pane = permission_pane_name_for_macos(is_macos_27_or_newer);
    let binary = binary_path.map_or_else(
        || "paneru".to_string(),
        |path| format!("paneru ({})", path.display()),
    );
    format!(
        "Paneru needs {pane} permission to move, resize, and arrange windows.\n\n\
         In System Settings, open Privacy & Security → {pane}, then turn on {binary}.\n\n\
         If Paneru is already enabled after an update or switching between Homebrew and Cargo, \
         its code signature changed: click \"Reset Permission & Retry\" (or remove the old entry \
         with the – button and add the binary again with the + button)."
    )
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

    let binary_path = exe_path();
    let informative_text =
        accessibility_informative_text(objc2::available!(macos = 27.0), binary_path.as_deref());

    let alert = NSAlert::new(main_thread_marker);
    alert.setMessageText(&NSString::from_str("Allow Paneru to Control Windows"));
    alert.setInformativeText(&NSString::from_str(&informative_text));
    alert.addButtonWithTitle(&NSString::from_str("Continue"));
    alert.addButtonWithTitle(&NSString::from_str("Reset Permission & Retry"));
    alert.addButtonWithTitle(&NSString::from_str("Not Now"));

    let response = alert.runModal();
    if response == NSAlertFirstButtonReturn {
        AccessibilitySetupAction::Continue
    } else if response == NSAlertSecondButtonReturn {
        AccessibilitySetupAction::ResetAndRetry
    } else {
        AccessibilitySetupAction::NotNow
    }
}

pub(crate) fn handle_accessibility_setup_action(action: AccessibilitySetupAction) {
    match action {
        AccessibilitySetupAction::Continue => {
            request_ax_privilege();
        }
        AccessibilitySetupAction::ResetAndRetry => {
            reset_ax_privilege();
            request_ax_privilege();
        }
        AccessibilitySetupAction::NotNow => {}
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{accessibility_informative_text, permission_pane_name_for_macos};

    #[test]
    fn permission_pane_name_matches_macos_release() {
        assert_eq!(permission_pane_name_for_macos(false), "Accessibility");
        assert_eq!(
            permission_pane_name_for_macos(true),
            "Device Control and Data Access"
        );
    }

    #[test]
    fn informative_text_uses_macos_27_terminology_and_binary_path() {
        let text =
            accessibility_informative_text(true, Some(Path::new("/opt/homebrew/bin/paneru")));
        assert!(text.contains("Privacy & Security → Device Control and Data Access"));
        assert!(text.contains("paneru (/opt/homebrew/bin/paneru)"));
        assert!(text.contains("Reset Permission & Retry"));
        assert!(!text.contains("Paneru.app"));
    }

    #[test]
    fn informative_text_uses_accessibility_before_macos_27() {
        let text = accessibility_informative_text(false, None);
        assert!(text.contains("Privacy & Security → Accessibility"));
        assert!(text.contains("turn on paneru."));
        assert!(!text.contains("Paneru.app"));
    }
}
