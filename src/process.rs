use log::{debug, warn};
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_foundation::{
    NSKeyValueObservingOptions, NSObjectNSKeyValueObserverRegistration, NSString,
};
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use stdext::function_name;

use crate::app::{Application, InnerApplication};
use crate::events::EventSender;
use crate::platform::{Pid, ProcessInfo, ProcessSerialNumber, WorkspaceObserver, get_process_info};
use crate::skylight::ConnID;

#[repr(C)]
pub struct Process {
    pub app: Option<Arc<RwLock<InnerApplication>>>,
    pub psn: ProcessSerialNumber,
    pub pid: Pid,
    pub name: String,
    pub terminated: bool,
    pub application: Option<Retained<NSRunningApplication>>,
    pub policy: NSApplicationActivationPolicy,

    pub observer: Retained<WorkspaceObserver>,
    pub observing_launched: AtomicBool,
    pub observing_activated: AtomicBool,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.unobserve_finished_launching();
        self.unobserve_activation_policy();
    }
}

impl Process {
    pub fn new(psn: &ProcessSerialNumber, observer: Retained<WorkspaceObserver>) -> Pin<Box<Self>> {
        let mut pinfo = ProcessInfo::default();
        unsafe {
            get_process_info(psn, &mut pinfo);
        }
        let name = NonNull::new(pinfo.name.cast_mut())
            .map(|s| unsafe { s.as_ref() }.to_string())
            .unwrap_or_default();

        // [[NSRunningApplication runningApplicationWithProcessIdentifier:process->pid] retain];
        let apps =
            unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pinfo.pid) };

        Box::pin(Process {
            app: None,
            psn: psn.clone(),
            name,
            pid: pinfo.pid,
            terminated: pinfo.terminated,
            application: apps,
            policy: NSApplicationActivationPolicy::Prohibited,
            observer,
            observing_launched: AtomicBool::new(false),
            observing_activated: AtomicBool::new(false),
        })
    }

    pub fn get_app(&self) -> Option<Application> {
        self.app.as_ref().map(|app| Application {
            inner: Arc::downgrade(app),
        })
    }

    pub fn is_observable(&mut self) -> bool {
        if let Some(app) = &self.application {
            self.policy = unsafe { app.activationPolicy() };
            self.policy == NSApplicationActivationPolicy::Regular
        } else {
            self.policy = NSApplicationActivationPolicy::Prohibited;
            false
        }
    }

    pub fn finished_launching(&self) -> bool {
        self.application
            .as_ref()
            .is_some_and(|app| unsafe { app.isFinishedLaunching() })
    }

    pub fn observe_finished_launching(&self) {
        self.observe("finishedLaunching");
        self.observing_launched.store(true, Ordering::Relaxed);
    }

    pub fn unobserve_finished_launching(&self) {
        if self.observing_launched.load(Ordering::Relaxed) {
            self.unobserve("finishedLaunching");
            self.observing_launched.store(false, Ordering::Relaxed);
        }
    }

    pub fn observe_activation_policy(&self) {
        self.observe("activationPolicy");
        self.observing_activated.store(true, Ordering::Relaxed);
    }

    pub fn unobserve_activation_policy(&self) {
        if self.observing_activated.load(Ordering::Relaxed) {
            self.unobserve("activationPolicy");
            self.observing_activated.store(false, Ordering::Relaxed);
        }
    }

    fn observe(&self, flavor: &str) {
        if let Some(app) = self.application.as_ref() {
            unsafe {
                let key_path = NSString::from_str(flavor);
                let options = NSKeyValueObservingOptions::New | NSKeyValueObservingOptions::Initial;
                app.addObserver_forKeyPath_options_context(
                    self.observer.deref(),
                    key_path.as_ref(),
                    options,
                    NonNull::from(self).as_ptr().cast(),
                );
            }
            debug!(
                "{}: observing {flavor} for {}",
                function_name!(),
                &self.name
            );
        }
    }

    fn unobserve(&self, flavor: &str) {
        if let Some(app) = self.application.as_ref() {
            unsafe {
                let key_path = NSString::from_str(flavor);
                app.removeObserver_forKeyPath_context(
                    self.observer.deref(),
                    key_path.as_ref(),
                    NonNull::from(self).as_ptr().cast(),
                );
            }
            debug!(
                "{}: removed {flavor} observers for {}",
                function_name!(),
                &self.name
            );
        }
    }

    pub fn ready(&mut self) -> bool {
        if !self.finished_launching() {
            debug!(
                "{}: {} ({}) is not finished launching, subscribing to finishedLaunching changes",
                function_name!(),
                self.name,
                self.pid
            );
            self.observe_finished_launching();

            // NOTE: Do this again in case of race-conditions between the previous check and
            // key-value observation subscription. Not actually sure if this can happen in
            // practice..

            if !self.finished_launching() {
                return false;
            }
            self.unobserve_finished_launching();
            warn!(
                "{}: {} suddenly finished launching",
                function_name!(),
                self.name
            );
        }

        if !self.is_observable() {
            debug!(
                "{}: {} ({}) is not observable, subscribing to activationPolicy changes",
                function_name!(),
                self.name,
                self.pid
            );
            self.observe_activation_policy();

            // NOTE: Do this again in case of race-conditions between the previous check and
            // key-value observation subscription. Not actually sure if this can happen in
            // practice..

            if !self.is_observable() {
                return false;
            }
            self.unobserve_activation_policy();
            warn!(
                "{}: {} suddenly became observable",
                function_name!(),
                self.name
            );
        }
        true
    }

    pub fn create_application(&mut self, cid: ConnID, events: EventSender) -> Result<Application> {
        let app = Arc::new(RwLock::new(InnerApplication::new(cid, self, events)?));
        self.app = Some(app);
        self.get_app().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: unable to find added application.", function_name!(),),
        ))
    }
}
