use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::NSScreen;
use objc2_core_graphics::CGDirectDisplayID;
use objc2_foundation::{NSDefaultRunLoopMode, NSObject, NSObjectProtocol, NSRunLoop};
use objc2_quartz_core::CADisplayLink;
use tracing::{debug, warn};

use crate::platform::macos_major_version;
use crate::util::read_screen_property;

#[derive(Debug)]
struct DisplayLinkTargetIvars {
    fired: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruDisplayLinkTarget"]
    #[ivars = DisplayLinkTargetIvars]
    #[derive(Debug)]
    struct DisplayLinkTarget;

    unsafe impl NSObjectProtocol for DisplayLinkTarget {}

    impl DisplayLinkTarget {
        #[unsafe(method(displayLinkDidFire:))]
        fn display_link_did_fire(&self, _: &CADisplayLink) {
            self.ivars().fired.set(true);
        }
    }
);

impl DisplayLinkTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DisplayLinkTargetIvars {
            fired: Cell::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FramePacingMode {
    DisplayLink,
    TimerFallback,
}

fn pacing_mode(macos_major: u32, display_link_available: bool) -> FramePacingMode {
    if macos_major >= 14 && display_link_available {
        FramePacingMode::DisplayLink
    } else {
        FramePacingMode::TimerFallback
    }
}

pub(super) struct DisplayFramePacer {
    mtm: MainThreadMarker,
    target: Retained<DisplayLinkTarget>,
    display_id: Option<CGDirectDisplayID>,
    link: Option<Retained<CADisplayLink>>,
}

impl DisplayFramePacer {
    pub(super) fn new(mtm: MainThreadMarker) -> Self {
        Self {
            mtm,
            target: DisplayLinkTarget::new(mtm),
            display_id: None,
            link: None,
        }
    }

    pub(super) fn arm(&mut self, display_id: CGDirectDisplayID) -> bool {
        if macos_major_version() < 14 {
            return false;
        }
        if self.display_id != Some(display_id) {
            self.configure(display_id);
        }
        if pacing_mode(macos_major_version(), self.link.is_some()) == FramePacingMode::TimerFallback
        {
            return false;
        }
        let Some(link) = self.link.as_ref() else {
            return false;
        };
        self.target.ivars().fired.set(false);
        link.setPaused(false);
        true
    }

    pub(super) fn frame_fired(&self) -> bool {
        self.target.ivars().fired.get()
    }

    pub(super) fn pause(&self) {
        if let Some(link) = self.link.as_ref() {
            link.setPaused(true);
        }
    }

    fn configure(&mut self, display_id: CGDirectDisplayID) {
        if let Some(link) = self.link.take() {
            link.invalidate();
        }
        self.display_id = Some(display_id);

        let screens = NSScreen::screens(self.mtm);
        let Some((link, maximum_fps)) = read_screen_property(&screens, display_id, |screen| {
            let maximum_fps = screen.maximumFramesPerSecond();
            let link = unsafe {
                screen.displayLinkWithTarget_selector(&self.target, sel!(displayLinkDidFire:))
            };
            (link, maximum_fps)
        }) else {
            warn!(
                display_id,
                "unable to create display link for active screen"
            );
            return;
        };

        link.setPaused(true);
        let run_loop = NSRunLoop::mainRunLoop();
        unsafe {
            link.addToRunLoop_forMode(&run_loop, NSDefaultRunLoopMode);
        }
        debug!(
            display_id,
            maximum_fps, "using display-synchronized animation pacing"
        );
        self.link = Some(link);
    }
}

impl Drop for DisplayFramePacer {
    fn drop(&mut self) {
        if let Some(link) = self.link.take() {
            link.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FramePacingMode, pacing_mode};

    #[test]
    fn uses_timer_fallback_before_macos_14() {
        assert_eq!(pacing_mode(13, true), FramePacingMode::TimerFallback);
    }

    #[test]
    fn uses_timer_fallback_when_display_link_creation_fails() {
        assert_eq!(pacing_mode(14, false), FramePacingMode::TimerFallback);
    }

    #[test]
    fn uses_display_link_when_supported_and_available() {
        assert_eq!(pacing_mode(14, true), FramePacingMode::DisplayLink);
    }
}
