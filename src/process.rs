use log::{debug, error, warn};
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_core_foundation::CFString;
use objc2_foundation::{
    NSKeyValueObservingOptions, NSObjectNSKeyValueObserverRegistration, NSString,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use stdext::function_name;
use stdext::prelude::RwLockExt;

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
    pub fn application_launched(&mut self, window_manager: &mut WindowManager) {
        if self.terminated {
            warn!(
                "{}: {} ({}) terminated during launch",
                function_name!(),
                self.name,
                self.pid
            );
            return;
        }

        if !self.finished_launching() {
            // debug("%s: %s (%d) is not finished launching, subscribing to finishedLaunching changes\n", __FUNCTION__, process->name, process->pid);
            debug!(
                "{}: {} ({}) is not finished launching, subscribing to finishedLaunching changes",
                function_name!(),
                self.name,
                self.pid
            );
            // workspace_application_observe_finished_launching(g_workspace_context, process);
            self.observe_finished_launching();

            //
            // NOTE(koekeishiya): Do this again in case of race-conditions between the previous
            // check and key-value observation subscription. Not actually sure if this can happen
            // in practice..
            //

            // if (workspace_application_is_finished_launching(process)) {
            //     @try {
            //         NSRunningApplication *application = __atomic_load_n(&process->ns_application, __ATOMIC_RELAXED);
            //         if (application && [application observationInfo]) {
            //             [application removeObserver:g_workspace_context forKeyPath:@"finishedLaunching" context:process];
            //         }
            //     } @catch (NSException * __unused exception) {}
            // } else { return; }
            if !self.finished_launching() {
                return;
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
            // workspace_application_observe_activation_policy(g_workspace_context, process);
            self.observe_activation_policy();

            //
            // NOTE(koekeishiya): Do this again in case of race-conditions between the previous
            // check and key-value observation subscription. Not actually sure if this can happen
            // in practice..
            //

            if !self.is_observable() {
                return;
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
            warn!(
                "{}: App {} already exists.",
                function_name!(),
                app.inner().name
            );
            return;
        }
        let app =
            Application::from_process(window_manager.main_cid, self, window_manager.tx.clone());
        // TODO: maybe refactor with WindowManager::start()

        if !app.observe() {
            warn!(
                "{}: failed to observe {}",
                function_name!(),
                app.inner().name
            );
            return;
        }

        debug!(
            "{}: Adding {} to list of apps.",
            function_name!(),
            app.inner().name
        );
        window_manager
            .applications
            .insert(app.inner().pid, app.clone());

        // int window_count;
        // struct window **window_list = window_manager_add_application_windows(
        //     &g_space_manager, &g_window_manager, application, &window_count);
        // uint32_t prev_window_id = g_window_manager.focused_window_id;
        if let Some(windows) = window_manager.add_application_windows(&app) {
            debug!(
                "{}: Added windows {} for {}.",
                function_name!(),
                windows
                    .iter()
                    .map(|window| format!("{}", window.inner().id))
                    .collect::<Vec<_>>()
                    .join(", "),
                app.inner().name
            );

            if let Some(active_panel) = window_manager.active_panel() {
                active_panel.force_write().extend(windows.iter().cloned())
            }
        };

        // uint64_t sid;
        // bool default_origin =
        //     g_window_manager.window_origin_mode == WINDOW_ORIGIN_DEFAULT;
        //
        // if (!default_origin) {
        //   if (g_window_manager.window_origin_mode == WINDOW_ORIGIN_FOCUSED) {
        //     sid = g_space_manager.current_space_id;
        //   } else /* if (g_window_manager.window_origin_mode == WINDOW_ORIGIN_CURSOR)
        //           */
        //   {
        //     sid = space_manager_cursor_space();
        //   }
        // }
        //
        // int view_count = 0;
        // struct view **view_list = ts_alloc_list(struct view *, window_count);
        //
        // for (int i = 0; i < window_count; ++i) {
        //   struct window *window = window_list[i];
        //
        //   if (window_manager_should_manage_window(window) &&
        //       !window_manager_find_managed_window(&g_window_manager, window)) {
        //     if (default_origin)
        //       sid = window_space(window->id);
        //
        //     struct view *view = space_manager_find_view(&g_space_manager, sid);
        //     if (view->layout != VIEW_FLOAT) {
        //       //
        //       // @cleanup
        //       //
        //       // :AXBatching
        //       //
        //       // NOTE(koekeishiya): Batch all operations and mark the view as dirty so
        //       // that we can perform a single flush, making sure that each window is
        //       // only moved and resized a single time, when the final layout has been
        //       // computed. This is necessary to make sure that we do not call the AX
        //       // API for each modification to the tree.
        //       //
        //
        //       window_manager_adjust_layer(window, LAYER_BELOW);
        //       view_add_window_node_with_insertion_point(view, window, prev_window_id);
        //       window_manager_add_managed_window(&g_window_manager, window, view);
        //
        //       view_set_flag(view, VIEW_IS_DIRTY);
        //       view_list[view_count++] = view;
        //
        //       prev_window_id = window->id;
        //     }
        //   }
        //
        //   if (window_manager_is_window_eligible(window)) {
        //     event_signal_push(SIGNAL_WINDOW_CREATED, window);
        //   }
        // }
        //
        // //
        // // @cleanup
        // //
        // // :AXBatching
        // //
        // // NOTE(koekeishiya): Flush previously batched operations if the view is
        // // marked as dirty. This is necessary to make sure that we do not call the AX
        // // API for each modification to the tree.
        // //
        //
        // for (int i = 0; i < view_count; ++i) {
        //   struct view *view = view_list[i];
        //   if (!space_is_visible(view->sid))
        //     continue;
        //   if (!view_is_dirty(view))
        //     continue;
        //
        //   window_node_flush(view->root);
        //   view_clear_flag(view, VIEW_IS_DIRTY);
        // }
        //
        // if (workspace_is_macos_sequoia()) {
        //   update_window_notifications();
        // }
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
                let ptr = NonNull::from(self).as_ptr() as *mut c_void;
                app.addObserver_forKeyPath_options_context(
                    self.observer.deref(),
                    key_path.as_ref(),
                    options,
                    ptr,
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
                let ptr = NonNull::from(self).as_ptr() as *mut c_void;
                app.removeObserver_forKeyPath_context(
                    self.observer.deref(),
                    key_path.as_ref(),
                    ptr,
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
    pub fn find_process(&mut self, psn: &ProcessSerialNumber) -> Option<&mut Pin<Box<Process>>> {
        self.processes.get_mut(psn)
    }

    pub fn process_add(
        &mut self,
        psn: &ProcessSerialNumber,
        observer: Retained<WorkspaceObserver>,
    ) -> Option<&mut Pin<Box<Process>>> {
        if self.processes.contains_key(psn) {
            return self.find_process(psn);
        }

        let mut pinfo = ProcessInfo::default();
        unsafe {
            get_process_info(psn, &mut pinfo);
        }
        let name = match NonNull::new(pinfo.name as *mut CFString) {
            Some(name) => unsafe { name.as_ref() }.to_string(),
            None => {
                error!("{}: nullptr 'name' passed.", function_name!());
                return None;
            }
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
