//! Helpers for mapping window entities to accessibility types

use alloc::{collections::VecDeque, sync::Arc};
use bevy_input_focus::InputFocus;
use core::cell::RefCell;
use std::sync::Mutex;
use winit::{event::WindowEvent, event_loop::ActiveEventLoop};

use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId, Role,
    TreeId, TreeInfo, TreeUpdate,
};
use bevy_a11y::{
    AccessibilityNode, AccessibilityRequested, AccessibilitySystems,
    ActionRequest as ActionRequestWrapper, ManageAccessibilityUpdates,
};
use bevy_app::{App, Plugin, PostUpdate};
use bevy_derive::{Deref, DerefMut};
use bevy_ecs::{entity::EntityHashMap, prelude::*, system::NonSendMarker};
use bevy_window::{PrimaryWindow, Window, WindowClosed};

thread_local! {
    /// Temporary storage of access kit adapter data to replace usage of `!Send` resources. This will be replaced with proper
    /// storage of `!Send` data after issue #17667 is complete.
    pub static ACCESS_KIT_ADAPTERS: RefCell<AccessKitAdapters> = const { RefCell::new(AccessKitAdapters::new()) };
}

/// Maps window entities to their `AccessKit` [`Adapter`]s.
#[derive(Default, Deref, DerefMut)]
pub struct AccessKitAdapters(pub EntityHashMap<Adapter>);

impl AccessKitAdapters {
    /// Creates a new empty `AccessKitAdapters`.
    pub const fn new() -> Self {
        Self(EntityHashMap::new())
    }
}

/// Maps window entities to their respective [`ActionRequest`]s.
#[derive(Resource, Default, Deref, DerefMut)]
pub struct WinitActionRequestHandlers(pub EntityHashMap<Arc<Mutex<WinitActionRequestHandler>>>);

/// Forwards `AccessKit` [`ActionRequest`]s from winit to an event channel.
#[derive(Clone, Default, Deref, DerefMut)]
pub struct WinitActionRequestHandler(pub VecDeque<ActionRequest>);

impl WinitActionRequestHandler {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self(VecDeque::new())))
    }
}

struct AccessKitState {
    name: String,
    entity: Entity,
    requested: AccessibilityRequested,
}

impl AccessKitState {
    fn new(
        name: impl Into<String>,
        entity: Entity,
        requested: AccessibilityRequested,
    ) -> Arc<Mutex<Self>> {
        let name = name.into();

        Arc::new(Mutex::new(Self {
            name,
            entity,
            requested,
        }))
    }

    fn build_root(&mut self) -> Node {
        let mut node = Node::new(Role::Window);
        node.set_label(self.name.clone());
        node
    }

    fn build_initial_tree(&mut self) -> TreeUpdate {
        let root = self.build_root();
        let accesskit_window_id = NodeId(self.entity.to_bits());
        let mut tree = TreeInfo::new(accesskit_window_id);
        tree.toolkit_name = Some("Bevy".into());
        tree.toolkit_version = Some(env!("CARGO_PKG_VERSION").into());
        self.requested.set(true);

        TreeUpdate {
            nodes: vec![(accesskit_window_id, root)],
            tree: Some(tree),
            tree_id: TreeId::ROOT,
            focus: accesskit_window_id,
        }
    }
}

struct WinitActivationHandler(Arc<Mutex<AccessKitState>>);

impl ActivationHandler for WinitActivationHandler {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        Some(self.0.lock().unwrap().build_initial_tree())
    }
}

impl WinitActivationHandler {
    pub fn new(state: Arc<Mutex<AccessKitState>>) -> Self {
        Self(state)
    }
}

#[derive(Clone, Default)]
struct WinitActionHandler(Arc<Mutex<WinitActionRequestHandler>>);

impl ActionHandler for WinitActionHandler {
    fn do_action(&mut self, request: ActionRequest) {
        let mut requests = self.0.lock().unwrap();
        requests.push_back(request);
    }
}

impl WinitActionHandler {
    pub fn new(handler: Arc<Mutex<WinitActionRequestHandler>>) -> Self {
        Self(handler)
    }
}

struct WinitDeactivationHandler;

impl DeactivationHandler for WinitDeactivationHandler {
    fn deactivate_accessibility(&mut self) {}
}

// AccessKit's winit adapter still uses winit 0.30. Use the same platform
// adapters directly so these windows and events use only winit 0.31.
#[cfg(target_os = "macos")]
type PlatformAdapter = accesskit_macos::SubclassingAdapter;
#[cfg(target_os = "ios")]
type PlatformAdapter = accesskit_ios::SubclassingAdapter;
#[cfg(target_os = "windows")]
type PlatformAdapter = accesskit_windows::SubclassingAdapter;
#[cfg(all(
    feature = "accesskit_unix",
    any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )
))]
type PlatformAdapter = accesskit_unix::Adapter;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    all(
        feature = "accesskit_unix",
        any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )
    )
)))]
type PlatformAdapter = ();

/// Connects a Bevy accessibility tree to the window's platform adapter.
pub struct Adapter(Option<PlatformAdapter>);

impl Adapter {
    #[cfg_attr(
        any(target_os = "macos", target_os = "ios"),
        expect(unsafe_code, reason = "AccessKit requires a native view pointer")
    )]
    fn new(
        _window: &dyn winit::window::Window,
        _activation: WinitActivationHandler,
        _action: WinitActionHandler,
        _deactivation: WinitDeactivationHandler,
    ) -> Self {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        #[cfg(target_os = "macos")]
        {
            if let Ok(handle) = _window.window_handle()
                && let RawWindowHandle::AppKit(handle) = handle.as_raw()
            {
                // SAFETY: Winit supplied a live NSView and this runs on the
                // main thread, before making the window visible.
                return Self(Some(unsafe {
                    PlatformAdapter::new(handle.ns_view.as_ptr(), _activation, _action)
                }));
            }
            Self(None)
        }
        #[cfg(target_os = "ios")]
        {
            if let Ok(handle) = _window.window_handle()
                && let RawWindowHandle::UiKit(handle) = handle.as_raw()
            {
                // SAFETY: Winit supplied a live UIView and this runs on the
                // main thread, before making the window visible.
                return Self(Some(unsafe {
                    PlatformAdapter::new(
                        handle.ui_view.as_ptr(),
                        _activation,
                        _action,
                        _deactivation,
                    )
                }));
            }
            Self(None)
        }
        #[cfg(target_os = "windows")]
        {
            if let Ok(handle) = _window.window_handle()
                && let RawWindowHandle::Win32(handle) = handle.as_raw()
            {
                let hwnd = accesskit_windows::HWND(handle.hwnd.get() as *mut _);
                return Self(Some(PlatformAdapter::new(hwnd, _activation, _action)));
            }
            Self(None)
        }
        #[cfg(all(
            feature = "accesskit_unix",
            any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            )
        ))]
        {
            Self(Some(PlatformAdapter::new(
                _activation,
                _action,
                _deactivation,
            )))
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "windows",
            all(
                feature = "accesskit_unix",
                any(
                    target_os = "linux",
                    target_os = "dragonfly",
                    target_os = "freebsd",
                    target_os = "netbsd",
                    target_os = "openbsd"
                )
            )
        )))]
        Self(None)
    }

    fn update_if_active(&mut self, _updater: impl FnOnce() -> TreeUpdate) {
        let _platform_adapter = &mut self.0;

        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
        if let Some(adapter) = _platform_adapter
            && let Some(events) = adapter.update_if_active(_updater)
        {
            events.raise();
        }

        #[cfg(all(
            feature = "accesskit_unix",
            any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            )
        ))]
        if let Some(adapter) = _platform_adapter {
            adapter.update_if_active(_updater);
        }
    }

    /// Lets the platform adapter observe winit events before other systems.
    pub fn process_event(&mut self, _window: &dyn winit::window::Window, _event: &WindowEvent) {
        #[cfg(target_os = "macos")]
        if let (Some(adapter), WindowEvent::Focused(focused)) = (&mut self.0, _event)
            && let Some(events) = adapter.update_view_focus_state(*focused)
        {
            events.raise();
        }

        #[cfg(all(
            feature = "accesskit_unix",
            any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            )
        ))]
        if let Some(adapter) = &mut self.0 {
            use accesskit::Rect;

            match _event {
                WindowEvent::Moved(_) | WindowEvent::SurfaceResized(_) => {
                    let (outer_position, surface_size) = match _event {
                        WindowEvent::Moved(position) => (*position, _window.surface_size()),
                        WindowEvent::SurfaceResized(size) => {
                            (_window.outer_position().unwrap_or_default(), *size)
                        }
                        _ => unreachable!(),
                    };
                    let outer_position = outer_position.cast::<f64>();
                    let surface_offset = _window.surface_position().cast::<f64>();
                    let surface_position = (
                        outer_position.x + surface_offset.x,
                        outer_position.y + surface_offset.y,
                    );
                    adapter.set_root_window_bounds(
                        Rect::from_origin_size(
                            <(f64, f64)>::from(outer_position),
                            <(f64, f64)>::from(_window.outer_size().cast::<f64>()),
                        ),
                        Rect::from_origin_size(
                            surface_position,
                            <(f64, f64)>::from(surface_size.cast::<f64>()),
                        ),
                    );
                }
                WindowEvent::Focused(focused) => adapter.update_window_focus_state(*focused),
                _ => {}
            }
        }
    }
}

/// Prepares accessibility for a winit window.
pub(crate) fn prepare_accessibility_for_window(
    _event_loop: &dyn ActiveEventLoop,
    winit_window: &dyn winit::window::Window,
    entity: Entity,
    name: String,
    accessibility_requested: AccessibilityRequested,
    adapters: &mut AccessKitAdapters,
    handlers: &mut WinitActionRequestHandlers,
) {
    let state = AccessKitState::new(name, entity, accessibility_requested);
    let activation_handler = WinitActivationHandler::new(Arc::clone(&state));

    let action_request_handler = WinitActionRequestHandler::new();
    let action_handler = WinitActionHandler::new(Arc::clone(&action_request_handler));
    let deactivation_handler = WinitDeactivationHandler;

    let adapter = Adapter::new(
        winit_window,
        activation_handler,
        action_handler,
        deactivation_handler,
    );

    adapters.insert(entity, adapter);
    handlers.insert(entity, action_request_handler);
}

fn window_closed(
    mut handlers: ResMut<WinitActionRequestHandlers>,
    mut window_closed_reader: MessageReader<WindowClosed>,
    _non_send_marker: NonSendMarker,
) {
    ACCESS_KIT_ADAPTERS.with_borrow_mut(|adapters| {
        for WindowClosed { window, .. } in window_closed_reader.read() {
            adapters.remove(window);
            handlers.remove(window);
        }
    });
}

fn poll_receivers(
    handlers: Res<WinitActionRequestHandlers>,
    mut actions: MessageWriter<ActionRequestWrapper>,
) {
    for (_id, handler) in handlers.iter() {
        let mut handler = handler.lock().unwrap();
        while let Some(event) = handler.pop_front() {
            actions.write(ActionRequestWrapper(event));
        }
    }
}

fn should_update_accessibility_nodes(
    accessibility_requested: Res<AccessibilityRequested>,
    manage_accessibility_updates: Res<ManageAccessibilityUpdates>,
) -> bool {
    accessibility_requested.get() && manage_accessibility_updates.get()
}

fn update_accessibility_nodes(
    focus: Option<Res<InputFocus>>,
    primary_window: Query<(Entity, &Window), With<PrimaryWindow>>,
    nodes: Query<(
        Entity,
        &AccessibilityNode,
        Option<&Children>,
        Option<&ChildOf>,
    )>,
    node_entities: Query<Entity, With<AccessibilityNode>>,
    _non_send_marker: NonSendMarker,
) {
    ACCESS_KIT_ADAPTERS.with_borrow_mut(|adapters| {
        let Ok((primary_window_id, primary_window)) = primary_window.single() else {
            return;
        };
        let Some(adapter) = adapters.get_mut(&primary_window_id) else {
            return;
        };
        let Some(focus) = focus else {
            return;
        };
        if focus.is_changed() || !nodes.is_empty() {
            // Don't panic if the focused entity does not currently exist
            // It's probably waiting to be spawned
            if let Some(focused_entity) = focus.get()
                && !node_entities.contains(focused_entity)
            {
                return;
            }

            adapter.update_if_active(|| {
                update_adapter(
                    nodes,
                    node_entities,
                    primary_window,
                    primary_window_id,
                    focus,
                )
            });
        }
    });
}

fn update_adapter(
    nodes: Query<(
        Entity,
        &AccessibilityNode,
        Option<&Children>,
        Option<&ChildOf>,
    )>,
    node_entities: Query<Entity, With<AccessibilityNode>>,
    primary_window: &Window,
    primary_window_id: Entity,
    focus: Res<InputFocus>,
) -> TreeUpdate {
    let mut to_update = vec![];
    let mut window_children = vec![];
    for (entity, node, children, child_of) in &nodes {
        let mut node = (**node).clone();
        queue_node_for_update(entity, child_of, &node_entities, &mut window_children);
        add_children_nodes(children, &node_entities, &mut node);
        let node_id = NodeId(entity.to_bits());
        to_update.push((node_id, node));
    }
    let mut window_node = Node::new(Role::Window);
    if primary_window.focused {
        let title = primary_window.title.clone();
        window_node.set_label(title.into_boxed_str());
    }
    window_node.set_children(window_children);
    let node_id = NodeId(primary_window_id.to_bits());
    let window_update = (node_id, window_node);
    to_update.insert(0, window_update);
    TreeUpdate {
        nodes: to_update,
        tree: None,
        tree_id: TreeId::ROOT,
        focus: NodeId(focus.get().unwrap_or(primary_window_id).to_bits()),
    }
}

#[inline]
fn queue_node_for_update(
    node_entity: Entity,
    child_of: Option<&ChildOf>,
    node_entities: &Query<Entity, With<AccessibilityNode>>,
    window_children: &mut Vec<NodeId>,
) {
    let should_push = if let Some(child_of) = child_of {
        !node_entities.contains(child_of.parent())
    } else {
        true
    };
    if should_push {
        window_children.push(NodeId(node_entity.to_bits()));
    }
}

#[inline]
fn add_children_nodes(
    children: Option<&Children>,
    node_entities: &Query<Entity, With<AccessibilityNode>>,
    node: &mut Node,
) {
    let Some(children) = children else {
        return;
    };
    for child in children {
        if node_entities.contains(*child) {
            node.push_child(NodeId(child.to_bits()));
        }
    }
}

/// Implements winit-specific `AccessKit` functionality.
pub struct AccessKitPlugin;

impl Plugin for AccessKitPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WinitActionRequestHandlers>()
            .add_message::<ActionRequestWrapper>()
            .add_systems(
                PostUpdate,
                (
                    poll_receivers,
                    update_accessibility_nodes
                        .run_if(should_update_accessibility_nodes)
                        // This is unlikely to result in real conflicts,
                        // as FocusChangeEvents only mutates internal state of InputFocus,
                        // and update_accessibility_nodes only reads from it.
                        // However, in case this changes in the future, this is a safer choice,
                        // as accessibility updates could conceivably want to read focus change events.
                        .after(bevy_input_focus::InputFocusSystems::FocusChangeEvents),
                    window_closed
                        .before(poll_receivers)
                        .before(update_accessibility_nodes),
                )
                    .in_set(AccessibilitySystems::Update),
            );
    }
}
