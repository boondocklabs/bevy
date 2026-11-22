use approx::relative_eq;
use bevy_app::{App, AppExit, PluginsState};
use bevy_ecs::{
    change_detection::{DetectChanges, Res},
    entity::Entity,
    message::MessageCursor,
    prelude::*,
    system::SystemState,
    world::FromWorld,
};
use bevy_input::touch::TouchPhase;
use bevy_input::{
    gestures::*,
    keyboard::{Key, KeyCode, KeyboardInput},
    mouse::{MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel},
};
use bevy_log::{trace, warn};
use bevy_math::{ivec2, DVec2, Vec2};
use bevy_platform::collections::HashMap;
use bevy_platform::time::Instant;
#[cfg(not(target_arch = "wasm32"))]
use bevy_tasks::tick_global_task_pools_on_main_thread;
use core::cell::Cell;
use std::{rc::Rc, sync::mpsc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event,
    event::{
        ButtonSource, DeviceEvent, DeviceId, FingerId, Modifiers, PointerKind, PointerSource,
        StartCause, WindowEvent,
    },
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::ModifiersKeyState,
    window::WindowId,
};

use bevy_window::{
    AppLifecycle, CursorEntered, CursorLeft, Ime, RawCursorMoved, RequestRedraw, Window,
    WindowBackendScaleFactorChanged, WindowCloseRequested, WindowDestroyed,
    WindowEvent as BevyWindowEvent, WindowFocused, WindowMoved, WindowOccluded, WindowResized,
    WindowScaleFactorChanged, WindowThemeChanged,
};
#[cfg(target_os = "android")]
use bevy_window::{PrimaryWindow, RawHandleWrapper, RawHandleWrapperHolder};

use crate::{
    accessibility::ACCESS_KIT_ADAPTERS,
    converters::{self, convert_touch_phase},
    create_windows,
    system::{create_monitors, CachedWindow, WinitWindowPressedKeys},
    AppSendEvent, CreateMonitorParams, CreateWindowParams, RawWinitWindowEvent, UpdateMode,
    WinitSettings, WinitUserEvent, WINIT_WINDOWS,
};

/// Persistent state that is used to run the [`App`] according to the current
/// [`UpdateMode`].
pub(crate) struct WinitAppRunnerState {
    /// The running app.
    app: App,
    /// Exit value once the loop is finished.
    app_exit: Rc<Cell<Option<AppExit>>>,
    user_events: mpsc::Receiver<WinitUserEvent>,
    surfaces_available: bool,
    #[cfg(target_os = "android")]
    activity_resumed: bool,
    active_touches: HashMap<(Entity, FingerId), winit::dpi::LogicalPosition<f64>>,
    file_dnd: crate::drag_and_drop::FileDragAndDropState,
    /// Current update mode of the app.
    update_mode: UpdateMode,
    /// Is `true` if a new [`WindowEvent`] event has been received since the last update.
    window_event_received: bool,
    /// Is `true` if a new [`DeviceEvent`] event has been received since the last update.
    device_event_received: bool,
    /// Is `true` if a new user event has been received since the last update.
    user_event_received: bool,
    /// Is `true` if the app has requested a redraw since the last update.
    redraw_requested: bool,
    /// Is `true` if the app has already updated since the last redraw.
    ran_update_since_last_redraw: bool,
    /// Is `true` if enough time has elapsed since `last_update` to run another update.
    wait_elapsed: bool,
    /// Number of "forced" updates to trigger on application start
    startup_forced_updates: u32,

    /// Current app lifecycle state.
    lifecycle: AppLifecycle,
    /// The previous app lifecycle state.
    previous_lifecycle: AppLifecycle,
    /// Bevy window events to send
    bevy_window_events: Vec<bevy_window::WindowEvent>,
    /// Raw Winit window events to send
    raw_winit_events: Vec<RawWinitWindowEvent>,
    /// this frame's updated winit modifier state per window, buffered for reconciliation
    /// against `WinitWindowPressedKeys` once all of this batch's key events
    /// have been seen (see `reconcile_pending_modifiers`).
    pending_modifiers: Vec<(Entity, Modifiers)>,

    windows_system_state: SystemState<
        Query<
            'static,
            'static,
            (
                &'static mut Window,
                &'static mut CachedWindow,
                &'static mut WinitWindowPressedKeys,
            ),
        >,
    >,
    /// time at which next tick is scheduled to run when `update_mode` is [`UpdateMode::Reactive`]
    scheduled_tick_start: Option<Instant>,
}

impl WinitAppRunnerState {
    fn new(mut app: App, user_events: mpsc::Receiver<WinitUserEvent>) -> Self {
        let windows_system_state: SystemState<
            Query<(&mut Window, &mut CachedWindow, &mut WinitWindowPressedKeys)>,
        > = SystemState::new(app.world_mut());

        Self {
            app,
            lifecycle: AppLifecycle::Idle,
            previous_lifecycle: AppLifecycle::Idle,
            app_exit: Rc::new(Cell::new(None)),
            user_events,
            surfaces_available: false,
            #[cfg(target_os = "android")]
            activity_resumed: false,
            active_touches: HashMap::new(),
            file_dnd: Default::default(),
            update_mode: UpdateMode::Continuous,
            window_event_received: false,
            device_event_received: false,
            user_event_received: false,
            redraw_requested: false,
            ran_update_since_last_redraw: false,
            wait_elapsed: false,
            // 3 seems to be enough, 5 is a safe margin
            startup_forced_updates: 5,
            bevy_window_events: Vec::new(),
            raw_winit_events: Vec::new(),
            pending_modifiers: Vec::new(),
            windows_system_state,
            scheduled_tick_start: None,
        }
    }

    fn reset_on_update(&mut self) {
        self.window_event_received = false;
        self.device_event_received = false;
        self.user_event_received = false;
    }

    fn world(&self) -> &World {
        self.app.world()
    }

    pub(crate) fn world_mut(&mut self) -> &mut World {
        self.app.world_mut()
    }
}

impl ApplicationHandler for WinitAppRunnerState {
    fn new_events(&mut self, event_loop: &dyn ActiveEventLoop, cause: StartCause) {
        if event_loop.exiting() {
            return;
        }

        #[cfg(feature = "trace")]
        let _span = tracing::info_span!("winit event_handler").entered();

        if self.app.plugins_state() != PluginsState::Cleaned {
            if self.app.plugins_state() != PluginsState::Ready {
                #[cfg(not(target_arch = "wasm32"))]
                tick_global_task_pools_on_main_thread();
            } else {
                self.app.finish();
                self.app.cleanup();
            }
            self.redraw_requested = true;
        }

        self.wait_elapsed = match cause {
            StartCause::WaitCancelled {
                requested_resume, ..
            } => {
                // If the resume time is not after now, it means that at least the wait timeout
                // has elapsed. Alternatively, if the resume time is unset, the wait never elapses.
                requested_resume
                    .map(|resume| resume <= Instant::now())
                    .unwrap_or_default()
            }
            _ => true,
        };
    }

    fn resumed(&mut self, _event_loop: &dyn ActiveEventLoop) {
        #[cfg(target_os = "android")]
        {
            self.activity_resumed = true;
            if !self.surfaces_available {
                return;
            }
        }

        self.lifecycle = AppLifecycle::WillResume;
    }

    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.surfaces_available = true;
        // Desktop event loops do not emit resumed.
        #[cfg(not(target_os = "android"))]
        if self.lifecycle == AppLifecycle::Idle {
            self.lifecycle = AppLifecycle::WillResume;
        }
        self.create_pending_windows(event_loop);

        #[cfg(target_os = "android")]
        {
            self.restore_android_window();
            if self.activity_resumed && self.lifecycle != AppLifecycle::Running {
                self.lifecycle = AppLifecycle::WillResume;
            }
        }
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.user_event_received = true;
        while let Ok(event) = self.user_events.try_recv() {
            match event {
                WinitUserEvent::WakeUp => self.redraw_requested = true,
                WinitUserEvent::WindowAdded => self.create_pending_windows(event_loop),
            }
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &dyn ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        self.window_event_received = true;

        #[cfg_attr(
            not(target_os = "windows"),
            expect(unused_mut, reason = "only needs to be mut on windows for now")
        )]
        let mut manual_run_redraw_requested = false;

        WINIT_WINDOWS.with_borrow(|winit_windows| {
            ACCESS_KIT_ADAPTERS.with_borrow_mut(|access_kit_adapters| {
                let mut windows = self
                    .windows_system_state
                    .get_mut(self.app.world_mut())
                    .unwrap();

                let Some(window) = winit_windows.get_window_entity(window_id) else {
                    warn!("Skipped event {event:?} for unknown winit Window Id {window_id:?}");
                    return;
                };

                let Ok((mut win, _, mut pressed_keys)) = windows.get_mut(window) else {
                    warn!(
                        "Window {window:?} is missing `Window` component, skipping event {event:?}"
                    );
                    return;
                };

                // Store a copy of the event to send to a MessageWriter later.
                self.raw_winit_events.push(RawWinitWindowEvent {
                    window_id,
                    event: event.clone(),
                });

                // Allow AccessKit to respond to `WindowEvent`s before they reach
                // the engine.
                if let Some(adapter) = access_kit_adapters.get_mut(&window)
                    && let Some(winit_window) = winit_windows.get_window(window)
                {
                    adapter.process_event(winit_window.as_ref(), &event);
                }

                self.file_dnd
                    .handle(_event_loop, window, &event, &mut self.bevy_window_events);

                match event {
                    WindowEvent::SurfaceResized(size) => self
                        .bevy_window_events
                        .send(react_to_resize(window, &mut win, size)),
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        let (window_backend_scale_factor_changed, window_scale_factor_changed) =
                            react_to_scale_factor_change(window, &mut win, scale_factor);

                        self.bevy_window_events
                            .send(window_backend_scale_factor_changed);
                        if let Some(window_scale_factor_changed) = window_scale_factor_changed {
                            self.bevy_window_events.send(window_scale_factor_changed);
                        }
                    }
                    WindowEvent::CloseRequested => self
                        .bevy_window_events
                        .send(WindowCloseRequested { window }),
                    WindowEvent::KeyboardInput {
                        ref event,
                        // On some platforms, winit sends "synthetic" key press events when the window
                        // gains or loses focus. These are not implemented on every platform, so we ignore
                        // winit's synthetic key pressed and implement the same mechanism ourselves.
                        // (See the `WinitWindowPressedKeys` component)
                        is_synthetic: false,
                        ..
                    } => {
                        let keyboard_input = converters::convert_keyboard_input(event, window);
                        if event.state.is_pressed() {
                            pressed_keys.0.insert(
                                keyboard_input.key_code,
                                keyboard_input.logical_key.clone(),
                            );
                        } else {
                            pressed_keys.0.remove(&keyboard_input.key_code);
                        }
                        self.bevy_window_events.send(keyboard_input);
                    }
                    WindowEvent::ModifiersChanged(mods) => {
                        // Buffer the modified state, to be reconciled at end-of-batch
                        if let Some(entry) = self
                            .pending_modifiers
                            .iter_mut()
                            .find(|(entity, _)| *entity == window)
                        {
                            entry.1 = mods;
                        } else {
                            self.pending_modifiers.push((window, mods));
                        }
                    }
                    WindowEvent::PointerMoved {
                        position, source, ..
                    } => match source {
                        PointerSource::Touch { finger_id, force } => {
                            if let Some(previous) =
                                self.active_touches.get_mut(&(window, finger_id))
                            {
                                let location =
                                    position.to_logical(win.resolution.scale_factor() as f64);
                                *previous = location;
                                self.bevy_window_events
                                    .send(converters::convert_touch_input(
                                        finger_id,
                                        TouchPhase::Moved,
                                        force,
                                        location,
                                        window,
                                    ));
                            }
                        }
                        _ => self.bevy_window_events.send(RawCursorMoved {
                            window,
                            physical_position: DVec2::new(position.x, position.y),
                        }),
                    },
                    WindowEvent::PointerEntered { position, kind, .. } => {
                        if !matches!(kind, PointerKind::Touch(_)) {
                            self.bevy_window_events.send(CursorEntered { window });
                            // Winit does not send a separate move event on pointer entry.
                            self.bevy_window_events.send(RawCursorMoved {
                                window,
                                physical_position: DVec2::new(position.x, position.y),
                            });
                        }
                    }
                    WindowEvent::PointerLeft { position, kind, .. } => {
                        if let PointerKind::Touch(finger_id) = kind {
                            if let Some(previous) = self.active_touches.remove(&(window, finger_id))
                            {
                                // The touch may have been cancelled; Winit may not provide a final position.
                                let location = position
                                    .map(|p| p.to_logical(win.resolution.scale_factor() as f64))
                                    .unwrap_or(previous);
                                self.bevy_window_events
                                    .send(converters::convert_touch_input(
                                        finger_id,
                                        TouchPhase::Canceled,
                                        None,
                                        location,
                                        window,
                                    ));
                            }
                        } else {
                            self.bevy_window_events.send(CursorLeft { window });
                        }
                    }
                    WindowEvent::PointerButton {
                        state,
                        button,
                        position,
                        ..
                    } => {
                        if let ButtonSource::Touch { finger_id, force } = button {
                            let location =
                                position.to_logical(win.resolution.scale_factor() as f64);
                            let phase = if state.is_pressed() {
                                self.active_touches.insert((window, finger_id), location);
                                TouchPhase::Started
                            } else {
                                self.active_touches.remove(&(window, finger_id));
                                TouchPhase::Ended
                            };
                            self.bevy_window_events
                                .send(converters::convert_touch_input(
                                    finger_id, phase, force, location, window,
                                ));
                        } else if let Some(button) = button.mouse_button() {
                            self.bevy_window_events.send(MouseButtonInput {
                                button: converters::convert_mouse_button(button),
                                state: converters::convert_element_state(state),
                                window,
                            });
                        }
                    }
                    WindowEvent::PinchGesture { delta, .. } => {
                        self.bevy_window_events.send(PinchGesture(delta as f32));
                    }
                    WindowEvent::RotationGesture { delta, .. } => {
                        self.bevy_window_events.send(RotationGesture(delta));
                    }
                    WindowEvent::DoubleTapGesture { .. } => {
                        self.bevy_window_events.send(DoubleTapGesture);
                    }
                    WindowEvent::PanGesture { delta, .. } => {
                        self.bevy_window_events.send(PanGesture(Vec2 {
                            x: delta.x,
                            y: delta.y,
                        }));
                    }
                    WindowEvent::MouseWheel { delta, phase, .. } => {
                        let phase = convert_touch_phase(phase);
                        match delta {
                            event::MouseScrollDelta::LineDelta(x, y) => {
                                self.bevy_window_events.send(MouseWheel {
                                    unit: MouseScrollUnit::Line,
                                    x,
                                    y,
                                    window,
                                    phase,
                                });
                            }
                            event::MouseScrollDelta::PixelDelta(p) => {
                                self.bevy_window_events.send(MouseWheel {
                                    unit: MouseScrollUnit::Pixel,
                                    x: p.x as f32,
                                    y: p.y as f32,
                                    window,
                                    phase,
                                });
                            }
                            _ => {}
                        }
                    }
                    WindowEvent::Focused(focused) => {
                        win.focused = focused;
                        self.bevy_window_events
                            .send(WindowFocused { window, focused });
                    }
                    WindowEvent::Occluded(occluded) => {
                        self.bevy_window_events
                            .send(WindowOccluded { window, occluded });
                    }
                    WindowEvent::Moved(position) => {
                        let position = ivec2(position.x, position.y);
                        win.position.set(position);
                        self.bevy_window_events
                            .send(WindowMoved { window, position });
                    }
                    WindowEvent::Ime(event) => match event {
                        event::Ime::Preedit(value, cursor) => {
                            self.bevy_window_events.send(Ime::Preedit {
                                window,
                                value,
                                cursor,
                            });
                        }
                        event::Ime::Commit(value) => {
                            self.bevy_window_events.send(Ime::Commit { window, value });
                        }
                        event::Ime::Enabled => {
                            self.bevy_window_events.send(Ime::Enabled { window });
                        }
                        event::Ime::Disabled => {
                            self.bevy_window_events.send(Ime::Disabled { window });
                        }
                        // Bevy does not request the IME surrounding-text capability.
                        event::Ime::DeleteSurrounding { .. } => {}
                    },
                    WindowEvent::ThemeChanged(theme) => {
                        self.bevy_window_events.send(WindowThemeChanged {
                            window,
                            theme: converters::convert_winit_theme(theme),
                        });
                    }
                    WindowEvent::Destroyed => {
                        self.active_touches
                            .retain(|(entity, _), _| *entity != window);
                        self.bevy_window_events.send(WindowDestroyed { window });
                    }
                    WindowEvent::RedrawRequested => {
                        self.ran_update_since_last_redraw = false;

                        // https://github.com/bevyengine/bevy/issues/17488
                        #[cfg(target_os = "windows")]
                        {
                            // Have the startup behavior run in about_to_wait, which prevents issues with
                            // invisible window creation. https://github.com/bevyengine/bevy/issues/18027
                            if self.startup_forced_updates == 0 {
                                manual_run_redraw_requested = true;
                            }
                        }
                    }
                    _ => {}
                }

                let mut windows = self.world_mut().query::<(&mut Window, &mut CachedWindow)>();
                if let Ok((window_component, mut cache)) = windows.get_mut(self.world_mut(), window)
                    && window_component.is_changed()
                {
                    **cache = window_component.clone();
                }
            });
        });

        if manual_run_redraw_requested {
            self.redraw_requested(_event_loop);
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &dyn ActiveEventLoop,
        _device_id: Option<DeviceId>,
        event: DeviceEvent,
    ) {
        self.device_event_received = true;

        if let DeviceEvent::PointerMotion { delta: (x, y) } = event {
            let delta = Vec2::new(x as f32, y as f32);
            self.bevy_window_events.send(MouseMotion { delta });
        }
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        #[cfg(target_os = "android")]
        if !self.surfaces_available && self.lifecycle != AppLifecycle::WillSuspend {
            return;
        }

        let mut create_monitor = SystemState::<CreateMonitorParams>::from_world(self.world_mut());
        create_monitors(
            event_loop,
            create_monitor.get_mut(self.world_mut()).unwrap(),
        );
        create_monitor.apply(self.world_mut());

        // TODO: This is a workaround for https://github.com/bevyengine/bevy/issues/17488
        //       while preserving the iOS fix in https://github.com/bevyengine/bevy/pull/11245
        //       The monitor sync logic likely belongs in monitor event handlers and not here.
        #[cfg(not(target_os = "windows"))]
        self.redraw_requested(event_loop);

        // Have the startup behavior run in about_to_wait, which prevents issues with
        // invisible window creation. https://github.com/bevyengine/bevy/issues/18027
        #[cfg(target_os = "windows")]
        {
            fn headless_or_all_invisible() -> bool {
                WINIT_WINDOWS.with_borrow(|winit_windows| {
                    winit_windows
                        .windows
                        .iter()
                        .all(|(_, w)| !w.is_visible().unwrap_or(false))
                })
            }

            if !event_loop.exiting()
                && (self.startup_forced_updates > 0
                    || matches!(self.update_mode, UpdateMode::Reactive { .. })
                    || self.window_event_received
                    || headless_or_all_invisible())
            {
                self.redraw_requested(event_loop);
            }
        }
    }

    fn suspended(&mut self, _event_loop: &dyn ActiveEventLoop) {
        #[cfg(target_os = "android")]
        {
            self.activity_resumed = false;
        }
        // Mark the state as `WillSuspend`. This will let the schedule run one last time
        // before actually suspending to let the application react
        if self.lifecycle != AppLifecycle::Suspended {
            self.lifecycle = AppLifecycle::WillSuspend;
        }
    }

    fn destroy_surfaces(&mut self, _event_loop: &dyn ActiveEventLoop) {
        self.surfaces_available = false;

        #[cfg(target_os = "android")]
        {
            // Android invalidates the native surface at this callback. Remove
            // its handle before the last update so render extraction can drop it.
            let mut query = self.world_mut().query_filtered::<
                (Entity, Option<&RawHandleWrapperHolder>),
                (With<PrimaryWindow>, With<RawHandleWrapper>),
            >();
            if let Ok((entity, holder)) = query.single(self.world()) {
                if let Some(holder) = holder {
                    *holder.0.lock().unwrap() = None;
                }
                self.world_mut()
                    .entity_mut(entity)
                    .remove::<RawHandleWrapper>();

                // Update even when Android already sent the activity's stop event.
                if self.lifecycle == AppLifecycle::Suspended {
                    self.run_app_update();
                } else {
                    self.lifecycle = AppLifecycle::WillSuspend;
                    self.redraw_requested(_event_loop);
                }
            }
        }
    }
}

impl Drop for WinitAppRunnerState {
    fn drop(&mut self) {
        // Release windows before the Winit event loop and its platform state are destroyed.
        WINIT_WINDOWS.with(|windows| windows.borrow_mut().windows.clear());
        self.world_mut().clear_all();
    }
}

impl WinitAppRunnerState {
    fn create_pending_windows(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.surfaces_available {
            let mut create_window = SystemState::<CreateWindowParams>::from_world(self.world_mut());
            create_windows(event_loop, create_window.get_mut(self.world_mut()).unwrap());
            create_window.apply(self.world_mut());
        }
    }

    #[cfg(target_os = "android")]
    fn restore_android_window(&mut self) {
        let mut query = self.world_mut().query_filtered::<Entity, (
            With<PrimaryWindow>,
            With<CachedWindow>,
            Without<RawHandleWrapper>,
        )>();
        let Ok(entity) = query.single(self.world()) else {
            return;
        };

        // Winit reuses the Android window object; query it for the new NativeWindow handle.
        let raw_handle = WINIT_WINDOWS.with_borrow(|windows| {
            windows
                .get_window(entity)
                .and_then(|window| RawHandleWrapper::new(window).ok())
        });
        let Some(raw_handle) = raw_handle else {
            warn!("Android reported a new surface, but the primary window has no native handle");
            return;
        };
        if let Some(holder) = self.world().get::<RawHandleWrapperHolder>(entity) {
            *holder.0.lock().unwrap() = Some(raw_handle.clone());
        }
        self.world_mut().entity_mut(entity).insert(raw_handle);
    }

    fn redraw_requested(&mut self, event_loop: &dyn ActiveEventLoop) {
        let mut redraw_message_cursor = MessageCursor::<RequestRedraw>::default();
        let mut close_message_cursor = MessageCursor::<WindowCloseRequested>::default();

        let mut focused_windows_state: SystemState<(Res<WinitSettings>, Query<(Entity, &Window)>)> =
            SystemState::new(self.world_mut());

        let (config, windows) = focused_windows_state.get(self.world()).unwrap();
        let focused = windows.iter().any(|(_, window)| window.focused);

        let mut update_mode = config.update_mode(focused);
        let mut should_update = self.should_update(update_mode);

        if self.startup_forced_updates > 0 {
            self.startup_forced_updates -= 1;
            // Ensure that an update is triggered on the first iterations for app initialization
            should_update = true;
        }

        if self.lifecycle == AppLifecycle::WillSuspend {
            self.lifecycle = AppLifecycle::Suspended;
            // Trigger one last update to enter the suspended state
            should_update = true;
            self.ran_update_since_last_redraw = false;
        }

        if self.lifecycle == AppLifecycle::WillResume {
            self.lifecycle = AppLifecycle::Running;
            // Trigger the update to enter the running state
            should_update = true;
            // Trigger the next redraw to refresh the screen immediately
            self.redraw_requested = true;
        }

        // Notifies a lifecycle change
        if self.lifecycle != self.previous_lifecycle {
            self.previous_lifecycle = self.lifecycle;
            self.bevy_window_events.send(self.lifecycle);
        }

        // This is recorded before running app.update(), to run the next cycle after a correct timeout.
        // If the cycle takes more than the wait timeout, it will be re-executed immediately.
        let begin_frame_time = Instant::now();

        if should_update {
            let (_, windows) = focused_windows_state.get(self.world()).unwrap();
            // If no windows exist, this will evaluate to `true`.
            let all_invisible = windows.iter().all(|w| !w.1.visible);

            // Not redrawing, but the timeout elapsed.
            //
            // Additional condition for Windows OS.
            // If no windows are visible, redraw calls will never succeed, which results in no app update calls being performed.
            // This is a temporary solution, full solution is mentioned here: https://github.com/bevyengine/bevy/issues/1343#issuecomment-770091684
            if !self.ran_update_since_last_redraw || all_invisible {
                self.run_app_update();
                #[cfg(feature = "custom_cursor")]
                self.update_cursors(event_loop);
                #[cfg(not(feature = "custom_cursor"))]
                self.update_cursors();
                self.ran_update_since_last_redraw = true;
            } else {
                self.redraw_requested = true;
            }

            // Read RequestRedraw events that may have been sent during the update
            if let Some(app_redraw_events) = self.world().get_resource::<Messages<RequestRedraw>>()
                && redraw_message_cursor
                    .read(app_redraw_events)
                    .last()
                    .is_some()
            {
                self.redraw_requested = true;
            }

            // Running the app may have produced WindowCloseRequested events that should be processed
            if let Some(close_request_messages) = self
                .world()
                .get_resource::<Messages<WindowCloseRequested>>()
                && close_message_cursor
                    .read(close_request_messages)
                    .last()
                    .is_some()
            {
                self.redraw_requested = true;
            }

            // Running the app may have changed the WinitSettings resource, so we have to re-extract it.
            let (config, windows) = focused_windows_state.get(self.world()).unwrap();
            let focused = windows.iter().any(|(_, window)| window.focused);
            update_mode = config.update_mode(focused);
        }

        // The update mode could have been changed, so we need to redraw and force an update
        if update_mode != self.update_mode {
            // Trigger the next redraw since we're changing the update mode
            self.redraw_requested = true;
            // Consider the wait as elapsed since it could have been cancelled by a user event
            self.wait_elapsed = true;
            // reset the scheduled start time
            self.scheduled_tick_start = None;

            self.update_mode = update_mode;
        }

        match update_mode {
            UpdateMode::Continuous => {
                // per winit's docs on [Window::is_visible](https://docs.rs/winit/latest/winit/window/struct.Window.html#method.is_visible),
                // we cannot use the visibility to drive rendering on these platforms
                // so we cannot discern whether to beneficially use `Poll` or not?
                cfg_select! {
                    not(any(
                        target_arch = "wasm32",
                        target_os = "android",
                        target_os = "ios",
                        target_os = "tvos",
                        all(target_os = "linux", any(feature = "x11", feature = "wayland"))
                    )) => {
                        let visible = WINIT_WINDOWS.with_borrow(|winit_windows| {
                            winit_windows
                                .windows
                                .iter()
                                .any(|(_, w)| w.is_visible().unwrap_or(false))
                        });

                        event_loop.set_control_flow(if visible {
                            ControlFlow::Wait
                        } else {
                            ControlFlow::Poll
                        });
                    }
                    _ => {
                        event_loop.set_control_flow(ControlFlow::Wait);
                    }
                }

                // Trigger the next redraw to refresh the screen immediately if waiting
                if let ControlFlow::Wait = event_loop.control_flow() {
                    self.redraw_requested = true;
                }
            }
            UpdateMode::Reactive { wait, .. } => {
                // Set the next timeout, starting from the instant we were scheduled to begin
                if self.wait_elapsed {
                    self.redraw_requested = true;

                    let begin_instant = self.scheduled_tick_start.unwrap_or(begin_frame_time);
                    if let Some(next) = begin_instant.checked_add(wait) {
                        let now = Instant::now();
                        if next < now {
                            // request next redraw as soon as possible if we are already past the next scheduled frame start time
                            event_loop.set_control_flow(ControlFlow::Poll);
                            self.scheduled_tick_start = Some(now);
                        } else {
                            event_loop.set_control_flow(ControlFlow::WaitUntil(next));
                            self.scheduled_tick_start = Some(next);
                        }
                    } else {
                        // If the checked_add fails, we have no concrete instant to wait til.
                        // In such a case, set our control flow to wait indefinitely..
                        event_loop.set_control_flow(ControlFlow::Wait);
                        // .. and set no scheduled re-start instant.
                        self.scheduled_tick_start = None;
                    }
                }
            }
        }

        if self.redraw_requested && self.lifecycle != AppLifecycle::Suspended {
            WINIT_WINDOWS.with_borrow(|winit_windows| {
                for window in winit_windows.windows.values() {
                    window.request_redraw();
                }
            });
            self.redraw_requested = false;
        }

        if let Some(app_exit) = self.app.should_exit() {
            self.app_exit.set(Some(app_exit));

            event_loop.exit();
        }
    }

    fn should_update(&self, update_mode: UpdateMode) -> bool {
        let handle_event = match update_mode {
            UpdateMode::Continuous => {
                self.wait_elapsed
                    || self.user_event_received
                    || self.window_event_received
                    || self.device_event_received
            }
            UpdateMode::Reactive {
                react_to_device_events,
                react_to_user_events,
                react_to_window_events,
                ..
            } => {
                self.wait_elapsed
                    || (react_to_device_events && self.device_event_received)
                    || (react_to_user_events && self.user_event_received)
                    || (react_to_window_events && self.window_event_received)
            }
        };

        handle_event && self.lifecycle.is_active()
    }

    fn run_app_update(&mut self) {
        self.reset_on_update();

        self.forward_bevy_events();

        if self.app.plugins_state() == PluginsState::Cleaned {
            self.app.update();
        }
    }

    /// Apply the final per-window modifier state of this event batch to
    /// `WinitWindowPressedKeys`, synthesizing `KeyboardInput` events for any
    /// modifier key whose transition winit never delivered as a key event.
    /// That happens when the OS consumes a shortcut chord (the keyup lands
    /// elsewhere — e.g. the macOS Cmd+Shift+5 screen-capture overlay takes
    /// the keyups without any focus transition, especially on the web) or
    /// when focus returns with a modifier already held (the keydown predates
    /// focus). Winit re-derives modifier state from input events, so a stale
    /// modifier heals on the next input event even when no focus transition
    /// was ever reported.
    ///
    /// Running at end-of-batch means the batch's real key events have
    /// already been applied to `WinitWindowPressedKeys`, so ordinary
    /// modifier presses and releases synthesize nothing.
    fn reconcile_pending_modifiers(&mut self) {
        if self.pending_modifiers.is_empty() {
            return;
        }

        let pending = core::mem::take(&mut self.pending_modifiers);
        let mut synthesized = Vec::new();

        for (window, mods) in pending {
            let Some(mut pressed_keys) = self.world_mut().get_mut::<WinitWindowPressedKeys>(window)
            else {
                continue;
            };

            let state = mods.state();
            for (key_codes, logical_key, group_pressed) in [
                (
                    [
                        (KeyCode::SuperLeft, mods.lsuper_state()),
                        (KeyCode::SuperRight, mods.rsuper_state()),
                    ],
                    Key::Super,
                    state.meta_key(),
                ),
                (
                    [
                        (KeyCode::ControlLeft, mods.lcontrol_state()),
                        (KeyCode::ControlRight, mods.rcontrol_state()),
                    ],
                    Key::Control,
                    state.control_key(),
                ),
                (
                    [
                        (KeyCode::AltLeft, mods.lalt_state()),
                        (KeyCode::AltRight, mods.ralt_state()),
                    ],
                    Key::Alt,
                    state.alt_key(),
                ),
                (
                    [
                        (KeyCode::ShiftLeft, mods.lshift_state()),
                        (KeyCode::ShiftRight, mods.rshift_state()),
                    ],
                    Key::Shift,
                    state.shift_key(),
                ),
            ] {
                let any_group_keycode_pressed = key_codes
                    .iter()
                    .any(|(_, state)| state == &ModifiersKeyState::Pressed);

                for (key_code, state) in key_codes {
                    let recorded = pressed_keys.0.contains_key(&key_code);

                    // we can only be sure a key has been released if either all group keys
                    // are released, or the platform provably supports known modifier states
                    // (only macOS attests them; web, linux and windows currently always
                    // report `ModifiersKeyState::Unknown`)
                    let known_released = !group_pressed
                        || (state == ModifiersKeyState::Unknown && any_group_keycode_pressed);

                    if state == ModifiersKeyState::Pressed && !recorded {
                        // if this keycode is attested down but we never saw its keydown
                        // (e.g. it was already held when focus was gained): reapply it.
                        // if the platform cannot attest modifier state this is unreachable.
                        pressed_keys.0.insert(key_code, logical_key.clone());
                        synthesized.push(KeyboardInput {
                            key_code,
                            logical_key: logical_key.clone(),
                            state: bevy_input::ButtonState::Pressed,
                            repeat: false,
                            window,
                            text: None,
                        });
                    } else if recorded && known_released {
                        // this side is provably up but we never saw its keyup
                        // (e.g. the OS consumed it as part of a shortcut chord):
                        // release the recorded key.
                        let Some(logical_key) = pressed_keys.0.remove(&key_code) else {
                            continue;
                        };
                        synthesized.push(KeyboardInput {
                            key_code,
                            logical_key,
                            state: bevy_input::ButtonState::Released,
                            repeat: false,
                            window,
                            text: None,
                        });
                    }
                }
            }
        }

        for event in synthesized {
            self.bevy_window_events.send(event);
        }
    }

    #[expect(
        clippy::drain_collect,
        reason = "draining preserves the capacity of the event buffers, which are refilled every frame"
    )]
    fn forward_bevy_events(&mut self) {
        self.reconcile_pending_modifiers();

        let raw_winit_events = self.raw_winit_events.drain(..).collect::<Vec<_>>();
        let window_events = self.bevy_window_events.drain(..).collect::<Vec<_>>();
        let world = self.world_mut();

        if !raw_winit_events.is_empty() {
            world
                .resource_mut::<Messages<RawWinitWindowEvent>>()
                .write_batch(raw_winit_events);
        }

        if !window_events.is_empty() {
            world
                .resource_mut::<Messages<BevyWindowEvent>>()
                .write_batch(window_events);
        }
    }
}

/// The default [`App::runner`] for the [`WinitPlugin`](crate::WinitPlugin) plugin.
///
/// Overriding the app's [runner](bevy_app::App::runner) while using `WinitPlugin` will bypass the
/// `EventLoop`.
pub(crate) fn winit_runner(
    mut app: App,
    event_loop: EventLoop,
    user_events: mpsc::Receiver<WinitUserEvent>,
) -> AppExit {
    if app.plugins_state() == PluginsState::Ready {
        app.finish();
        app.cleanup();
    }

    let runner_state = WinitAppRunnerState::new(app, user_events);

    trace!("starting winit event loop");
    #[cfg(not(target_arch = "wasm32"))]
    let app_exit = Rc::clone(&runner_state.app_exit);
    if let Err(err) = event_loop.run_app(runner_state) {
        bevy_log::error!("winit event loop returned an error: {err}");
        return AppExit::error();
    }

    cfg_select! {
        // Winit registers the browser handler and returns immediately.
        target_arch = "wasm32" => AppExit::Success,
        _ => app_exit.take().unwrap_or_else(|| {
            bevy_log::error!("Failed to receive an app exit code! This is a bug");
            AppExit::error()
        })
    }
}

pub(crate) fn react_to_resize(
    window_entity: Entity,
    window: &mut Window,
    size: PhysicalSize<u32>,
) -> WindowResized {
    window
        .resolution
        .set_physical_resolution(size.width, size.height);

    WindowResized {
        window: window_entity,
        width: window.width(),
        height: window.height(),
    }
}

pub(crate) fn react_to_scale_factor_change(
    window_entity: Entity,
    window: &mut Window,
    scale_factor: f64,
) -> (
    WindowBackendScaleFactorChanged,
    Option<WindowScaleFactorChanged>,
) {
    let prior_factor = window.resolution.scale_factor();
    window.resolution.set_scale_factor(scale_factor as f32);

    let window_backend_scale_factor_changed = WindowBackendScaleFactorChanged {
        window: window_entity,
        scale_factor,
    };

    let scale_factor_override = window.resolution.scale_factor_override();

    let window_scale_factor_changed =
        if scale_factor_override.is_none() && !relative_eq!(scale_factor as f32, prior_factor) {
            let window_scale_factor_changed = WindowScaleFactorChanged {
                window: window_entity,
                scale_factor,
            };
            Some(window_scale_factor_changed)
        } else {
            None
        };

    (
        window_backend_scale_factor_changed,
        window_scale_factor_changed,
    )
}

#[cfg(test)]
mod tests {
    use bevy_app::Update;

    use super::*;

    #[test]
    fn test_react_to_scale_factor_change_with_changed_scale_factor() {
        let (mut app, window_entity) = setup_react_to_scale_factor_change_test_app(1.0, 2.0);
        app.update();

        let window = app.world().get::<Window>(window_entity);
        assert_eq!(window.unwrap().resolution.scale_factor(), 2.0);

        let window_backend_scale_factor_changed_messages = app
            .world()
            .resource::<Messages<WindowBackendScaleFactorChanged>>();
        assert_eq!(window_backend_scale_factor_changed_messages.len(), 1);

        let mut window_backend_scale_factor_changed_messages_iter =
            window_backend_scale_factor_changed_messages.iter_current_update_messages();
        assert_eq!(
            window_backend_scale_factor_changed_messages_iter.next(),
            Some(&WindowBackendScaleFactorChanged {
                window: window_entity,
                scale_factor: 2.0,
            })
        );
        assert_eq!(
            window_backend_scale_factor_changed_messages_iter.next(),
            None
        );

        let window_scale_factor_changed_messages =
            app.world().resource::<Messages<WindowScaleFactorChanged>>();
        assert_eq!(window_scale_factor_changed_messages.len(), 1);

        let mut window_scale_factor_changed_messages_iter =
            window_scale_factor_changed_messages.iter_current_update_messages();
        assert_eq!(
            window_scale_factor_changed_messages_iter.next(),
            Some(&WindowScaleFactorChanged {
                window: window_entity,
                scale_factor: 2.0,
            })
        );
        assert_eq!(window_scale_factor_changed_messages_iter.next(), None);

        let window_event_messages = app.world().resource::<Messages<BevyWindowEvent>>();
        assert_eq!(window_event_messages.len(), 2);

        let mut window_event_messages_iter = window_event_messages.iter_current_update_messages();
        assert_eq!(
            window_event_messages_iter.next(),
            Some(&BevyWindowEvent::WindowBackendScaleFactorChanged(
                WindowBackendScaleFactorChanged {
                    window: window_entity,
                    scale_factor: 2.0
                }
            ))
        );
        assert_eq!(
            window_event_messages_iter.next(),
            Some(&BevyWindowEvent::WindowScaleFactorChanged(
                WindowScaleFactorChanged {
                    window: window_entity,
                    scale_factor: 2.0
                }
            ))
        );
        assert_eq!(window_event_messages_iter.next(), None);
    }

    #[test]
    fn test_react_to_scale_factor_change_with_same_scale_factor() {
        let (mut app, window_entity) = setup_react_to_scale_factor_change_test_app(1.0, 1.0);
        app.update();

        let window = app.world().get::<Window>(window_entity);
        assert_eq!(window.unwrap().resolution.scale_factor(), 1.0);

        let window_backend_scale_factor_changed_messages = app
            .world()
            .resource::<Messages<WindowBackendScaleFactorChanged>>();
        assert_eq!(window_backend_scale_factor_changed_messages.len(), 1);

        let mut window_backend_scale_factor_changed_messages_iter =
            window_backend_scale_factor_changed_messages.iter_current_update_messages();
        assert_eq!(
            window_backend_scale_factor_changed_messages_iter.next(),
            Some(&WindowBackendScaleFactorChanged {
                window: window_entity,
                scale_factor: 1.0,
            })
        );
        assert_eq!(
            window_backend_scale_factor_changed_messages_iter.next(),
            None
        );

        let window_scale_factor_changed_messages =
            app.world().resource::<Messages<WindowScaleFactorChanged>>();
        assert!(window_scale_factor_changed_messages.is_empty());

        let window_event_messages = app.world().resource::<Messages<BevyWindowEvent>>();
        assert_eq!(window_event_messages.len(), 1);

        let mut window_event_messages_iter = window_event_messages.iter_current_update_messages();
        assert_eq!(
            window_event_messages_iter.next(),
            Some(&BevyWindowEvent::WindowBackendScaleFactorChanged(
                WindowBackendScaleFactorChanged {
                    window: window_entity,
                    scale_factor: 1.0
                }
            ))
        );
        assert_eq!(window_event_messages_iter.next(), None);
    }

    fn setup_react_to_scale_factor_change_test_app(
        initial_scale_factor: f32,
        changed_scale_factor: f64,
    ) -> (App, Entity) {
        let mut app = App::new();
        app.add_message::<WindowBackendScaleFactorChanged>();
        app.add_message::<WindowScaleFactorChanged>();
        app.add_message::<BevyWindowEvent>();
        app.add_systems(
            Update,
            move |mut window: Single<(Entity, &mut Window)>,
                  mut window_backend_scale_factor_changed_writer: MessageWriter<
                WindowBackendScaleFactorChanged,
            >,
                  mut window_scale_factor_changed_writer: MessageWriter<
                WindowScaleFactorChanged,
            >,
                  mut window_event: MessageWriter<BevyWindowEvent>| {
                let (window_backend_scale_factor_changed, window_scale_factor_changed) =
                    react_to_scale_factor_change(window.0, &mut window.1, changed_scale_factor);
                window_backend_scale_factor_changed_writer
                    .write(window_backend_scale_factor_changed.clone());
                window_event.write(BevyWindowEvent::WindowBackendScaleFactorChanged(
                    window_backend_scale_factor_changed,
                ));
                if let Some(window_scale_factor_changed) = window_scale_factor_changed {
                    window_scale_factor_changed_writer.write(window_scale_factor_changed.clone());
                    window_event.write(BevyWindowEvent::WindowScaleFactorChanged(
                        window_scale_factor_changed,
                    ));
                }
            },
        );

        let mut window = Window::default();
        window.resolution.set_scale_factor(initial_scale_factor);
        let window_entity = app.world_mut().spawn(window).id();

        (app, window_entity)
    }

    #[test]
    fn test_react_to_resize_with_changed_size() {
        let (mut app, window_entity) =
            setup_react_to_resize(PhysicalSize::new(1280, 720), PhysicalSize::new(1920, 1080));
        app.update();

        let window = app.world().get::<Window>(window_entity).unwrap();
        assert_eq!(window.resolution.physical_width(), 1920);
        assert_eq!(window.resolution.physical_height(), 1080);

        let window_resized_messages = app.world().resource::<Messages<WindowResized>>();
        assert_eq!(window_resized_messages.len(), 1);

        let mut window_resized_messages_iter =
            window_resized_messages.iter_current_update_messages();
        assert_eq!(
            window_resized_messages_iter.next(),
            Some(&WindowResized {
                window: window_entity,
                width: 1920.0,
                height: 1080.0
            })
        );
        assert_eq!(window_resized_messages_iter.next(), None);

        let window_event_messages = app.world().resource::<Messages<BevyWindowEvent>>();
        assert_eq!(window_event_messages.len(), 1);

        let mut window_event_messages_iter = window_event_messages.iter_current_update_messages();
        assert_eq!(
            window_event_messages_iter.next(),
            Some(&BevyWindowEvent::WindowResized(WindowResized {
                window: window_entity,
                width: 1920.0,
                height: 1080.0
            }))
        );
        assert_eq!(window_event_messages_iter.next(), None);
    }

    #[test]
    fn test_react_to_resize_with_same_size() {
        let (mut app, window_entity) =
            setup_react_to_resize(PhysicalSize::new(1280, 720), PhysicalSize::new(1280, 720));
        app.update();

        let window = app.world().get::<Window>(window_entity).unwrap();
        assert_eq!(window.resolution.physical_width(), 1280);
        assert_eq!(window.resolution.physical_height(), 720);

        let window_resized_messages = app.world().resource::<Messages<WindowResized>>();
        assert_eq!(window_resized_messages.len(), 1);

        let mut window_resized_messages_iter =
            window_resized_messages.iter_current_update_messages();
        assert_eq!(
            window_resized_messages_iter.next(),
            Some(&WindowResized {
                window: window_entity,
                width: 1280.0,
                height: 720.0
            })
        );
        assert_eq!(window_resized_messages_iter.next(), None);

        let window_event_messages = app.world().resource::<Messages<BevyWindowEvent>>();
        assert_eq!(window_event_messages.len(), 1);

        let mut window_event_messages_iter = window_event_messages.iter_current_update_messages();
        assert_eq!(
            window_event_messages_iter.next(),
            Some(&BevyWindowEvent::WindowResized(WindowResized {
                window: window_entity,
                width: 1280.0,
                height: 720.0
            }))
        );
        assert_eq!(window_event_messages_iter.next(), None);
    }

    fn setup_react_to_resize(
        initial_size: PhysicalSize<u32>,
        changed_size: PhysicalSize<u32>,
    ) -> (App, Entity) {
        let mut app = App::new();
        app.add_message::<WindowResized>();
        app.add_message::<BevyWindowEvent>();
        app.add_systems(
            Update,
            move |mut window: Single<(Entity, &mut Window)>,
                  mut window_resized_writer: MessageWriter<WindowResized>,
                  mut window_event: MessageWriter<BevyWindowEvent>| {
                let window_resized = react_to_resize(window.0, &mut window.1, changed_size);
                window_resized_writer.write(window_resized.clone());
                window_event.write(BevyWindowEvent::WindowResized(window_resized));
            },
        );

        let mut window = Window::default();
        window
            .resolution
            .set_physical_resolution(initial_size.width, initial_size.height);
        let window_entity = app.world_mut().spawn(window).id();

        (app, window_entity)
    }
}
