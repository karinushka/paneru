use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSHapticFeedbackManager, NSHapticFeedbackPattern, NSHapticFeedbackPerformanceTime,
    NSHapticFeedbackPerformer as _,
};

/// Requests the system's generic trackpad pulse for a confirmed page change.
pub(crate) fn page_changed(_main_thread: MainThreadMarker) {
    NSHapticFeedbackManager::defaultPerformer().performFeedbackPattern_performanceTime(
        NSHapticFeedbackPattern::Generic,
        NSHapticFeedbackPerformanceTime::Now,
    );
}
