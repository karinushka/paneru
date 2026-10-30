use arc_swap::ArcSwap;
use core::ptr::NonNull;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventType, NSTouch, NSTouchPhase};
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use objc2_foundation::NSSet;
use scopeguard::ScopeGuard;
use std::ffi::c_void;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use stdext::function_name;
use tracing::{error, info, warn};

use crate::commands::{Command, Direction, Operation};
use crate::config::Config;
use crate::config::swipe::focus_direction;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::platform::Modifiers;

const NX_DEVICEFNKEYMASK: u64 = 0x0080_0100;

/// The currently active set of passthrough keybindings, shared lock-free with
/// the `CGEvent` tap callback thread via `ArcSwap`.
static FOCUSED_PASSTHROUGH: LazyLock<ArcSwap<Vec<(u8, Modifiers)>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replace the passthrough keybinding set that the event tap checks on every
/// key-down. Called from the ECS thread on focus change and config reload.
pub fn set_focused_passthrough(keys: Vec<(u8, Modifiers)>) {
    FOCUSED_PASSTHROUGH.store(Arc::new(keys));
}

/// How long to suppress scroll wheel events after a vertical swipe gesture,
/// covering macOS momentum scroll that continues after finger lift.
const VERTICAL_GESTURE_SCROLL_SUPPRESS: Duration = Duration::from_millis(1200);

/// Limit repeated wheel inputs while the next window moves into view.
const WINDOW_SCROLL_INTERVAL: Duration = Duration::from_millis(180);

/// One finger, as a single gesture event saw it.
struct Touch<I> {
    identity: I,
    x: f64,
    y: f64,
    device_width_points: f64,
    /// Whether the finger landed this event, which starts a new sample series.
    began: bool,
}

impl Touch<Retained<AnyObject>> {
    /// Samples the whole set in one pass.
    fn sample(touches: &NSSet<NSTouch>) -> Vec<Self> {
        touches
            .iter()
            .map(|touch| {
                let position = touch.normalizedPosition();
                Self {
                    identity: touch.identity(),
                    x: position.x,
                    y: position.y,
                    device_width_points: touch.deviceSize().width,
                    began: touch.phase() == NSTouchPhase::Began,
                }
            })
            .collect()
    }
}

#[derive(Debug, PartialEq)]
enum SwipeMovement {
    Horizontal {
        delta: f64,
        distance_cm: Option<f64>,
    },
    Vertical(f64),
}

/// Compare consecutive touch samples. Matching identities matters because
/// `NSSet` can change iteration order between gesture events.
fn swipe_movement<I>(
    previous_touches: &[Touch<I>],
    touches: &[Touch<I>],
    same_identity: impl Fn(&I, &I) -> bool,
) -> Option<SwipeMovement> {
    if previous_touches.len() != touches.len() || previous_touches.is_empty() {
        return None;
    }

    let deltas = touches
        .iter()
        .map(|current| {
            previous_touches
                .iter()
                .find(|previous| same_identity(&previous.identity, &current.identity))
                .map(|previous| {
                    (
                        previous.x - current.x,
                        previous.y - current.y,
                        current.device_width_points,
                    )
                })
        })
        .collect::<Option<Vec<_>>>()?;

    let x_sum: f64 = deltas.iter().map(|(x, _, _)| x).sum();
    let y_sum: f64 = deltas.iter().map(|(_, y, _)| y).sum();
    let horizontal = x_sum.abs() >= y_sum.abs();
    let moved_fingers = deltas
        .iter()
        .filter(|(x, y, _)| (if horizontal { *x } else { *y }).abs() > SWIPE_THRESHOLD)
        .count();
    if moved_fingers == 0 {
        return None;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "a gesture cannot have enough fingers to lose precision"
    )]
    let moved_fingers = moved_fingers as f64;
    if horizontal {
        Some(SwipeMovement::Horizontal {
            delta: x_sum / moved_fingers,
            distance_cm: physical_swipe_distance_cm(
                deltas
                    .iter()
                    .filter(|(x, _, _)| x.abs() > SWIPE_THRESHOLD)
                    .map(|(x, _, width)| (*x, *width)),
            ),
        })
    } else {
        Some(SwipeMovement::Vertical(y_sum / moved_fingers))
    }
}

/// Keybinds registered by the Lua runtime as `(keycode, modifiers, handler_id)`,
/// shared lock-free with the event tap. Checked before the config bindings so a
/// scripted bind can override a TOML one.
static LUA_KEYBINDS: LazyLock<ArcSwap<Vec<(u8, Modifiers, u32)>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replace the Lua keybind set that the event tap checks on every key-down.
/// Called from the main thread on script load and hot reload.
#[cfg(feature = "lua")]
pub fn set_lua_keybinds(keys: Vec<(u8, Modifiers, u32)>) {
    LUA_KEYBINDS.store(Arc::new(keys));
}

const SWIPE_THRESHOLD: f64 = 0.0005;
const GESTURE_MINIMAL_FINGERS: usize = 3;

/// Outcome of an event tap health check.
///
/// A dead tap is silent: the socket keeps answering and windows keep tiling on
/// command, so only an explicit check distinguishes it from a healthy daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TapHealth {
    Healthy,
    Reenabled,
    Rebuilt,
    Failed,
}

/// `InputHandler` manages low-level input events from the macOS `CGEventTap`.
/// It intercepts keyboard and mouse events, processes gestures, and dispatches them as higher-level `Event`s.
pub(super) struct InputHandler {
    /// The `EventSender` for dispatching input events.
    events: Option<EventSender>,
    /// The application `Config` for looking up keybindings.
    config: Config,
    /// The preceding gesture event's touch positions.
    previous_touches: Vec<Touch<Retained<AnyObject>>>,
    /// Timestamp of `previous_touches`.
    last_swipe_sample_timestamp: Option<f64>,
    /// The `CFMachPort` representing the `CGEventTap`.
    tap_port: Option<CFRetained<CFMachPort>>,
    /// Run loop source feeding `tap_port`. Retained so the tap can be torn down
    /// and rebuilt when macOS invalidates the mach port across a sleep cycle.
    run_loop_source: Option<CFRetained<CFRunLoopSource>>,
    /// Timestamp of the last swipe gesture event. Scroll wheel events
    /// are suppressed for a short window after this to prevent the OS from
    /// scrolling windows underneath (including momentum scroll after finger lift).
    last_swipe_time: Option<Instant>,
    last_window_scroll: Option<(Instant, Direction)>,
    invalid_device_size_warned: bool,
    // Prevents from being Unpin automatically
    _pin: PhantomPinned,
}

pub(super) type PinnedInputHandler =
    ScopeGuard<Pin<Box<InputHandler>>, Box<dyn FnOnce(Pin<Box<InputHandler>>)>>;

impl InputHandler {
    /// Creates a new `InputHandler` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send input-related events.
    /// * `config` - The `Config` object for looking up keybindings.
    ///
    /// # Returns
    ///
    /// A new `InputHandler`.
    pub(super) fn new(events: EventSender, config: Config) -> Self {
        InputHandler {
            events: Some(events),
            config,
            previous_touches: Vec::new(),
            last_swipe_sample_timestamp: None,
            tap_port: None,
            run_loop_source: None,
            last_swipe_time: None,
            last_window_scroll: None,
            invalid_device_size_warned: false,
            _pin: PhantomPinned,
        }
    }

    /// Starts the input handler by creating and enabling a `CGEventTap`. It also sets up a cleanup hook.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event tap is created and started successfully, otherwise `Err(Error)`.
    pub(super) fn start(self) -> Result<PinnedInputHandler> {
        let mut pinned = Box::pin(self);
        // Safety: the handler stays pinned in its Box for the life of the
        // daemon, so the pointer handed to the tap callback stays valid.
        unsafe { pinned.as_mut().get_unchecked_mut() }.install_tap()?;

        Ok(scopeguard::guard(
            pinned,
            Box::new(|mut handler: Pin<Box<Self>>| {
                info!("Unregistering event_handler");
                // Safety: as above. Tearing the tap down does not move it.
                unsafe { handler.as_mut().get_unchecked_mut() }.uninstall_tap();
            }),
        ))
    }

    /// Creates the `CGEventTap` and wires its mach port into the main run loop.
    ///
    /// The caller must keep `self` pinned: the tap callback holds a raw pointer
    /// back to this handler and dereferences it on every event.
    fn install_tap(&mut self) -> Result<()> {
        let mouse_event_mask = (1 << CGEventType::MouseMoved.0)
            | (1 << CGEventType::LeftMouseDown.0)
            | (1 << CGEventType::LeftMouseUp.0)
            | (1 << CGEventType::LeftMouseDragged.0)
            | (1 << CGEventType::RightMouseDown.0)
            | (1 << CGEventType::RightMouseUp.0)
            | (1 << CGEventType::RightMouseDragged.0)
            | (1 << CGEventType::ScrollWheel.0)
            | (1 << NSEventType::Gesture.0)
            | (1 << CGEventType::KeyDown.0);

        let this = NonNull::from(&mut *self).as_ptr();
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mouse_event_mask,
                Some(Self::callback),
                this.cast(),
            )
        }
        .ok_or_else(|| {
            Error::PermissionDenied(format!("{}: Can not create EventTap.", function_name!()))
        })?;

        let (run_loop_source, main_loop) = CFMachPort::new_run_loop_source(None, Some(&port), 0)
            .zip(CFRunLoop::main())
            .ok_or_else(|| {
                Error::PermissionDenied(format!(
                    "{}: Unable to create run loop source",
                    function_name!()
                ))
            })?;
        CFRunLoop::add_source(&main_loop, Some(&run_loop_source), unsafe {
            kCFRunLoopCommonModes
        });

        self.tap_port = Some(port);
        self.run_loop_source = Some(run_loop_source);
        Ok(())
    }

    fn uninstall_tap(&mut self) {
        if let Some(run_loop_source) = self.run_loop_source.take()
            && let Some(main_loop) = CFRunLoop::main()
        {
            CFRunLoop::remove_source(&main_loop, Some(&run_loop_source), unsafe {
                kCFRunLoopCommonModes
            });
        }
        if let Some(port) = self.tap_port.take()
            && port.is_valid()
        {
            CGEvent::tap_enable(&port, false);
            CFMachPort::invalidate(&port);
        }
    }

    /// Verifies the tap still delivers events, and revives it when it does not.
    ///
    /// macOS can invalidate the tap's mach port across a sleep cycle without
    /// ever delivering `TapDisabled` to the callback. No further event then
    /// reaches the callback, so the re-arm branch there can never fire and the
    /// tap stays dead until the daemon restarts. Re-enabling an invalid port is
    /// a no-op, so that case needs a full rebuild.
    pub(super) fn ensure_tap_alive(&mut self) -> TapHealth {
        let Some(port) = self.tap_port.clone().filter(|port| port.is_valid()) else {
            return self.rebuild_tap();
        };

        if CGEvent::tap_is_enabled(&port) {
            return TapHealth::Healthy;
        }

        CGEvent::tap_enable(&port, true);
        if CGEvent::tap_is_enabled(&port) {
            warn!("event tap was disabled, re-enabled it");
            TapHealth::Reenabled
        } else {
            self.rebuild_tap()
        }
    }

    fn rebuild_tap(&mut self) -> TapHealth {
        self.uninstall_tap();
        match self.install_tap() {
            Ok(()) => {
                warn!("event tap was unusable, rebuilt it");
                TapHealth::Rebuilt
            }
            Err(err) => {
                error!("unable to rebuild event tap: {err}");
                TapHealth::Failed
            }
        }
    }

    /// The C-callback function for the `CGEventTap`. It dispatches to the `input_handler` method.
    /// This function is declared as `extern "C-unwind"`.
    ///
    /// # Arguments
    ///
    /// * `_` - The `CGEventTapProxy` (unused).
    /// * `event_type` - The `CGEventType` of the event.
    /// * `event_ref` - A mutable `NonNull` pointer to the `CGEvent`.
    /// * `this` - A raw pointer to the `InputHandler` instance.
    ///
    /// # Returns
    ///
    /// A raw mutable pointer to `CGEvent`. Returns `null_mut()` if the event is intercepted.
    extern "C-unwind" fn callback(
        _: CGEventTapProxy,
        event_type: CGEventType,
        mut event_ref: NonNull<CGEvent>,
        this: *mut c_void,
    ) -> *mut CGEvent {
        if let Some(this) =
            NonNull::new(this).map(|this| unsafe { this.cast::<InputHandler>().as_mut() })
        {
            let intercept = this.input_handler(event_type, unsafe { event_ref.as_ref() });
            if intercept {
                return null_mut();
            }
        } else {
            error!("Zero passed to Event Handler.");
        }
        unsafe { event_ref.as_mut() }
    }

    /// Handles various input events received from the `CGEventTap` callback. It sends corresponding `Event`s.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The `CGEventType` of the event.
    /// * `event` - A reference to the `CGEvent`.
    ///
    /// # Returns
    ///
    /// `true` if the event should be intercepted (not passed further), `false` otherwise.
    fn input_handler(&mut self, event_type: CGEventType, event: &CGEvent) -> bool {
        // Re-arm before any early return can swallow it. This is the only
        // notice macOS gives that it switched the tap off, and dropping it
        // leaves paneru deaf for the rest of the process lifetime.
        if matches!(
            event_type,
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
        ) {
            warn!("Tap Disabled, re-enabling");
            if let Some(port) = &self.tap_port {
                CGEvent::tap_enable(port, true);
            }
            return false;
        }

        let Some(events) = &self.events else {
            return false;
        };

        let flags = CGEvent::flags(Some(event));
        let modifiers = get_modifiers(flags);

        let result = match event_type {
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseDown { point, modifiers })
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseUp { point, modifiers })
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseDragged { point, modifiers })
            }
            CGEventType::MouseMoved => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseMoved { point, modifiers })
            }
            CGEventType::KeyDown => {
                let keycode =
                    CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
                // handle_keypress can intercept the event, so it may return true.
                return self.handle_keypress(keycode, flags);
            }
            CGEventType::ScrollWheel => {
                return self.handle_scroll_wheel(event);
            }
            // Returns directly: handle_swipe returns bool (intercept flag)
            // rather than Result like the other arms.
            _ => {
                return self.handle_swipe(event);
            }
        };
        if let Err(err) = result {
            error!("error sending event: {err}");
            // The socket is dead, so no use trying to send to it.
            // Trigger cleanup destructor, unregistering the handler.
            self.events = None;
        }
        // Do not intercept this event, let it fall through.
        false
    }

    /// Use the configured modifier to scroll the strip or select an adjacent window.
    fn handle_scroll_wheel(&mut self, event: &CGEvent) -> bool {
        // Suppress scroll events shortly after a swipe gesture to prevent
        // the OS from scrolling windows underneath, including momentum scroll events
        // that arrive after finger lift.
        if self
            .last_swipe_time
            .is_some_and(|t| t.elapsed() < VERTICAL_GESTURE_SCROLL_SUPPRESS)
        {
            return true;
        }

        let flags = CGEvent::flags(Some(event));
        let modifiers = get_scroll_modifiers(flags);

        let target_modifier = self.config.swipe_scroll_modifier();
        let vertical_mod = self.config.swipe_scroll_vertical_modifier();

        // Check the combined modifier (base + vertical) first, then fall back to
        // base-only. matches() rejects extra modifier groups, so cmd+shift held
        // would fail a cmd-only check. We need to accept both.
        let base_match = target_modifier.matches(modifiers);
        let combined_match =
            vertical_mod.is_some_and(|vm| (target_modifier | vm).matches(modifiers));
        if !combined_match && !base_match {
            return false;
        }

        if let Some(events) = &self.events {
            let h_delta = CGEvent::double_value_field(
                Some(event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
            );
            let v_delta = CGEvent::double_value_field(
                Some(event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
            );

            // Vertical workspace switching when vertical modifier is also held.
            // Don't set last_vertical_gesture here: the suppress timer is for
            // trackpad momentum scroll, not discrete wheel ticks.
            if combined_match && v_delta.abs() > 0.001 {
                _ = events.send(Event::VerticalScrollTick { delta: v_delta });
                return true;
            }

            // If we have any horizontal delta, or if there's only vertical delta, use it.
            let delta = if h_delta.abs() > 0.001 {
                h_delta
            } else if v_delta.abs() > 0.001 {
                v_delta
            } else {
                0.0
            };

            if delta.abs() > 0.001 {
                if self.config.swipe_scroll_window_step() {
                    let direction = scroll_step_direction(&self.config, delta);
                    let momentum = CGEvent::integer_value_field(
                        Some(event),
                        CGEventField::ScrollWheelEventMomentumPhase,
                    );
                    if accept_window_scroll(
                        &mut self.last_window_scroll,
                        &direction,
                        momentum != 0,
                        Instant::now(),
                    ) {
                        _ = events.send(Event::Command {
                            command: Command::Window(Operation::Focus(direction)),
                        });
                    }
                } else {
                    _ = events.send(Event::Scroll { delta });
                }
                return true; // Intercept: don't let the window scroll
            }
        }
        false
    }

    /// Handles swipe gesture events. Routes to horizontal `Swipe` or vertical
    /// `VerticalSwipe` based on axis dominance. Returns true to intercept the event.
    fn handle_swipe(&mut self, event: &CGEvent) -> bool {
        const NS_EVENT_PHASE_ENDED: usize = 1 << 3; // 8
        const NS_EVENT_PHASE_CANCELLED: usize = 1 << 4; // 16

        let Some(configured_fingers) = self
            .config
            .swipe_gesture_fingers()
            .filter(|fingers| *fingers >= GESTURE_MINIMAL_FINGERS)
        else {
            // No valid gesture is configured, so leave native macOS gestures alone.
            return false;
        };

        objc2::rc::autoreleasepool(|_| {
            let Some(ns_event) = NSEvent::eventWithCGEvent(event) else {
                error!("{}: Unable to convert CGEvent to NSEvent", function_name!());
                return false;
            };
            if ns_event.r#type() != NSEventType::Gesture {
                return false;
            }

            let timestamp = ns_event.timestamp();

            // Fingers lifted off touchpad.
            let phase = ns_event.phase();
            if phase.0 & NS_EVENT_PHASE_CANCELLED != 0 || phase.0 & NS_EVENT_PHASE_ENDED != 0 {
                self.previous_touches.clear();
                self.invalid_device_size_warned = false;
                self.last_swipe_sample_timestamp = None;
                if let Some(events) = &self.events {
                    _ = events.send(Event::TouchpadUp);
                }
                return false;
            }

            let fingers = Touch::sample(&ns_event.allTouches());
            if !gesture_should_intercept(Some(configured_fingers), fingers.len()) {
                // AppKit interleaves phase events with an empty touch set
                // between usable samples. They do not mean the fingers lifted.
                // That's why empty finger counts don't trigger a reset.
                if should_reset_swipe_samples(fingers.len(), self.previous_touches.len()) {
                    self.previous_touches.clear();
                    self.last_swipe_sample_timestamp = None;
                }
                return false;
            }
            if fingers.iter().any(|finger| finger.began) {
                self.invalid_device_size_warned = false;
                // A fresh gesture: nothing carries over from the last one.
                self.previous_touches.clear();
                self.last_swipe_sample_timestamp = None;
                if let Some(events) = &self.events {
                    _ = events.send(Event::TouchpadDown);
                }
            }

            let movement = swipe_movement(&self.previous_touches, &fingers, |previous, current| {
                // SAFETY: NSTouch identities conform to NSObject and answer isEqual:.
                unsafe { msg_send![&**previous, isEqual: &**current] }
            });
            let elapsed_secs = self
                .last_swipe_sample_timestamp
                .replace(timestamp)
                .map(|previous| (timestamp - previous).max(0.0));
            self.previous_touches = fingers;
            match movement {
                Some(SwipeMovement::Horizontal { delta, distance_cm }) => {
                    if distance_cm.is_none() && !self.invalid_device_size_warned {
                        warn!("trackpad did not provide a valid device size for paging");
                        self.invalid_device_size_warned = true;
                    }
                    if let Some(events) = &self.events {
                        let _ = events.send(Event::Swipe {
                            delta,
                            fingers: configured_fingers,
                            distance_cm: distance_cm.unwrap_or(0.0),
                            elapsed_secs,
                        });
                        self.last_swipe_time = Some(Instant::now());
                    }
                }
                Some(SwipeMovement::Vertical(delta)) if self.config.swipe_vertical() => {
                    if let Some(events) = &self.events {
                        let _ = events.send(Event::VerticalSwipe {
                            delta,
                            fingers: configured_fingers,
                        });
                        self.last_swipe_time = Some(Instant::now());
                    }
                }
                Some(SwipeMovement::Vertical(_)) => return false,
                None => (),
            }

            // If we have 3 or more fingers on the trackpad, we intercept the event
            // to prevent it from being interpreted as a scroll by the OS.
            true
        })
    }

    /// Handles key press events. It determines the modifier mask and attempts to find a matching keybinding in the configuration.
    /// If a binding is found, it sends a `Command` event and intercepts the key press.
    ///
    /// # Arguments
    ///
    /// * `keycode` - The key code of the pressed key.
    /// * `eventflags` - The `CGEventFlags` representing active modifiers.
    ///
    /// # Returns
    ///
    /// `true` if the key press was handled and should be intercepted, `false` otherwise.
    fn handle_keypress(&self, keycode: i64, eventflags: CGEventFlags) -> bool {
        let Some(events) = &self.events else {
            return false;
        };

        let mask = get_modifiers(eventflags);

        // On a native fullscreen space, keybindings are still intercepted so
        // that paneru can actively switch back to the previous workspace.
        // Non-paneru keys pass through naturally (find_keybind returns None).

        let keycode = keycode.try_into().ok();
        keycode
            .and_then(|keycode| {
                let passthrough = FOCUSED_PASSTHROUGH.load();
                if passthrough
                    .iter()
                    .any(|(c, m)| *c == keycode && m.matches(mask))
                {
                    return None;
                }
                // Lua-registered binds take precedence over the TOML config.
                let lua_binds = LUA_KEYBINDS.load();
                if let Some((_, _, id)) = lua_binds
                    .iter()
                    .find(|(c, m, _)| *c == keycode && m.matches(mask))
                {
                    return Some(Command::Lua(*id));
                }
                self.config.find_keybind(keycode, mask)
            })
            .and_then(|command| {
                events
                    .send(Event::Command { command })
                    .inspect_err(|err| error!("Error sending command: {err}"))
                    .ok()
            })
            .is_some()
    }
}

fn gesture_should_intercept(configured_fingers: Option<usize>, actual_fingers: usize) -> bool {
    configured_fingers.is_some_and(|configured| {
        configured >= GESTURE_MINIMAL_FINGERS && configured == actual_fingers
    })
}

fn should_reset_swipe_samples(actual_fingers: usize, previous_fingers: usize) -> bool {
    actual_fingers != 0 && actual_fingers != previous_fingers
}

// Remote mouse tools can send group flags without left or right flags.
// Use the group only when the event does not specify a side.
fn get_scroll_modifiers(eventflags: CGEventFlags) -> Modifiers {
    const GROUP_FLAGS: [(Modifiers, u64); 4] = [
        (Modifiers::ALT, CGEventFlags::MaskAlternate.0),
        (Modifiers::SHIFT, CGEventFlags::MaskShift.0),
        (Modifiers::CMD, CGEventFlags::MaskCommand.0),
        (Modifiers::CTRL, CGEventFlags::MaskControl.0),
    ];

    GROUP_FLAGS
        .iter()
        .fold(get_modifiers(eventflags), |modifiers, (group, flag)| {
            if !modifiers.intersects(*group) && eventflags.0 & flag != 0 {
                modifiers | *group
            } else {
                modifiers
            }
        })
}

fn get_modifiers(eventflags: CGEventFlags) -> Modifiers {
    const MODIFIER_MASKS: [(Modifiers, u64); 8] = [
        (Modifiers::LALT, 0x0000_0020),
        (Modifiers::RALT, 0x0000_0040),
        (Modifiers::LSHIFT, 0x0000_0002),
        (Modifiers::RSHIFT, 0x0000_0004),
        (Modifiers::LCMD, 0x0000_0008),
        (Modifiers::RCMD, 0x0000_0010),
        (Modifiers::LCTRL, 0x0000_0001),
        (Modifiers::RCTRL, 0x0000_2000),
    ];

    // Fn key should be checked for separately, because pressing
    // some keys (i.e. leftarrow) seems to inadvertently toggling it.
    if eventflags.0 == NX_DEVICEFNKEYMASK {
        tracing::debug!("event flags {:#x}", eventflags.0);
        return Modifiers::FN;
    }

    MODIFIER_MASKS
        .iter()
        .fold(Modifiers::empty(), |modifiers, (modifier, mask)| {
            if eventflags.0 & mask != 0 {
                modifiers | *modifier
            } else {
                modifiers
            }
        })
}

fn scroll_step_direction(config: &Config, delta: f64) -> Direction {
    focus_direction(config.swipe_gesture_direction(), delta)
}

fn physical_swipe_distance_cm(deltas: impl IntoIterator<Item = (f64, f64)>) -> Option<f64> {
    let (total, count) = deltas.into_iter().try_fold(
        (0.0, 0_u32),
        |(total, count), (normalized_delta, width_points)| {
            (normalized_delta.is_finite() && width_points.is_finite() && width_points > 0.0)
                .then_some((
                    total + normalized_delta * width_points * 2.54 / 72.0,
                    count + 1,
                ))
        },
    )?;
    (count > 0).then_some(total / f64::from(count))
}

fn accept_window_scroll(
    last: &mut Option<(Instant, Direction)>,
    direction: &Direction,
    momentum: bool,
    now: Instant,
) -> bool {
    if momentum
        || last.as_ref().is_some_and(|(time, previous)| {
            previous == direction && now.duration_since(*time) < WINDOW_SCROLL_INTERVAL
        })
    {
        return false;
    }
    *last = Some((now, direction.clone()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_fingers(x: f64) -> Vec<Touch<usize>> {
        (0..3)
            .map(|identity| Touch {
                identity,
                x,
                y: 0.0,
                device_width_points: 144.0,
                began: false,
            })
            .collect()
    }

    fn movement(previous: &[Touch<usize>], current: &[Touch<usize>]) -> Option<SwipeMovement> {
        swipe_movement(previous, current, |a, b| a == b)
    }

    fn assert_horizontal(movement: Option<SwipeMovement>, expected: f64) {
        let Some(SwipeMovement::Horizontal { delta, .. }) = movement else {
            panic!("expected horizontal swipe, got {movement:?}");
        };
        assert!((delta - expected).abs() < 1e-12, "{delta} != {expected}");
    }

    #[test]
    fn horizontal_sample_reports_its_physical_distance() {
        let actual = movement(&three_fingers(0.0), &three_fingers(0.0006));
        let Some(SwipeMovement::Horizontal { delta, distance_cm }) = actual else {
            panic!("expected horizontal swipe, got {actual:?}");
        };
        assert!((delta + 0.0006).abs() < 1e-12);
        assert_eq!(distance_cm, Some(-0.003_048));
    }

    #[test]
    fn subthreshold_samples_do_not_accumulate() {
        assert_eq!(movement(&three_fingers(0.0), &three_fingers(0.0002)), None);
        assert_eq!(
            movement(&three_fingers(0.0002), &three_fingers(0.0004)),
            None
        );
        assert_eq!(
            movement(&three_fingers(0.0004), &three_fingers(0.0006)),
            None
        );
        assert_horizontal(
            movement(&three_fingers(0.0006), &three_fingers(0.0012)),
            -0.0006,
        );
    }

    #[test]
    fn subthreshold_reversal_is_ignored() {
        assert_eq!(movement(&three_fingers(0.0), &three_fingers(0.0004)), None);
        assert_eq!(
            movement(&three_fingers(0.0004), &three_fingers(0.0001)),
            None
        );
    }

    #[test]
    fn still_fingers_emit_no_motion() {
        assert_eq!(
            movement(&three_fingers(0.0004), &three_fingers(0.0004)),
            None
        );
    }

    #[test]
    fn finger_replacement_does_not_report_a_jump() {
        let mut replacement = three_fingers(0.002);
        replacement[2].identity = 3;
        assert_eq!(movement(&three_fingers(0.0004), &replacement), None);
        let mut next = three_fingers(0.0026);
        next[2].identity = 3;
        assert_horizontal(movement(&replacement, &next), -0.0006);
    }

    #[test]
    fn finger_order_changes_do_not_change_delta() {
        let mut current = three_fingers(0.0006);
        current.reverse();
        assert_horizontal(movement(&three_fingers(0.0), &current), -0.0006);
    }

    #[test]
    fn sustained_swipe_continues_when_one_finger_lags() {
        let mut next = three_fingers(0.004);
        next[2].x = 0.002;
        assert_horizontal(movement(&three_fingers(0.002), &next), -0.004 / 2.0);

        let mut later = three_fingers(0.006);
        later[2].x = 0.002;
        assert_horizontal(movement(&next, &later), -0.004 / 2.0);
    }

    #[test]
    fn one_moving_finger_can_start_a_swipe() {
        let mut one_finger_moves = three_fingers(0.0);
        one_finger_moves[0].x = 0.02;
        assert_horizontal(movement(&three_fingers(0.0), &one_finger_moves), -0.02);
    }

    #[test]
    fn one_moving_finger_sets_vertical_delta() {
        let previous = three_fingers(0.0);
        let mut current = three_fingers(0.0);
        current[0].y = 0.02;
        let actual = movement(&previous, &current);
        assert_eq!(actual, Some(SwipeMovement::Vertical(-0.02)));
    }

    #[test]
    fn fast_vertical_motion_keeps_its_existing_direction() {
        let vertical = |y| {
            (0..3)
                .map(|identity| Touch {
                    identity,
                    x: 0.0,
                    y,
                    device_width_points: 144.0,
                    began: false,
                })
                .collect::<Vec<_>>()
        };
        let actual = movement(&vertical(0.0), &vertical(0.002));
        let Some(SwipeMovement::Vertical(delta)) = actual else {
            panic!("expected vertical swipe, got {actual:?}");
        };
        assert!((delta + 0.002).abs() < 1e-12);
    }

    fn step_config(extra: &str) -> Config {
        Config::try_from(format!(
            "[options]\n[bindings]\n[swipe.scroll]\nmodifier = 'alt'\nwindow_step = true\n{extra}"
        ).as_str()).expect("valid test config")
    }

    #[test]
    fn wheel_steps_use_delta_sign_and_configured_direction() {
        for reversed in [false, true] {
            let config = step_config(if reversed {
                "[swipe.gesture]\ndirection = 'Reversed'\n"
            } else {
                ""
            });
            for delta in [0.1, 1.0, 3.0, 120.0] {
                for signed_delta in [delta, -delta] {
                    let direction = scroll_step_direction(&config, signed_delta);
                    let expected = if (signed_delta > 0.0) == reversed {
                        Direction::West
                    } else {
                        Direction::East
                    };
                    assert_eq!(direction, expected);
                }
            }
        }
    }

    #[test]
    fn physical_swipe_distance_averages_finger_travel_and_preserves_sign() {
        // 144 device points at 72 ppi is 5.08 cm wide; half its width is 2.54 cm.
        let left = [(0.5, 144.0); 3];
        assert_eq!(physical_swipe_distance_cm(left), Some(2.54));
        let right = [(-0.5, 144.0); 4];
        assert_eq!(physical_swipe_distance_cm(right), Some(-2.54));
        assert_eq!(physical_swipe_distance_cm([(0.5, 0.0)]), None);
    }

    #[test]
    fn wheel_scroll_stays_free_unless_step_mode_is_enabled() {
        assert!(!Config::default().swipe_scroll_window_step());
        let disabled = Config::try_from("[swipe.scroll]\nwindow_step = false\n").unwrap();
        assert!(!disabled.swipe_scroll_window_step());
        let enabled = Config::try_from("[swipe.scroll]\nwindow_step = true\n").unwrap();
        assert!(enabled.swipe_scroll_window_step());
    }

    #[test]
    fn wheel_steps_limit_repeats_but_allow_immediate_reversal() {
        let now = Instant::now();
        let mut last = None;
        assert!(accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now
        ));
        assert!(!accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now + Duration::from_millis(100)
        ));
        assert!(accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now + WINDOW_SCROLL_INTERVAL
        ));
        assert!(accept_window_scroll(
            &mut last,
            &Direction::West,
            false,
            now + WINDOW_SCROLL_INTERVAL
        ));
    }

    #[test]
    fn wheel_momentum_does_not_select_a_window_or_delay_the_next_input() {
        let now = Instant::now();
        let mut last = None;
        assert!(!accept_window_scroll(
            &mut last,
            &Direction::East,
            true,
            now
        ));
        assert!(last.is_none());
        assert!(accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now
        ));
        assert!(!accept_window_scroll(
            &mut last,
            &Direction::West,
            true,
            now
        ));
        assert!(accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now + WINDOW_SCROLL_INTERVAL
        ));
    }

    #[test]
    fn wheel_steps_focus_adjacent_windows_and_stop_at_strip_edges() {
        use crate::tests::TestHarness;

        let config = step_config("");
        let mut events = vec![Event::MenuOpened { window_id: 0 }];
        events.extend(
            [120.0, 3.0, 3.0, -120.0, -3.0, -3.0].map(|delta| Event::Command {
                command: Command::Window(Operation::Focus(scroll_step_direction(&config, delta))),
            }),
        );
        let mut harness = TestHarness::new().with_config(config).with_windows(3);
        for (iteration, expected) in [0, 1, 2, 2, 1, 0, 0].into_iter().enumerate() {
            harness = harness.on_iteration(iteration, move |world, _| {
                crate::assert_focused!(world, expected);
                assert_eq!(
                    world.query::<&crate::ecs::Scrolling>().iter(world).count(),
                    0
                );
            });
        }
        harness.run(events);
    }

    const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
    const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
    const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
    const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
    const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
    const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
    const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
    const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;

    #[test]
    fn no_modifiers() {
        assert_eq!(get_modifiers(CGEventFlags(0)), Modifiers::empty());
    }

    #[test]
    fn fn_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICEFNKEYMASK)),
            Modifiers::FN
        );
    }

    #[test]
    fn single_left_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELALTKEYMASK)),
            Modifiers::LALT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELSHIFTKEYMASK)),
            Modifiers::LSHIFT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELCMDKEYMASK)),
            Modifiers::LCMD
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELCTLKEYMASK)),
            Modifiers::LCTRL
        );
    }

    #[test]
    fn single_right_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERALTKEYMASK)),
            Modifiers::RALT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERSHIFTKEYMASK)),
            Modifiers::RSHIFT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERCMDKEYMASK)),
            Modifiers::RCMD
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERCTLKEYMASK)),
            Modifiers::RCTRL
        );
    }

    #[test]
    fn both_sides_of_same_modifier() {
        let flags = NX_DEVICELALTKEYMASK | NX_DEVICERALTKEYMASK;
        assert_eq!(get_modifiers(CGEventFlags(flags)), Modifiers::ALT);
    }

    #[test]
    fn multiple_different_modifiers() {
        let flags = NX_DEVICELCTLKEYMASK | NX_DEVICELALTKEYMASK;
        assert_eq!(
            get_modifiers(CGEventFlags(flags)),
            Modifiers::LCTRL | Modifiers::LALT
        );
    }

    #[test]
    fn mixed_sides_across_groups() {
        let flags = NX_DEVICELCMDKEYMASK | NX_DEVICERALTKEYMASK | NX_DEVICERSHIFTKEYMASK;
        assert_eq!(
            get_modifiers(CGEventFlags(flags)),
            Modifiers::LCMD | Modifiers::RALT | Modifiers::RSHIFT
        );
    }

    #[test]
    fn device_independent_flags_ignored() {
        let generic_alt: u64 = 0x0008_0000;
        assert_eq!(get_modifiers(CGEventFlags(generic_alt)), Modifiers::empty());
    }

    #[test]
    fn shared_mouse_alt_scroll_matches_generic_binding() {
        // Deskflow uses the group flag for scroll events from Windows Alt.
        let modifiers = get_scroll_modifiers(CGEventFlags(0x2008_0000));
        assert!(Modifiers::ALT.matches(modifiers));
        // The event does not identify a side. Do not select one.
        assert!(!Modifiers::LALT.matches(modifiers));
        assert!(!Modifiers::RALT.matches(modifiers));
    }

    #[test]
    fn shared_mouse_extra_modifier_does_not_trigger_alt_scroll() {
        let modifiers = get_scroll_modifiers(CGEventFlags(0x200c_0000));
        assert!(!Modifiers::ALT.matches(modifiers));
        assert!((Modifiers::ALT | Modifiers::CTRL).matches(modifiers));
    }

    #[test]
    fn physical_scroll_keeps_the_reported_modifier_side() {
        let modifiers = get_scroll_modifiers(CGEventFlags(0x0008_0000 | NX_DEVICERALTKEYMASK));
        assert!(Modifiers::ALT.matches(modifiers));
        assert!(Modifiers::RALT.matches(modifiers));
        assert!(!Modifiers::LALT.matches(modifiers));
    }

    #[test]
    fn ordinary_scroll_does_not_trigger_alt_scroll() {
        assert!(!Modifiers::ALT.matches(get_scroll_modifiers(CGEventFlags(0x2000_0000))));
    }

    #[test]
    fn shared_mouse_scroll_supports_other_modifier_groups() {
        for (group, flag) in [
            (Modifiers::SHIFT, 0x0002_0000),
            (Modifiers::CTRL, 0x0004_0000),
            (Modifiers::CMD, 0x0010_0000),
        ] {
            assert!(group.matches(get_scroll_modifiers(CGEventFlags(flag))));
        }
    }

    #[test]
    fn secondary_fn_flag_is_not_ignored() {
        // Ensure we don't accidentally filter out the fn mask as "device independent"
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICEFNKEYMASK)),
            Modifiers::FN
        );
    }

    #[test]
    fn only_intercepts_the_explicitly_configured_gesture() {
        assert!(!gesture_should_intercept(None, 3));
        assert!(!gesture_should_intercept(Some(2), 2));
        assert!(gesture_should_intercept(Some(3), 3));
        assert!(!gesture_should_intercept(Some(4), 3));
        assert!(gesture_should_intercept(Some(4), 4));
    }

    #[test]
    fn empty_intermediate_gesture_event_keeps_the_previous_sample() {
        assert!(!should_reset_swipe_samples(0, 3));
        assert_horizontal(
            movement(&three_fingers(0.0), &three_fingers(0.0006)),
            -0.0006,
        );
    }

    #[test]
    fn nonempty_finger_count_change_starts_new_samples() {
        assert!(should_reset_swipe_samples(2, 3));
        assert_eq!(
            movement(&three_fingers(0.0), &three_fingers(0.0006)[..2]),
            None
        );
    }
}
