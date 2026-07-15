use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, extern_class, extern_methods,
    msg_send,
};
use objc2_foundation::{NSBundle, NSObject, NSString};
use tracing::warn;

use crate::events::{Event, EventSender};

const SILENT_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpdateStatus {
    pub is_checking: bool,
    pub available_version: Option<String>,
}

#[derive(Clone, Debug)]
struct SparkleUpdaterDelegateIvars {
    status: Rc<RefCell<UpdateStatus>>,
    events: EventSender,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruSparkleUpdaterDelegate"]
    #[ivars = SparkleUpdaterDelegateIvars]
    #[derive(Debug)]
    struct SparkleUpdaterDelegate;

    impl SparkleUpdaterDelegate {
        #[unsafe(method(updater:didFindValidUpdate:))]
        fn did_find_valid_update(&self, _updater: &AnyObject, item: &AnyObject) {
            let display_version: Retained<NSString> =
                unsafe { msg_send![item, displayVersionString] };
            let mut status = self.ivars().status.borrow_mut();
            status.available_version = Some(display_version.to_string());
            _ = self.ivars().events.send(Event::UpdaterStatusChanged);
        }

        #[unsafe(method(updaterDidNotFindUpdate:))]
        fn did_not_find_update(&self, _updater: &AnyObject) {
            self.ivars().status.borrow_mut().available_version = None;
            _ = self.ivars().events.send(Event::UpdaterStatusChanged);
        }

        #[unsafe(method(updater:didFinishUpdateCycleForUpdateCheck:error:))]
        fn did_finish_update_cycle(
            &self,
            _updater: &AnyObject,
            _update_check: isize,
            _error: Option<&AnyObject>,
        ) {
            self.ivars().status.borrow_mut().is_checking = false;
            _ = self.ivars().events.send(Event::UpdaterStatusChanged);
        }
    }
);

impl SparkleUpdaterDelegate {
    fn new(
        mtm: MainThreadMarker,
        status: Rc<RefCell<UpdateStatus>>,
        events: EventSender,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SparkleUpdaterDelegateIvars { status, events });
        unsafe { msg_send![super(this), init] }
    }
}

extern_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SPUUpdater"]
    #[derive(Debug)]
    struct SPUUpdater;
);

impl SPUUpdater {
    extern_methods!(
        #[unsafe(method(canCheckForUpdates))]
        #[unsafe(method_family = none)]
        fn can_check_for_updates(&self) -> bool;

        #[unsafe(method(checkForUpdateInformation))]
        #[unsafe(method_family = none)]
        fn check_for_update_information(&self);
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SPUStandardUpdaterController"]
    #[derive(Debug)]
    struct SPUStandardUpdaterController;
);

impl SPUStandardUpdaterController {
    extern_methods!(
        #[unsafe(method(initWithStartingUpdater:updaterDelegate:userDriverDelegate:))]
        #[unsafe(method_family = init)]
        fn init_with_starting_updater(
            this: Allocated<Self>,
            starting_updater: bool,
            updater_delegate: Option<&AnyObject>,
            user_driver_delegate: Option<&AnyObject>,
        ) -> Retained<Self>;

        #[unsafe(method(updater))]
        #[unsafe(method_family = none)]
        fn updater(&self) -> Retained<SPUUpdater>;

        /// # Safety
        ///
        /// The sender, when present, must be a valid Objective-C object.
        #[unsafe(method(checkForUpdates:))]
        #[unsafe(method_family = none)]
        unsafe fn check_for_updates(&self, sender: Option<&AnyObject>);
    );
}

/// Owns Sparkle's loaded framework bundle and standard updater controller.
///
/// Sparkle is loaded at runtime so regular `cargo` builds do not need a
/// framework search path or a link-time dependency on Sparkle.
pub struct SparkleUpdater {
    _framework_bundle: Retained<NSBundle>,
    _delegate: Retained<SparkleUpdaterDelegate>,
    controller: Retained<SPUStandardUpdaterController>,
    status: Rc<RefCell<UpdateStatus>>,
    events: EventSender,
    silent_schedule: SilentUpdateSchedule,
}

#[derive(Clone, Copy, Debug)]
struct SilentUpdateSchedule {
    next_check: Instant,
}

impl SilentUpdateSchedule {
    fn new(now: Instant) -> Self {
        Self { next_check: now }
    }

    fn deadline(self) -> Instant {
        self.next_check
    }

    fn due(self, now: Instant) -> bool {
        now >= self.next_check
    }

    fn advance(&mut self, now: Instant) {
        self.next_check = now + SILENT_CHECK_INTERVAL;
    }
}

fn mark_silent_check_started(status: &RefCell<UpdateStatus>, events: &EventSender) {
    status.borrow_mut().is_checking = true;
    _ = events.send(Event::UpdaterStatusChanged);
}

impl SparkleUpdater {
    pub fn load(mtm: MainThreadMarker, events: EventSender) -> Option<Self> {
        let main_bundle = NSBundle::mainBundle();
        let Some(frameworks_path) = main_bundle.privateFrameworksPath() else {
            warn!("unable to load Sparkle: the main bundle has no private frameworks path");
            return None;
        };
        let framework_path = frameworks_path
            .stringByAppendingPathComponent(&NSString::from_str("Sparkle.framework"));
        let Some(framework_bundle) = NSBundle::bundleWithPath(&framework_path) else {
            warn!(
                path = %framework_path,
                "unable to load Sparkle: framework bundle was not found"
            );
            return None;
        };

        if let Err(error) = unsafe { framework_bundle.loadAndReturnError() } {
            warn!(
                path = %framework_path,
                error = %error.localizedDescription(),
                "unable to load Sparkle framework"
            );
            return None;
        }

        // Check dynamically before using the typed class wrapper. Calling
        // `SPUStandardUpdaterController::class()` while the class is absent
        // would panic inside objc2's class lookup.
        if AnyClass::get(c"SPUStandardUpdaterController").is_none() {
            warn!("unable to load Sparkle: updater controller class is missing");
            return None;
        }

        let status = Rc::new(RefCell::new(UpdateStatus::default()));
        let delegate = SparkleUpdaterDelegate::new(mtm, Rc::clone(&status), events.clone());
        let controller = SPUStandardUpdaterController::init_with_starting_updater(
            SPUStandardUpdaterController::alloc(mtm),
            true,
            Some(&delegate),
            None,
        );

        Some(Self {
            _framework_bundle: framework_bundle,
            _delegate: delegate,
            controller,
            status,
            events,
            silent_schedule: SilentUpdateSchedule::new(Instant::now()),
        })
    }

    pub fn controller_target(&self) -> &AnyObject {
        &self.controller
    }

    pub fn can_check_for_updates(&self) -> bool {
        self.controller.updater().can_check_for_updates()
    }

    pub fn status(&self) -> UpdateStatus {
        self.status.borrow().clone()
    }

    pub fn next_silent_check(&self) -> Instant {
        self.silent_schedule.deadline()
    }

    pub fn maybe_check_silently(&mut self, now: Instant) {
        if !self.silent_schedule.due(now) {
            return;
        }
        self.silent_schedule.advance(now);
        if self.status.borrow().is_checking {
            return;
        }

        let updater = self.controller.updater();
        if !updater.can_check_for_updates() {
            return;
        }

        mark_silent_check_started(&self.status, &self.events);
        updater.check_for_update_information();
    }

    #[allow(dead_code)]
    pub fn check_for_updates(&self) {
        unsafe { self.controller.check_for_updates(None) };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SILENT_CHECK_INTERVAL, SilentUpdateSchedule, UpdateStatus, mark_silent_check_started,
    };
    use crate::events::{Event, EventSender};
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    #[test]
    fn silent_update_schedule_exposes_real_hourly_deadline() {
        let now = Instant::now();
        let mut schedule = SilentUpdateSchedule::new(now);
        assert!(schedule.due(now));
        schedule.advance(now);
        assert_eq!(schedule.deadline(), now + SILENT_CHECK_INTERVAL);
        assert!(!schedule.due(now + Duration::from_secs(3599)));
        assert!(schedule.due(now + SILENT_CHECK_INTERVAL));
    }

    #[test]
    fn silent_check_start_emits_status_transition() {
        let status = RefCell::new(UpdateStatus::default());
        let (events, receiver) = EventSender::new();

        mark_silent_check_started(&status, &events);

        assert!(status.borrow().is_checking);
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::UpdaterStatusChanged)
        ));
    }
}
