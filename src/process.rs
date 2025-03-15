use log::{debug, error, warn};
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_core_foundation::CFString;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::thread;
use std::time::Duration;

use crate::app::Application;
use crate::platform::{Pid, ProcessInfo, ProcessSerialNumber, get_process_info};
use crate::windows::WindowManager;

#[derive(Debug)]
#[repr(C)]
pub struct Process {
    pub psn: ProcessSerialNumber,
    pub pid: Pid,
    pub name: String,
    terminated: bool,
    application: Option<Retained<NSRunningApplication>>,
    policy: NSApplicationActivationPolicy,
}

impl Process {
    pub fn application_launched(&mut self, window_manager: &mut WindowManager) {
        if self.terminated {
            warn!("{} ({}) terminated during launch", self.name, self.pid);
            return;
        }

        if !self.finished_launching() {
            debug!(
                "{} ({}) is not finished launching, subscribing to finishedLaunching changes",
                self.name, self.pid
            );

            //
            // NOTE(koekeishiya): Do this again in case of race-conditions between the previous
            // check and key-value observation subscription. Not actually sure if this can happen
            // in practice..
            //
        }

        // FIXME: currently this polls for the application to be ready loaded.
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(300));
            if self.finished_launching() {
                break;
            }
        }

        if !self.is_observable() {
            debug!(
                "{} ({}) is not observable, subscribing to activationPolicy changes",
                self.name, self.pid
            );

            //
            // NOTE(koekeishiya): Do this again in case of race-conditions between the previous
            // check and key-value observation subscription. Not actually sure if this can happen
            // in practice..
            //

            if self.is_observable() {
                // @try {
                //     NSRunningApplication *application = __atomic_load_n(&process->ns_application, __ATOMIC_RELAXED);
                //     if (application && [application observationInfo]) {
                //         [application removeObserver:g_workspace_context forKeyPath:@"activationPolicy" context:process];
                //     }
                // } @catch (NSException * __unused exception) {}
            } else {
                return;
            }
        }

        //
        // NOTE(koekeishiya): If we somehow receive a duplicate launched event due to the
        // subscription-timing-mess above, simply ignore the event..
        //

        if let Some(app) = window_manager.find_application(self.pid) {
            warn!(
                "application_launched: App {} already exists.",
                app.inner().name
            );
            return;
        }
        let app =
            Application::from_process(window_manager.main_cid, self, window_manager.tx.clone());
        // TODO: maybe refactor with WindowManager::start()

        if !app.observe() {
            warn!(
                "application_launched: failed to observe {}",
                app.inner().name
            );
            return;
        }

        debug!(
            "application_launched: Adding {} to list of apps.",
            app.inner().name
        );
        window_manager
            .applications
            .insert(app.inner().pid, app.clone());

        let windows = window_manager.add_application_windows(&app);
        debug!(
            "application_launched: Added windows {} for {}.",
            windows
                .into_iter()
                .map(|window| format!("{}", window.inner().id))
                .collect::<Vec<_>>()
                .join(", "),
            app.inner().name
        );
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

    fn finished_launching(&self) -> bool {
        self.application
            .as_ref()
            .is_some_and(|app| unsafe { app.isFinishedLaunching() })
    }
}

#[derive(Debug, Default)]
pub struct ProcessManager {
    pub processes: HashMap<ProcessSerialNumber, Process>,
}

impl ProcessManager {
    pub fn find_process(&mut self, psn: &ProcessSerialNumber) -> Option<&mut Process> {
        self.processes.get_mut(psn)
    }

    pub fn process_add(&mut self, psn: &ProcessSerialNumber) -> Option<&mut Process> {
        if self.processes.contains_key(psn) {
            // NOTE(koekeishiya): Some garbage applications (e.g Steam) are reported twice with the
            // same PID and PSN for some hecking reason. It is by definition NOT possible for two
            // processes to exist at the same time with the same PID and PSN. If we detect such a
            // scenario we simply discard the dupe notification..
            return None;
        }
        let mut pinfo = ProcessInfo::default();
        unsafe {
            get_process_info(psn, &mut pinfo);
        }
        let name = if let Some(name) = NonNull::new(pinfo.name as *mut CFString) {
            unsafe { name.as_ref() }.to_string()
        } else {
            error!("process_add: nullptr 'name' passed.");
            return None;
        };

        let apps =
            unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pinfo.pid) };

        let process = Process {
            psn: psn.clone(),
            name,
            pid: pinfo.pid,
            terminated: pinfo.terminated,
            application: apps,
            policy: NSApplicationActivationPolicy::Prohibited,
        };
        self.processes.insert(psn.clone(), process);
        self.find_process(psn)
    }

    pub fn process_delete(&mut self, psn: &ProcessSerialNumber) {
        self.processes.remove(psn);
    }
}
