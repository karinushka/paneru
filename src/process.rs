use log::{debug, warn};
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_foundation::{
    NSKeyValueObservingOptions, NSObjectNSKeyValueObserverRegistration, NSString,
};
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use stdext::function_name;

use crate::app::Application;
use crate::platform::{Pid, ProcessInfo, ProcessSerialNumber, WorkspaceObserver, get_process_info};
use crate::windows::WindowManager;

#[derive(Debug)]
#[repr(C)]
pub struct Process {
    pub psn: ProcessSerialNumber,
    pub pid: Pid,
    pub name: String,
    terminated: bool,
    pub application: Option<Retained<NSRunningApplication>>,
    pub policy: NSApplicationActivationPolicy,

    pub observer: Retained<WorkspaceObserver>,
    observing_launched: AtomicBool,
    observing_activated: AtomicBool,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.unobserve_finished_launching();
        self.unobserve_activation_policy();
    }
}

impl Process {
    pub fn application_launched(&mut self, window_manager: &mut WindowManager) -> Result<()> {
        if self.terminated {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                format!(
                    "{}: {} ({}) terminated during launch",
                    function_name!(),
                    self.name,
                    self.pid
                ),
            ));
        }

        if !self.finished_launching() {
            debug!(
                "{}: {} ({}) is not finished launching, subscribing to finishedLaunching changes",
                function_name!(),
                self.name,
                self.pid
            );
            self.observe_finished_launching();

            //
            // NOTE(koekeishiya): Do this again in case of race-conditions between the previous
            // check and key-value observation subscription. Not actually sure if this can happen
            // in practice..
            //

            if !self.finished_launching() {
                return Ok(());
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

            //
            // NOTE(koekeishiya): Do this again in case of race-conditions between the previous
            // check and key-value observation subscription. Not actually sure if this can happen
            // in practice..
            //

            if !self.is_observable() {
                return Ok(());
            }
            self.unobserve_activation_policy();
            warn!(
                "{}: {} suddenly became observable",
                function_name!(),
                self.name
            );
        }

        //
        // NOTE(koekeishiya): If we somehow receive a duplicate launched event due to the
        // subscription-timing-mess above, simply ignore the event..
        //

        if let Some(app) = window_manager.find_application(self.pid) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("{}: App {} already exists.", function_name!(), app.name()),
            ));
        }
        let app =
            Application::from_process(window_manager.main_cid, self, window_manager.tx.clone());
        // TODO: maybe refactor with WindowManager::start()

        if !app.observe()? {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{}: failed to register all observers {}",
                    function_name!(),
                    app.name()
                ),
            ));
        }

        debug!(
            "{}: Adding {} to list of apps.",
            function_name!(),
            app.name()
        );
        window_manager.applications.insert(app.pid(), app.clone());

        // int window_count;
        // struct window **window_list = window_manager_add_application_windows(
        //     &g_space_manager, &g_window_manager, application, &window_count);
        // uint32_t prev_window_id = g_window_manager.focused_window_id;
        let windows = window_manager.add_application_windows(&app)?;
        debug!(
            "{}: Added windows {} for {}.",
            function_name!(),
            windows
                .iter()
                .map(|window| format!("{}", window.id()))
                .collect::<Vec<_>>()
                .join(", "),
            app.name()
        );

        let active_panel = window_manager
            .active_display()?
            .active_panel(window_manager.main_cid)?;
        let insert_at = window_manager
            .focused_window
            .and_then(|id| window_manager.find_window(id))
            .and_then(|window| active_panel.index_of(&window).ok());
        match insert_at {
            Some(mut after) => {
                for window in &windows {
                    after = active_panel.insert_at(after, window.clone())?;
                }
            }
            None => windows.iter().for_each(|window| {
                active_panel.append(window.clone());
            }),
        };

        if let Some(window) = windows.first() {
            window_manager.reshuffle_around(window)?;
        }

        Ok(())
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
}

#[derive(Debug, Default)]
pub struct ProcessManager {
    pub processes: HashMap<ProcessSerialNumber, Pin<Box<Process>>>,
}

impl ProcessManager {
    pub fn find_process(&mut self, psn: &ProcessSerialNumber) -> Result<&mut Pin<Box<Process>>> {
        self.processes.get_mut(psn).ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: Psn {:?} not found.", function_name!(), psn),
        ))
    }

    pub fn process_add(
        &mut self,
        psn: &ProcessSerialNumber,
        observer: Retained<WorkspaceObserver>,
    ) -> Result<&mut Pin<Box<Process>>> {
        if self.processes.contains_key(psn) {
            return self.find_process(psn);
        }

        let mut pinfo = ProcessInfo::default();
        unsafe {
            get_process_info(psn, &mut pinfo);
        }
        let name = NonNull::new(pinfo.name.cast_mut()).ok_or(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{}: Nullptr as name for process {:?}.",
                function_name!(),
                psn
            ),
        ))?;
        let name = unsafe { name.as_ref() }.to_string();

        let apps =
            unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pinfo.pid) };

        let process = Process {
            psn: psn.clone(),
            name,
            pid: pinfo.pid,
            terminated: pinfo.terminated,
            application: apps,
            policy: NSApplicationActivationPolicy::Prohibited,
            observer,
            observing_launched: AtomicBool::new(false),
            observing_activated: AtomicBool::new(false),
        };
        self.processes.insert(psn.clone(), Box::pin(process));
        self.find_process(psn)
    }

    pub fn process_delete(&mut self, psn: &ProcessSerialNumber) {
        self.processes.remove(psn);
    }
}
