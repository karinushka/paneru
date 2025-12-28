use log::debug;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_foundation::{
    NSKeyValueObservingOptions, NSObjectNSKeyValueObserverRegistration, NSString,
};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use stdext::function_name;

use crate::events::BProcess;
use crate::platform::{Pid, ProcessSerialNumber, WorkspaceObserver};
use crate::skylight::OSStatus;

unsafe extern "C" {
    /// Get a copy of the name of a process.
    ///
    /// # Deprecated
    /// Use the `localizedName` property of the appropriate `NSRunningApplication` object.
    ///
    /// # Discussion
    /// Use this call to get the name of a process as a `CFString`. The name returned is a copy,
    /// so the caller must `CFRelease` the name when finished with it. The difference between
    /// this call and the `processName` field filled in by `GetProcessInformation` is that
    /// the name here is a `CFString`, and thus is capable of representing a multi-lingual name,
    /// whereas previously only a mac-encoded string was possible.
    ///
    /// # Mac OS X threading
    /// Thread safe since version 10.3
    ///
    /// # Arguments
    ///
    /// * `psn` - Serial number of the target process.
    /// * `name` - `CFString` representing the name of the process (must be released by caller with `CFRelease`).
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    ///
    /// # Original signature:
    /// `OSStatus` CopyProcessName(const `ProcessSerialNumber` *psn, `CFStringRef` *name);
    fn CopyProcessName(psn: *const ProcessSerialNumber, name: *mut *const CFString) -> OSStatus;

    /// Get the UNIX process ID corresponding to a process.
    ///
    /// # Deprecated
    /// Use the `processIdentifier` property of the appropriate `NSRunningApplication` object.
    ///
    /// # Discussion
    /// Given a Process serial number, this call will get the UNIX process ID for that process.
    /// Note that this call does not make sense for Classic apps, since they all share a single
    /// UNIX process ID.
    ///
    /// # Mac OS X threading
    /// Thread safe since version 10.3
    ///
    /// # Arguments
    ///
    /// * `psn` - Serial number of the target process.
    /// * `pid` - UNIX process ID of the process.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    /// # Original signature:
    /// `OSStatus` GetProcessPID(const `ProcessSerialNumber` *psn, `pid_t` *pid);
    fn GetProcessPID(psn: *const ProcessSerialNumber, pid: *mut Pid) -> OSStatus;
}

pub trait ProcessApi: Send + Sync {
    fn is_observable(&mut self) -> bool;
    fn name(&self) -> &str;
    fn pid(&self) -> Pid;
    fn psn(&self) -> ProcessSerialNumber;
    fn application(&self) -> Option<Retained<NSRunningApplication>>;
    fn ready(&mut self) -> bool;
}

pub struct ProcessOS {
    pub inner: Pin<Box<Process>>,
}

impl ProcessApi for ProcessOS {
    fn is_observable(&mut self) -> bool {
        self.inner.is_observable()
    }

    fn name(&self) -> &str {
        self.inner.name.as_str()
    }

    fn pid(&self) -> Pid {
        self.inner.pid
    }

    fn psn(&self) -> ProcessSerialNumber {
        self.inner.psn
    }

    fn application(&self) -> Option<Retained<NSRunningApplication>> {
        self.inner.application.clone()
    }

    fn ready(&mut self) -> bool {
        self.inner.ready()
    }
}

impl From<Pin<Box<Process>>> for BProcess {
    fn from(inner: Pin<Box<Process>>) -> Self {
        BProcess(Box::new(ProcessOS { inner }))
    }
}

#[repr(C)]
pub struct Process {
    pub psn: ProcessSerialNumber,
    pub pid: Pid,
    pub name: String,
    pub application: Option<Retained<NSRunningApplication>>,
    pub policy: NSApplicationActivationPolicy,

    pub observer: Retained<WorkspaceObserver>,
    observing_launched: AtomicBool,
    observing_activated: AtomicBool,
}

impl Drop for Process {
    /// Cleans up observers when the `Process` object is dropped.
    fn drop(&mut self) {
        self.unobserve_finished_launching();
        self.unobserve_activation_policy();
    }
}

impl Process {
    /// Creates a new `Process` instance. It retrieves process information and attempts to get an `NSRunningApplication` instance.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the process.
    /// * `observer` - A `Retained<WorkspaceObserver>` for observing workspace events.
    ///
    /// # Returns
    ///
    /// A `Pin<Box<Self>>` containing the new `Process` instance.
    pub fn new(psn: &ProcessSerialNumber, observer: Retained<WorkspaceObserver>) -> Pin<Box<Self>> {
        let mut pid: Pid = 0;
        unsafe { GetProcessPID(psn, NonNull::from(&mut pid).as_ptr()) };

        let mut nameref: *const CFString = std::ptr::null();
        unsafe { CopyProcessName(psn, &raw mut nameref) };
        let name = NonNull::new(nameref.cast_mut())
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .map(|name| name.to_string())
            .unwrap_or_default();

        // [[NSRunningApplication runningApplicationWithProcessIdentifier:process->pid] retain];
        let apps = NSRunningApplication::runningApplicationWithProcessIdentifier(pid);

        Box::pin(Process {
            psn: *psn,
            name,
            pid,
            application: apps,
            policy: NSApplicationActivationPolicy::Prohibited,
            observer,
            observing_launched: AtomicBool::new(false),
            observing_activated: AtomicBool::new(false),
        })
    }

    /// Checks if the application associated with this process is observable (i.e., has a regular activation policy).
    /// It updates the internal `policy` field.
    ///
    /// # Returns
    ///
    /// `true` if the application is observable, `false` otherwise.
    pub fn is_observable(&mut self) -> bool {
        if let Some(app) = &self.application {
            self.policy = app.activationPolicy();
            self.policy == NSApplicationActivationPolicy::Regular
        } else {
            self.policy = NSApplicationActivationPolicy::Prohibited;
            false
        }
    }

    /// Checks if the application associated with this process has finished launching.
    ///
    /// # Returns
    ///
    /// `true` if the application has finished launching, `false` otherwise.
    pub fn finished_launching(&self) -> bool {
        self.application
            .as_ref()
            .is_some_and(|app| app.isFinishedLaunching())
    }

    /// Subscribes to "finishedLaunching" key-value observations for the associated `NSRunningApplication`.
    pub fn observe_finished_launching(&self) {
        if !self.observing_launched.swap(true, Ordering::Acquire) {
            self.observe("finishedLaunching");
        }
    }

    /// Unsubscribes from "finishedLaunching" key-value observations.
    pub fn unobserve_finished_launching(&self) {
        if self.observing_launched.swap(false, Ordering::Release) {
            self.unobserve("finishedLaunching");
        }
    }

    /// Subscribes to "activationPolicy" key-value observations for the associated `NSRunningApplication`.
    pub fn observe_activation_policy(&self) {
        if !self.observing_activated.swap(true, Ordering::Acquire) {
            self.observe("activationPolicy");
        }
    }

    /// Unsubscribes from "activationPolicy" key-value observations.
    pub fn unobserve_activation_policy(&self) {
        if self.observing_activated.swap(false, Ordering::Release) {
            self.unobserve("activationPolicy");
        }
    }

    /// Helper function to add a key-value observer for a specified `flavor` (key path).
    ///
    /// # Arguments
    ///
    /// * `flavor` - The key path string to observe (e.g., "finishedLaunching", "activationPolicy").
    fn observe(&self, flavor: &str) {
        if let Some(app) = self.application.as_ref() {
            unsafe {
                let key_path = NSString::from_str(flavor);
                let options = NSKeyValueObservingOptions::New | NSKeyValueObservingOptions::Initial;
                app.addObserver_forKeyPath_options_context(
                    &self.observer,
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

    /// Helper function to remove a key-value observer for a specified `flavor` (key path).
    ///
    /// # Arguments
    ///
    /// * `flavor` - The key path string to unobserve.
    fn unobserve(&self, flavor: &str) {
        if let Some(app) = self.application.as_ref() {
            unsafe {
                let key_path = NSString::from_str(flavor);
                app.removeObserver_forKeyPath_context(
                    &self.observer,
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

    /// Checks if the process is ready for window management (finished launching and is observable).
    /// It subscribes to and unsubscribes from observers as needed to ensure the ready state.
    ///
    /// # Returns
    ///
    /// `true` if the process is ready, `false` otherwise.
    pub fn ready(&mut self) -> bool {
        if !self.finished_launching() {
            debug!(
                "{}: {} ({}) is not finished launching, subscribing to finishedLaunching changes",
                function_name!(),
                self.name,
                self.pid
            );
            self.observe_finished_launching();
            return false;
        }
        self.unobserve_finished_launching();

        if !self.is_observable() {
            debug!(
                "{}: {} ({}) is not observable, subscribing to activationPolicy changes",
                function_name!(),
                self.name,
                self.pid
            );
            self.observe_activation_policy();
            return false;
        }
        self.unobserve_activation_policy();
        true
    }
}
