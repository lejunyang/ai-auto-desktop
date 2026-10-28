use super::{
    driver_error, remaining, AtspiBackend, AutomationError, BackendNode, BackendSnapshot, Bounds,
    Gesture, Map, NativeRef, Value, MAX_FIELD_CHARS, MAX_NODES,
};
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::application::ApplicationProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::text::TextProxy;
use atspi::{CoordType, Interface, ObjectRef, State};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::blocking::{connection, Connection, Proxy};

const REGISTRY_NAME: &str = "org.a11y.atspi.Registry";
const REGISTRY_PATH: &str = "/org/a11y/atspi/accessible/root";
const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const XTEST_OUTPUT_LIMIT: usize = 64 * 1024;
const CAPTURE_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
const CAPTURE_METADATA_LIMIT: usize = 64 * 1024;
type ApplicationEntry = (NativeRef, Map<String, Value>);

pub struct LinuxBackend {
    connection: Connection,
    recordings: Mutex<BTreeMap<String, LinuxRecorder>>,
}

impl LinuxBackend {
    pub fn new() -> Result<Self, AutomationError> {
        let session = session_info();
        let session_bus = connection::Builder::session()
            .and_then(|builder| builder.method_timeout(Duration::from_secs(3)).build())
            .map_err(|error| unavailable("session bus is unavailable", &session, error))?;
        let bus = Proxy::new(
            &session_bus,
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Bus",
        )
        .and_then(|proxy| proxy.call::<_, _, String>("GetAddress", &()))
        .map_err(|error| unavailable("AT-SPI bus address is unavailable", &session, error))?;
        let connection = connection::Builder::address(bus.as_str())
            .and_then(|builder| builder.method_timeout(Duration::from_secs(3)).build())
            .map_err(|error| {
                unavailable("AT-SPI accessibility bus is unavailable", &session, error)
            })?;
        let backend = Self {
            connection,
            recordings: Mutex::new(BTreeMap::new()),
        };
        backend.children(&Self::root_ref(), Instant::now() + Duration::from_secs(3))?;
        Ok(backend)
    }

    fn root_ref() -> NativeRef {
        NativeRef {
            bus_name: REGISTRY_NAME.into(),
            object_path: REGISTRY_PATH.into(),
        }
    }

    fn native(reference: ObjectRef) -> NativeRef {
        NativeRef {
            bus_name: reference.name.as_str().to_string(),
            object_path: reference.path.as_str().to_string(),
        }
    }

    fn accessible<'a>(
        &'a self,
        target: &'a NativeRef,
    ) -> Result<AccessibleProxy<'a>, AutomationError> {
        let builder = AccessibleProxy::builder(self.connection.inner())
            .destination(target.bus_name.as_str())
            .and_then(|builder| builder.path(target.object_path.as_str()))
            .map_err(|error| native_failure("Accessible proxy", target, error))?;
        zbus::block_on(
            builder
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )
        .map_err(|error| native_failure("Accessible proxy", target, error))
    }

    fn children(
        &self,
        target: &NativeRef,
        deadline: Instant,
    ) -> Result<Vec<NativeRef>, AutomationError> {
        remaining(deadline, false)?;
        let children = zbus::block_on(self.accessible(target)?.get_children())
            .map_err(|error| native_failure("Accessible.GetChildren", target, error))?;
        remaining(deadline, false)?;
        if children.len() > MAX_NODES {
            return Err(driver_error(
                "DRIVER.OUTPUT_TOO_LARGE",
                "Accessible.GetChildren exceeded the hard fan-out limit",
            ));
        }
        Ok(children.into_iter().map(Self::native).collect())
    }

    fn process_id(&self, bus_name: &str) -> Option<u32> {
        Proxy::new(&self.connection, DBUS_NAME, DBUS_PATH, DBUS_INTERFACE)
            .and_then(|proxy| proxy.call::<_, _, u32>("GetConnectionUnixProcessID", &(bus_name,)))
            .ok()
    }

    fn application_info(
        &self,
        target: &NativeRef,
        deadline: Instant,
    ) -> Result<Map<String, Value>, AutomationError> {
        remaining(deadline, false)?;
        let accessible = self.accessible(target)?;
        let name = zbus::block_on(accessible.name()).ok();
        let locale = zbus::block_on(accessible.locale()).ok();
        let application = ApplicationProxy::builder(self.connection.inner())
            .destination(target.bus_name.as_str())
            .and_then(|builder| builder.path(target.object_path.as_str()))
            .map(|builder| builder.cache_properties(zbus::proxy::CacheProperties::No));
        let (toolkit_name, toolkit_version, atspi_version, application_id) = match application {
            Ok(builder) => match zbus::block_on(builder.build()) {
                Ok(proxy) => (
                    zbus::block_on(proxy.toolkit_name()).ok(),
                    zbus::block_on(proxy.version()).ok(),
                    zbus::block_on(proxy.atspi_version()).ok(),
                    zbus::block_on(proxy.id()).ok(),
                ),
                Err(_) => (None, None, None, None),
            },
            Err(_) => (None, None, None, None),
        };
        remaining(deadline, false)?;
        Ok(Map::from_iter([
            ("bus_name".into(), json!(target.bus_name)),
            ("object_path".into(), json!(target.object_path)),
            ("name".into(), optional_text(name)),
            (
                "process_id".into(),
                self.process_id(&target.bus_name)
                    .map_or(Value::Null, |value| json!(value)),
            ),
            ("toolkit_name".into(), optional_text(toolkit_name)),
            ("toolkit_version".into(), optional_text(toolkit_version)),
            ("atspi_version".into(), optional_text(atspi_version)),
            (
                "application_id".into(),
                application_id.map_or(Value::Null, |value| json!(value)),
            ),
            ("locale".into(), optional_text(locale)),
        ]))
    }

    fn applications(&self, deadline: Instant) -> Result<Vec<ApplicationEntry>, AutomationError> {
        let mut result = Vec::new();
        for target in self.children(&Self::root_ref(), deadline)? {
            remaining(deadline, false)?;
            if let Ok(info) = self.application_info(&target, deadline) {
                result.push((target, info));
            }
        }
        Ok(result)
    }

    fn resolve_application(
        &self,
        selector: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<(NativeRef, Map<String, Value>), AutomationError> {
        let candidates = self
            .applications(deadline)?
            .into_iter()
            .filter(|(_, info)| {
                selector
                    .iter()
                    .all(|(key, value)| info.get(key) == Some(value))
            })
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(driver_error(
                "DRIVER.NOT_FOUND",
                "application selector matched no AT-SPI application",
            )),
            many => Err(driver_error(
                "DRIVER.AMBIGUOUS",
                "application selector matched multiple AT-SPI applications",
            )
            .with_detail("candidate_count", json!(many.len()))),
        }
    }

    fn action_proxy<'a>(
        &'a self,
        target: &'a NativeRef,
    ) -> Result<ActionProxy<'a>, AutomationError> {
        let builder = ActionProxy::builder(self.connection.inner())
            .destination(target.bus_name.as_str())
            .and_then(|builder| builder.path(target.object_path.as_str()))
            .map_err(|error| native_failure("Action proxy", target, error))?;
        zbus::block_on(
            builder
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )
        .map_err(|error| native_failure("Action proxy", target, error))
    }

    fn component_proxy<'a>(
        &'a self,
        target: &'a NativeRef,
    ) -> Result<ComponentProxy<'a>, AutomationError> {
        let builder = ComponentProxy::builder(self.connection.inner())
            .destination(target.bus_name.as_str())
            .and_then(|builder| builder.path(target.object_path.as_str()))
            .map_err(|error| native_failure("Component proxy", target, error))?;
        zbus::block_on(
            builder
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )
        .map_err(|error| native_failure("Component proxy", target, error))
    }

    fn actions(&self, target: &NativeRef) -> Vec<atspi::Action> {
        self.action_proxy(target)
            .and_then(|proxy| {
                zbus::block_on(proxy.get_actions())
                    .map_err(|error| native_failure("Action.GetActions", target, error))
            })
            .unwrap_or_default()
    }

    /// Focus the target, then run one XTest helper command against it.
    fn run_input(
        &self,
        target: &NativeRef,
        mut args: Vec<String>,
        minimum_events: u64,
        process_id: u32,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let path = self.helper(HelperKind::Input)?;
        let focus = self.focus(target, deadline)?;
        std::thread::sleep(Duration::from_millis(50).min(remaining(deadline, true)?));
        args.extend([
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ]);
        let output = run_helper(
            &path,
            &args,
            None,
            deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )
        .map_err(after_focus)?;
        let mut input = parse_input_helper(output, "input_dispatch", minimum_events, process_id)
            .map_err(after_focus)?;
        input["focus"] = focus;
        Ok(input)
    }

    fn helper(&self, kind: HelperKind) -> Result<PathBuf, AutomationError> {
        let (environment, file_name, repository_name) = match kind {
            HelperKind::Input => (
                "AAD_LINUX_XTEST_HELPER",
                "aad-linux-xtest-helper",
                "x11_xtest_helper",
            ),
            HelperKind::Capture => (
                "AAD_LINUX_CAPTURE_HELPER",
                "aad-linux-capture-helper",
                "x11_capture_helper",
            ),
        };
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os(environment) {
            candidates.push(PathBuf::from(path));
        }
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                candidates.push(parent.join(file_name));
            }
        }
        candidates.push(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins/linux_atspi/.build")
                .join(repository_name),
        );
        let path = candidates
            .into_iter()
            .find(|candidate| candidate.exists())
            .ok_or_else(|| {
                driver_error(
                    "DRIVER.UNAVAILABLE",
                    "required Linux X11 helper was not found",
                )
                .with_detail("environment", json!(environment))
                .with_detail("helper", json!(file_name))
            })?;
        validate_helper(&path)?;
        Ok(path)
    }

    fn read_node(
        &self,
        target: NativeRef,
        parent_index: Option<usize>,
        application: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<(BackendNode, usize), AutomationError> {
        remaining(deadline, false)?;
        let accessible = self.accessible(&target)?;
        let role = zbus::block_on(accessible.get_role_name())
            .map(|role| super::normalize_role(&role))
            .unwrap_or_else(|_| "unknown".into());
        let interfaces = zbus::block_on(accessible.get_interfaces()).unwrap_or_default();
        let states = zbus::block_on(accessible.get_state()).ok();
        let protected = matches!(role.as_str(), "password_text" | "password");
        let name = zbus::block_on(accessible.name()).ok();
        let description = zbus::block_on(accessible.description()).ok();
        let attributes = zbus::block_on(accessible.get_attributes()).unwrap_or_default();
        let accessible_id = zbus::block_on(accessible.accessible_id()).ok();
        let child_count = zbus::block_on(accessible.child_count()).unwrap_or(-1);
        let bounds = if interfaces.contains(Interface::Component) {
            self.component_proxy(&target)
                .and_then(|proxy| {
                    zbus::block_on(proxy.get_extents(CoordType::Screen))
                        .map_err(|error| native_failure("Component.GetExtents", &target, error))
                })
                .ok()
                .map(|(x, y, width, height)| Bounds {
                    x,
                    y,
                    width: width.max(0),
                    height: height.max(0),
                })
        } else {
            None
        };
        let value = if protected || !interfaces.contains(Interface::Text) {
            None
        } else {
            TextProxy::builder(self.connection.inner())
                .destination(target.bus_name.as_str())
                .and_then(|builder| builder.path(target.object_path.as_str()))
                .ok()
                .and_then(|builder| {
                    zbus::block_on(
                        builder
                            .cache_properties(zbus::proxy::CacheProperties::No)
                            .build(),
                    )
                    .ok()
                })
                .and_then(|proxy| {
                    zbus::block_on(proxy.character_count())
                        .ok()
                        .map(|count| (proxy, count))
                })
                .and_then(|(proxy, count)| {
                    zbus::block_on(proxy.get_text(0, count.clamp(0, MAX_FIELD_CHARS as i32))).ok()
                })
        };
        let action_metadata = self.actions(&target);
        let toolkit = application.get("toolkit_name").and_then(Value::as_str);
        let toolkit_version = application
            .get("toolkit_version")
            .and_then(Value::as_str)
            .unwrap_or("");
        let actionable = states.is_none_or(|states| {
            states.contains(State::Enabled) && states.contains(State::Sensitive)
        });
        let mut actions = Vec::new();
        if actionable
            && states.is_none_or(|states| states.contains(State::Focusable))
            && interfaces.contains(Interface::Component)
        {
            actions.push("focus".into());
        }
        let qt_invoke = toolkit == Some("Qt")
            && toolkit_version.starts_with("5.")
            && role == "push_button"
            && action_metadata
                .iter()
                .filter(|action| action.name == "Press")
                .count()
                == 1;
        if qt_invoke || (actionable && toolkit != Some("Qt") && action_metadata.len() == 1) {
            actions.push("invoke".into());
        }
        if actionable && interfaces.contains(Interface::EditableText) && !protected {
            actions.push("set_text".into());
        }
        let process_id = application
            .get("process_id")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0 && *value <= u32::MAX as u64);
        let fully_interactive = states.is_some_and(|states| {
            [
                State::Enabled,
                State::Visible,
                State::Showing,
                State::Sensitive,
                State::Focusable,
            ]
            .into_iter()
            .all(|state| states.contains(state))
        });
        if fully_interactive
            && states.is_some_and(|states| states.contains(State::Editable))
            && matches!(role.as_str(), "entry" | "text")
            && interfaces.contains(Interface::Component)
            && process_id.is_some()
            && !protected
        {
            actions.push("type_text".into());
        }
        if fully_interactive
            && interfaces.contains(Interface::Component)
            && process_id.is_some()
            && bounds.is_some_and(|bounds| bounds.width > 0 && bounds.height > 0)
            && !protected
        {
            actions.extend(
                ["pointer_click", "pointer_gesture", "drag", "key_chord"].map(String::from),
            );
        }
        let gtk3 = toolkit == Some("gtk") && toolkit_version.starts_with("3.");
        if gtk3
            && matches!(role.as_str(), "check_box" | "toggle_button")
            && states.is_some()
            && action_metadata
                .iter()
                .filter(|action| action.name.eq_ignore_ascii_case("click"))
                .count()
                == 1
        {
            actions.push("toggle".into());
        }
        if gtk3
            && states.is_some_and(|states| states.contains(State::Expandable))
            && action_metadata
                .iter()
                .filter(|action| action.name.eq_ignore_ascii_case("activate"))
                .count()
                == 1
        {
            actions.extend(["expand".into(), "collapse".into()]);
        }
        let state_value = |state| states.map(|set| set.contains(state));
        let states = BTreeMap::from_iter([
            ("enabled".into(), state_value(State::Enabled)),
            ("visible".into(), state_value(State::Visible)),
            ("showing".into(), state_value(State::Showing)),
            ("focusable".into(), state_value(State::Focusable)),
            ("focused".into(), state_value(State::Focused)),
            ("editable".into(), state_value(State::Editable)),
            ("sensitive".into(), state_value(State::Sensitive)),
            ("protected".into(), Some(protected)),
            ("checked".into(), state_value(State::Checked)),
            ("expandable".into(), state_value(State::Expandable)),
            ("expanded".into(), state_value(State::Expanded)),
            ("selectable".into(), state_value(State::Selectable)),
            ("selected".into(), state_value(State::Selected)),
        ]);
        let native_actions = action_metadata
            .iter()
            .enumerate()
            .map(|(index, action)| json!({"index": index, "name": action.name, "description": action.description, "key_binding": action.keybinding}))
            .collect::<Vec<_>>();
        let native_action_name = if actions.contains(&"toggle".to_string()) {
            Some("click")
        } else if actions.contains(&"expand".to_string())
            || actions.contains(&"collapse".to_string())
        {
            Some("activate")
        } else if qt_invoke {
            Some("Press")
        } else {
            None
        };
        remaining(deadline, false)?;
        Ok((
            BackendNode {
                native: target.clone(),
                parent_index,
                role,
                name,
                description,
                value,
                attributes: attributes.into_iter().collect(),
                states,
                bounds,
                actions,
                provenance: Map::from_iter([
                    ("bus_name".into(), json!(target.bus_name)),
                    ("object_path".into(), json!(target.object_path)),
                    ("accessible_id".into(), optional_text(accessible_id)),
                    (
                        "application_name".into(),
                        application.get("name").cloned().unwrap_or(Value::Null),
                    ),
                    (
                        "toolkit_name".into(),
                        application
                            .get("toolkit_name")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ),
                    (
                        "toolkit_version".into(),
                        application
                            .get("toolkit_version")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ),
                    (
                        "process_id".into(),
                        application
                            .get("process_id")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ),
                    ("value_redacted".into(), json!(protected)),
                    ("coordinate_space".into(), json!("screen")),
                    ("atspi_actions".into(), json!(native_actions)),
                    (
                        "native_action_name".into(),
                        native_action_name.map_or(Value::Null, |value| json!(value)),
                    ),
                ]),
            },
            child_count.max(0) as usize,
        ))
    }
}

impl AtspiBackend for LinuxBackend {
    fn start_recording(
        &self,
        application: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<String, AutomationError> {
        self.start_watch(application, deadline)
    }
    fn collect_recording(&self, id: &str) -> Result<Value, AutomationError> {
        self.collect_watch(id)
    }
    fn stop_recording(&self, id: &str) -> Result<(), AutomationError> {
        self.stop_watch(id)
    }
    fn name(&self) -> &str {
        "rust_atspi"
    }

    fn session_info(&self) -> Map<String, Value> {
        session_info()
    }

    fn list_applications(
        &self,
        deadline: Instant,
    ) -> Result<Vec<Map<String, Value>>, AutomationError> {
        Ok(self
            .applications(deadline)?
            .into_iter()
            .map(|(_, info)| info)
            .collect())
    }

    fn capture(
        &self,
        application: &Map<String, Value>,
        max_depth: u32,
        max_nodes: usize,
        deadline: Instant,
    ) -> Result<BackendSnapshot, AutomationError> {
        let (root, info) = self.resolve_application(application, deadline)?;
        let mut queue = VecDeque::from([(root, None, 0u32)]);
        let mut nodes = Vec::new();
        let mut truncated = false;
        while let Some((target, parent, depth)) = queue.pop_front() {
            remaining(deadline, false)?;
            if nodes.len() >= max_nodes {
                truncated = true;
                break;
            }
            let (node, child_count) = self.read_node(target.clone(), parent, &info, deadline)?;
            let current = nodes.len();
            nodes.push(node);
            if depth >= max_depth {
                truncated |= child_count > 0;
                continue;
            }
            match self.children(&target, deadline) {
                Ok(children) => {
                    truncated |= children.len() != child_count;
                    for child in children {
                        if nodes.len() + queue.len() >= max_nodes {
                            truncated = true;
                            break;
                        }
                        queue.push_back((child, Some(current), depth + 1));
                    }
                }
                Err(error)
                    if error.code == "DRIVER.TIMEOUT"
                        || error.code == "DRIVER.OUTPUT_TOO_LARGE" =>
                {
                    return Err(error)
                }
                Err(_) => truncated = true,
            }
        }
        Ok(BackendSnapshot {
            application: info,
            nodes,
            truncated,
        })
    }

    fn focus(&self, target: &NativeRef, deadline: Instant) -> Result<Value, AutomationError> {
        remaining(deadline, false)?;
        let accepted = zbus::block_on(self.component_proxy(target)?.grab_focus())
            .map_err(|error| native_failure("Component.GrabFocus", target, error))?;
        if !accepted {
            return Err(driver_error(
                "DRIVER.ACTION_FAILED",
                "Component.GrabFocus did not accept the request",
            ));
        }
        Ok(json!({"native_interface": "Component.grab_focus", "accepted": true}))
    }

    fn invoke(&self, target: &NativeRef, deadline: Instant) -> Result<Value, AutomationError> {
        let actions = self.actions(target);
        let index = if actions.len() == 1 {
            0
        } else {
            actions
                .iter()
                .position(|action| action.name == "Press")
                .ok_or_else(|| {
                    driver_error(
                        "DRIVER.ACTION_UNSUPPORTED",
                        "invoke has no unique qualified native action",
                    )
                })?
        };
        remaining(deadline, false)?;
        let accepted = zbus::block_on(self.action_proxy(target)?.do_action(index as i32))
            .map_err(|error| native_failure("Action.DoAction", target, error))?;
        if !accepted {
            return Err(driver_error(
                "DRIVER.ACTION_FAILED",
                "Action.DoAction did not accept the request",
            ));
        }
        Ok(
            json!({"native_interface": "Action.do_action", "accepted": true, "native_action_name": actions[index].name}),
        )
    }

    fn set_text(
        &self,
        target: &NativeRef,
        text: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        remaining(deadline, false)?;
        let builder = EditableTextProxy::builder(self.connection.inner())
            .destination(target.bus_name.as_str())
            .and_then(|builder| builder.path(target.object_path.as_str()))
            .map_err(|error| native_failure("EditableText proxy", target, error))?;
        let proxy = zbus::block_on(
            builder
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )
        .map_err(|error| native_failure("EditableText proxy", target, error))?;
        let accepted = zbus::block_on(proxy.set_text_contents(text))
            .map_err(|error| native_failure("EditableText.SetTextContents", target, error))?;
        if !accepted {
            return Err(driver_error(
                "DRIVER.ACTION_FAILED",
                "EditableText.SetTextContents did not accept the request",
            ));
        }
        Ok(json!({"native_interface": "EditableText.set_text_contents", "accepted": true}))
    }

    fn named_action(
        &self,
        target: &NativeRef,
        name: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let actions = self.actions(target);
        let matches = actions
            .iter()
            .enumerate()
            .filter(|(_, action)| action.name.eq_ignore_ascii_case(name))
            .collect::<Vec<_>>();
        let [(index, action)] = matches.as_slice() else {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                format!("target has no unique native {name} action"),
            ));
        };
        remaining(deadline, false)?;
        let accepted = zbus::block_on(self.action_proxy(target)?.do_action(*index as i32))
            .map_err(|error| native_failure("Action.DoAction", target, error))?;
        if !accepted {
            return Err(driver_error(
                "DRIVER.ACTION_FAILED",
                "Action.DoAction did not accept the request",
            ));
        }
        Ok(
            json!({"native_interface": "Action.do_action", "native_action_name": name, "native_action": {"index": index, "name": action.name, "description": action.description, "key_binding": action.keybinding}, "accepted": true, "dispatched": true, "no_op": false}),
        )
    }

    fn accessible_at_point(
        &self,
        root: &NativeRef,
        point: (i32, i32),
        deadline: Instant,
    ) -> Result<Option<NativeRef>, AutomationError> {
        remaining(deadline, false)?;
        let reference = zbus::block_on(self.component_proxy(root)?.get_accessible_at_point(
            point.0,
            point.1,
            CoordType::Screen,
        ))
        .map_err(|error| native_failure("Component.GetAccessibleAtPoint", root, error))?;
        if reference.path.as_str() == "/org/a11y/atspi/null" {
            Ok(None)
        } else {
            Ok(Some(Self::native(reference)))
        }
    }

    fn type_text(
        &self,
        target: &NativeRef,
        text: &str,
        process_id: u32,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let path = self.helper(HelperKind::Input)?;
        let focus = self.focus(target, deadline)?;
        std::thread::sleep(Duration::from_millis(50).min(remaining(deadline, true)?));
        let args = [
            "type-text".to_string(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        let output = run_helper(
            &path,
            &args,
            Some(text.as_bytes()),
            deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )
        .map_err(after_focus)?;
        let input =
            parse_input_helper(output, "keyboard_dispatch", 2, process_id).map_err(after_focus)?;
        Ok(
            json!({"native_interface": "Component.grab_focus -> XTEST", "focus": focus, "input": input, "synthetic_input": true}),
        )
    }

    fn pointer_click(
        &self,
        target: &NativeRef,
        point: (i32, i32),
        process_id: u32,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let path = self.helper(HelperKind::Input)?;
        let focus = self.focus(target, deadline)?;
        std::thread::sleep(Duration::from_millis(50).min(remaining(deadline, true)?));
        let args = [
            "pointer-click".to_string(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--x".into(),
            point.0.to_string(),
            "--y".into(),
            point.1.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        let output = run_helper(
            &path,
            &args,
            None,
            deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )
        .map_err(after_focus)?;
        let mut result =
            parse_input_helper(output, "pointer_dispatch", 3, process_id).map_err(after_focus)?;
        if let Some(object) = result.as_object_mut() {
            object.insert("click_point".into(), json!({"x": point.0, "y": point.1}));
        }
        Ok(
            json!({"native_interface": "Component.grab_focus -> XTEST", "focus": focus, "input": result, "synthetic_input": true, "button": "left", "position": "center", "click_point": {"x": point.0, "y": point.1}}),
        )
    }

    fn pointer_gesture(
        &self,
        target: &NativeRef,
        point: (i32, i32),
        gesture: Gesture,
        process_id: u32,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let mut args = vec![
            "pointer-gesture".to_string(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--x".into(),
            point.0.to_string(),
            "--y".into(),
            point.1.to_string(),
        ];
        let minimum = match gesture {
            Gesture::Click { button, count } => {
                args.extend([
                    "--mode".into(),
                    "click".into(),
                    "--button".into(),
                    button.to_string(),
                    "--count".into(),
                    count.to_string(),
                ]);
                1 + 2 * u64::from(count)
            }
            Gesture::Wheel { button, notches } => {
                args.extend([
                    "--mode".into(),
                    "wheel".into(),
                    "--button".into(),
                    button.to_string(),
                    "--notches".into(),
                    notches.to_string(),
                ]);
                1 + 2 * u64::from(notches)
            }
            Gesture::Hover { ms } => {
                args.extend([
                    "--mode".into(),
                    "hover".into(),
                    "--hover-ms".into(),
                    ms.to_string(),
                ]);
                1
            }
        };
        let input = self.run_input(target, args, minimum, process_id, deadline)?;
        Ok(
            json!({"native_interface": "Component.grab_focus -> XTEST", "input": input, "synthetic_input": true, "point": {"x": point.0, "y": point.1}}),
        )
    }

    fn drag(
        &self,
        target: &NativeRef,
        from: (i32, i32),
        to: (i32, i32),
        button: u8,
        process_id: u32,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let args = vec![
            "drag".to_string(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--x".into(),
            from.0.to_string(),
            "--y".into(),
            from.1.to_string(),
            "--to-x".into(),
            to.0.to_string(),
            "--to-y".into(),
            to.1.to_string(),
            "--button".into(),
            button.to_string(),
        ];
        let input = self.run_input(target, args, 3, process_id, deadline)?;
        Ok(
            json!({"native_interface": "Component.grab_focus -> XTEST", "input": input, "synthetic_input": true, "from": {"x": from.0, "y": from.1}, "to": {"x": to.0, "y": to.1}}),
        )
    }

    fn key_chord(
        &self,
        target: &NativeRef,
        keys: &[String],
        process_id: u32,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let args = vec![
            "key-chord".to_string(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--keys".into(),
            keys.join(","),
        ];
        let minimum = 2 * keys.len() as u64;
        let input = self.run_input(target, args, minimum, process_id, deadline)?;
        Ok(
            json!({"native_interface": "Component.grab_focus -> XTEST", "input": input, "synthetic_input": true, "keys": keys}),
        )
    }

    fn capture_target(
        &self,
        _target: &NativeRef,
        bounds: Bounds,
        process_id: u32,
        deadline: Instant,
    ) -> Result<(Vec<u8>, Map<String, Value>), AutomationError> {
        let path = self.helper(HelperKind::Capture)?;
        let args = [
            "capture-target".to_string(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--x".into(),
            bounds.x.to_string(),
            "--y".into(),
            bounds.y.to_string(),
            "--width".into(),
            bounds.width.to_string(),
            "--height".into(),
            bounds.height.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        let output = run_helper(
            &path,
            &args,
            None,
            deadline,
            CAPTURE_OUTPUT_LIMIT,
            CAPTURE_METADATA_LIMIT,
        )?;
        let metadata = parse_one_json_object(&output.stderr, "capture helper metadata")?;
        if !output.status.success() {
            return Err(map_capture_failure(output.status, &metadata));
        }
        validate_capture_metadata(&metadata, process_id, bounds, output.stdout.len())?;
        let provenance = Map::from_iter([
            ("capture_method".into(), metadata["capture_method"].clone()),
            ("format".into(), metadata["format"].clone()),
            ("mime_type".into(), metadata["mime_type"].clone()),
            ("target_process_id".into(), metadata["target_pid"].clone()),
            ("target_window".into(), metadata["target_window"].clone()),
            (
                "target_top_level_window".into(),
                metadata["target_top_level_window"].clone(),
            ),
            ("root_window".into(), metadata["root_window"].clone()),
            ("bounds".into(), bounds.to_json()),
            (
                "root_size".into(),
                json!({"width": metadata["root_width"], "height": metadata["root_height"]}),
            ),
            (
                "cursor_included".into(),
                metadata["cursor_included"].clone(),
            ),
            (
                "occlusion_checked".into(),
                metadata["occlusion_checked"].clone(),
            ),
            (
                "same_euid_verified".into(),
                metadata["same_euid_verified"].clone(),
            ),
            ("scene_stable".into(), metadata["scene_stable"].clone()),
        ]);
        Ok((output.stdout, provenance))
    }
}

#[derive(Clone, Copy)]
enum HelperKind {
    Input,
    Capture,
}

struct HelperOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn validate_helper(path: &Path) -> Result<(), AutomationError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        driver_error("DRIVER.UNAVAILABLE", "Linux X11 helper cannot be inspected")
            .with_detail("cause", json!(error.to_string()))
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.permissions().mode() & 0o100 == 0
    {
        return Err(driver_error(
            "DRIVER.UNAVAILABLE",
            "Linux X11 helper identity or permissions are not trusted",
        ));
    }
    Ok(())
}

fn helper_environment(command: &mut Command) {
    command.env_clear();
    for name in [
        "DISPLAY",
        "XAUTHORITY",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "DESKTOP_SESSION",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

fn run_helper(
    path: &Path,
    args: &[String],
    input: Option<&[u8]>,
    deadline: Instant,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<HelperOutput, AutomationError> {
    remaining(deadline, false)?;
    let mut command = Command::new(path);
    command
        .args(args)
        .current_dir(path.parent().unwrap_or_else(|| Path::new("/")))
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    helper_environment(&mut command);
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|error| {
        driver_error(
            "DRIVER.UNAVAILABLE",
            "Linux X11 helper could not be started",
        )
        .with_detail("cause", json!(error.to_string()))
    })?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("stdin is piped");
        if let Err(error) = stdin.write_all(input) {
            terminate(&mut child);
            return Err(
                driver_error("DRIVER.ACTION_FAILED", "helper input could not be written")
                    .with_detail("cause", json!(error.to_string())),
            );
        }
    }
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    let stdout_thread = std::thread::spawn(move || {
        let _ = stdout_sender.send(("stdout", read_bounded(&mut stdout, stdout_limit)));
    });
    let stderr_thread = std::thread::spawn(move || {
        let _ = sender.send(("stderr", read_bounded(&mut stderr, stderr_limit)));
    });
    let mut stdout_result = None;
    let mut stderr_result = None;
    let mut status = None;
    loop {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(("stdout", Ok(bytes))) => stdout_result = Some(bytes),
            Ok(("stderr", Ok(bytes))) => stderr_result = Some(bytes),
            Ok((name, Err(error))) => {
                terminate(&mut child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(driver_error(
                    "DRIVER.ACTION_FAILED",
                    "Linux X11 helper output exceeded its limit",
                )
                .with_detail("stream", json!(name))
                .with_detail("cause", json!(error.to_string())));
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
            }
        }
        if status.is_none() {
            status = child.try_wait().map_err(|error| {
                driver_error(
                    "DRIVER.ACTION_FAILED",
                    "Linux X11 helper state could not be read",
                )
                .with_detail("cause", json!(error.to_string()))
            })?;
        }
        if status.is_some() && stdout_result.is_some() && stderr_result.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(super::timeout_error(false));
        }
    }
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    Ok(HelperOutput {
        status: status.expect("helper exited"),
        stdout: stdout_result.unwrap_or_default(),
        stderr: stderr_result.unwrap_or_default(),
    })
}

fn read_bounded(reader: &mut impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "output limit exceeded",
        ));
    }
    Ok(bytes)
}

fn terminate(child: &mut Child) {
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    let grace = Instant::now() + Duration::from_millis(200);
    while Instant::now() < grace {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn parse_input_helper(
    output: HelperOutput,
    phase: &str,
    minimum_events: u64,
    process_id: u32,
) -> Result<Value, AutomationError> {
    let mut dispatched = false;
    let mut result = None;
    for line in output.stdout.split(|byte| *byte == b'\n') {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if value["event"] == "dispatch_started" {
            dispatched = true;
        } else if value.get("ok").is_some() {
            dispatched |= value["dispatch_started"] == true;
            result = value.as_object().cloned();
        }
    }
    if output.status.success() {
        let result = result.ok_or_else(|| {
            driver_error("DRIVER.UNKNOWN_EFFECT", "helper success result is missing")
                .with_effect("unknown")
        })?;
        if !dispatched
            || result.get("ok") != Some(&Value::Bool(true))
            || result
                .get("events")
                .and_then(Value::as_u64)
                .is_none_or(|events| events < minimum_events)
        {
            return Err(driver_error(
                "DRIVER.UNKNOWN_EFFECT",
                "helper did not prove successful input dispatch",
            )
            .with_effect("unknown"));
        }
        return Ok(
            json!({"native_interface": "XTEST", "synthetic_input": true, "submitted": true, "events": result["events"], "codepoints": result.get("codepoints").cloned().unwrap_or(Value::Null), "expected_process_id": process_id}),
        );
    }
    let code = output.status.code();
    if dispatched || code == Some(70) {
        return Err(driver_error(
            "DRIVER.UNKNOWN_EFFECT",
            "XTest helper failed after dispatch",
        )
        .with_effect("unknown")
        .with_detail("phase", json!(phase)));
    }
    let (error_code, message, retryable) = match code {
        Some(69) => ("DRIVER.UNAVAILABLE", "X11/XTest is unavailable", false),
        Some(74) => (
            "DRIVER.ACTION_UNSUPPORTED",
            "current X11 keymap cannot represent the text",
            false,
        ),
        Some(75) => (
            "DRIVER.TIMEOUT",
            "XTest helper timed out before dispatch",
            true,
        ),
        _ => (
            "DRIVER.ACTION_FAILED",
            "XTest helper failed before dispatch",
            false,
        ),
    };
    Err(driver_error(error_code, message)
        .with_retryable(retryable)
        .with_detail("helper_exit_code", json!(code)))
}

fn after_focus(error: AutomationError) -> AutomationError {
    if error.code == "DRIVER.UNKNOWN_EFFECT" {
        error
    } else {
        driver_error(
            "DRIVER.UNKNOWN_EFFECT",
            "target focus changed before synthetic input completed",
        )
        .with_effect("unknown")
        .with_cause(error)
    }
}

fn parse_one_json_object(bytes: &[u8], name: &str) -> Result<Map<String, Value>, AutomationError> {
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(driver_error(
            "DRIVER.CAPTURE_FAILED",
            format!("{name} must contain exactly one record"),
        ));
    }
    serde_json::from_slice::<Value>(lines[0])
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| driver_error("DRIVER.CAPTURE_FAILED", format!("{name} is invalid")))
}

fn map_capture_failure(status: ExitStatus, metadata: &Map<String, Value>) -> AutomationError {
    let code = status.code();
    let (error_code, message, retryable) = match code {
        Some(69) => ("DRIVER.UNAVAILABLE", "X11 capture is unavailable", false),
        Some(75) => ("DRIVER.TIMEOUT", "X11 capture helper timed out", true),
        _ => (
            "DRIVER.CAPTURE_FAILED",
            "X11 capture helper failed closed",
            false,
        ),
    };
    driver_error(error_code, message)
        .with_retryable(retryable)
        .with_detail("helper_exit_code", json!(code))
        .with_detail(
            "helper_code",
            metadata.get("code").cloned().unwrap_or(Value::Null),
        )
}

fn validate_capture_metadata(
    metadata: &Map<String, Value>,
    process_id: u32,
    bounds: Bounds,
    png_bytes: usize,
) -> Result<(), AutomationError> {
    let integer = |name: &str| metadata.get(name).and_then(Value::as_u64);
    let signed = |name: &str| metadata.get(name).and_then(Value::as_i64);
    if metadata.get("ok") != Some(&Value::Bool(true))
        || integer("schema_version") != Some(1)
        || metadata.get("capture_method") != Some(&json!("x11_root_xgetimage"))
        || metadata.get("format") != Some(&json!("png"))
        || metadata.get("mime_type") != Some(&json!("image/png"))
        || integer("expected_pid") != Some(u64::from(process_id))
        || integer("target_pid") != Some(u64::from(process_id))
        || signed("x") != Some(i64::from(bounds.x))
        || signed("y") != Some(i64::from(bounds.y))
        || signed("width") != Some(i64::from(bounds.width))
        || signed("height") != Some(i64::from(bounds.height))
        || integer("png_bytes") != Some(png_bytes as u64)
        || metadata.get("cursor_included") != Some(&Value::Bool(false))
        || metadata.get("occlusion_checked") != Some(&Value::Bool(true))
        || metadata.get("same_euid_verified") != Some(&Value::Bool(true))
        || metadata.get("scene_stable") != Some(&Value::Bool(true))
        || [
            "target_window",
            "target_top_level_window",
            "root_window",
            "root_width",
            "root_height",
        ]
        .iter()
        .any(|name| integer(name).is_none_or(|value| value == 0))
    {
        return Err(driver_error(
            "DRIVER.CAPTURE_FAILED",
            "X11 capture helper evidence does not match the request",
        ));
    }
    Ok(())
}

fn monotonic_deadline_ns(deadline: Instant) -> Result<u64, AutomationError> {
    remaining(deadline, false)?;
    let mut clock = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } != 0 {
        return Err(driver_error(
            "DRIVER.UNAVAILABLE",
            "monotonic clock is unavailable",
        ));
    }
    let now = u64::try_from(clock.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(clock.tv_nsec).unwrap_or(0));
    let delta = deadline
        .saturating_duration_since(Instant::now())
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64;
    Ok(now.saturating_add(delta))
}

fn optional_text(value: Option<String>) -> Value {
    value
        .map(|value| Value::String(value.chars().take(MAX_FIELD_CHARS).collect()))
        .unwrap_or(Value::Null)
}

fn session_info() -> Map<String, Value> {
    let optional = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .map_or(Value::Null, Value::String)
    };
    Map::from_iter([
        ("session_type".into(), optional("XDG_SESSION_TYPE")),
        (
            "desktop".into(),
            std::env::var("XDG_CURRENT_DESKTOP")
                .ok()
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    std::env::var("DESKTOP_SESSION")
                        .ok()
                        .filter(|value| !value.is_empty())
                })
                .map_or(Value::Null, Value::String),
        ),
        ("display".into(), optional("DISPLAY")),
        ("wayland_display".into(), optional("WAYLAND_DISPLAY")),
        (
            "session_bus_advertised".into(),
            json!(std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()),
        ),
        (
            "atspi_bus_advertised".into(),
            json!(std::env::var_os("AT_SPI_BUS_ADDRESS").is_some()),
        ),
    ])
}

fn unavailable(
    message: &str,
    session: &Map<String, Value>,
    error: impl std::fmt::Display,
) -> AutomationError {
    driver_error("DRIVER.UNAVAILABLE", message)
        .with_detail("reason", json!("session_or_bus_unavailable"))
        .with_detail("session", Value::Object(session.clone()))
        .with_detail("cause", json!(error.to_string()))
}

fn native_failure(
    operation: &str,
    target: &NativeRef,
    error: impl std::fmt::Display,
) -> AutomationError {
    driver_error(
        "DRIVER.ACTION_FAILED",
        format!("AT-SPI native operation {operation} failed"),
    )
    .with_detail("operation", json!(operation))
    .with_detail("bus_name", json!(target.bus_name))
    .with_detail("object_path", json!(target.object_path))
    .with_detail("cause", json!(error.to_string()))
}

// ---------------------------------------------------------------------------
// Watching a person work
//
// Two sources feed one buffer of raw events in the shared shape that
// `aad_control::recording` turns into steps: AT-SPI signals from the watched
// application (text edits, checked state, focus), and the XTest helper's
// `record-input`, which reports clicks, wheel turns and key presses landing on
// that application's windows. Elements are described through `read_node`, so
// a recorded locator matches what a snapshot shows.
// ---------------------------------------------------------------------------

const MAX_WATCHED: usize = 2_000;
const WATCH_LIFETIME: Duration = Duration::from_secs(3_600);

#[derive(Default)]
struct Watched {
    events: VecDeque<Value>,
    dropped: u64,
    /// The element that last took focus, for key presses.
    focused: Option<Value>,
}

impl Watched {
    fn push(&mut self, event: Value) {
        if self.events.len() >= MAX_WATCHED {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(event);
    }
}

pub(crate) struct LinuxRecorder {
    stop: Arc<AtomicBool>,
    buffer: Arc<Mutex<Watched>>,
    input: Child,
}

/// Milliseconds on the same clock as the helper's `steady_clock`.
fn monotonic_ms() -> u64 {
    let mut clock = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) };
    u64::try_from(clock.tv_sec).unwrap_or(0) * 1_000
        + u64::try_from(clock.tv_nsec).unwrap_or(0) / 1_000_000
}

/// A key press worth a step: a combination with ctrl, alt or super, or a key
/// no text change would show. Plain typing is recorded as the text it made.
fn chord(value: &Value) -> Option<Vec<String>> {
    let raw = value["key"].as_str()?;
    let key = match raw {
        "Return" | "KP_Enter" => "enter".to_string(),
        "Tab" | "ISO_Left_Tab" => "tab".into(),
        "Escape" => "escape".into(),
        "BackSpace" => "backspace".into(),
        "Delete" => "delete".into(),
        "Prior" => "pageup".into(),
        "Next" => "pagedown".into(),
        "Home" | "End" | "Left" | "Right" | "Up" | "Down" | "Insert" => raw.to_lowercase(),
        "space" => "space".into(),
        other if other.len() == 1 => other.to_lowercase(),
        other
            if other.starts_with('F')
                && other[1..]
                    .parse::<u8>()
                    .is_ok_and(|n| (1..=24).contains(&n)) =>
        {
            other.to_lowercase()
        }
        _ => return None,
    };
    let held = |name: &str| value[name] == true;
    let named =
        matches!(key.as_str(), "enter" | "tab" | "escape") || key.starts_with('f') && key.len() > 1;
    if !(held("ctrl") || held("alt") || held("super") || named) {
        return None;
    }
    let mut keys = Vec::new();
    for (flag, name) in [
        ("ctrl", "ctrl"),
        ("shift", "shift"),
        ("alt", "alt"),
        ("super", "super"),
    ] {
        if held(flag) {
            keys.push(name.to_string());
        }
    }
    keys.push(key);
    Some(keys)
}

impl LinuxBackend {
    /// An element as the watched events carry it.
    fn describe(&self, target: &NativeRef, info: &Map<String, Value>) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (node, _) = self.read_node(target.clone(), None, info, deadline).ok()?;
        let protected = node.states.get("protected").copied().flatten() == Some(true);
        Some(json!({
            "role": node.role,
            "name": node.name.unwrap_or_default(),
            "protected": protected,
            "value": if protected { Value::Null } else { json!(node.value) },
        }))
    }

    /// The deepest element of the application under a screen point.
    fn element_at(&self, root: &NativeRef, point: (i32, i32)) -> Option<NativeRef> {
        let deadline = Instant::now() + Duration::from_secs(2);
        for window in self.children(root, deadline).ok()? {
            let Ok(Some(mut current)) = self.accessible_at_point(&window, point, deadline) else {
                continue;
            };
            for _ in 0..32 {
                match self.accessible_at_point(&current, point, deadline) {
                    Ok(Some(next)) if next != current => current = next,
                    _ => break,
                }
            }
            return Some(current);
        }
        None
    }

    fn watch_signals(
        self,
        root: NativeRef,
        info: Map<String, Value>,
        stop: Arc<AtomicBool>,
        buffer: Arc<Mutex<Watched>>,
    ) {
        // Applications only emit what someone has asked the registry for.
        if let Ok(registry) = Proxy::new(
            &self.connection,
            REGISTRY_NAME,
            "/org/a11y/atspi/registry",
            "org.a11y.atspi.Registry",
        ) {
            // GTK's bridge reads the libatspi spelling; Qt 5 only recognises
            // the D-Bus one ("Object:TextChanged") and ignores the other.
            for event in [
                "object:text-changed",
                "object:state-changed:checked",
                "object:state-changed:focused",
                "Object:TextChanged",
                "Object:StateChanged",
            ] {
                if registry
                    .call_method("RegisterEvent", &(event, Vec::<String>::new(), ""))
                    .is_err()
                {
                    let _ = registry.call_method("RegisterEvent", &(event,));
                }
            }
        }
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(root.bus_name.as_str())
            .and_then(|builder| builder.interface("org.a11y.atspi.Event.Object"))
            .map(|builder| builder.build());
        let Ok(rule) = rule else { return };
        let Ok(messages) =
            zbus::blocking::MessageIterator::for_match_rule(rule, &self.connection, Some(256))
        else {
            return;
        };
        for message in messages {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(message) = message else { continue };
            let header = message.header();
            let (Some(member), Some(path)) = (header.member(), header.path()) else {
                continue;
            };
            let member = member.to_string();
            let target = NativeRef {
                bus_name: root.bus_name.clone(),
                object_path: path.to_string(),
            };
            let body = message.body();
            let (kind, detail) = match body.deserialize::<(
                String,
                i32,
                i32,
                zbus::zvariant::OwnedValue,
                std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            )>() {
                Ok(fields) => (fields.0, fields.1),
                Err(_) => {
                    match body.deserialize::<(String, i32, i32, zbus::zvariant::OwnedValue)>() {
                        Ok(fields) => (fields.0, fields.1),
                        Err(_) => continue,
                    }
                }
            };
            let at_ms = monotonic_ms();
            match (member.as_str(), kind.as_str()) {
                ("TextChanged", _) => {
                    if let Some(node) = self.describe(&target, &info) {
                        buffer
                            .lock()
                            .unwrap()
                            .push(json!({"kind": "text", "at_ms": at_ms, "node": node}));
                    }
                }
                ("StateChanged", "checked") => {
                    if let Some(node) = self.describe(&target, &info) {
                        buffer.lock().unwrap().push(
                            json!({"kind": "toggle", "checked": detail == 1, "at_ms": at_ms, "node": node}),
                        );
                    }
                }
                ("StateChanged", "focused") if detail == 1 => {
                    let node = self.describe(&target, &info);
                    buffer.lock().unwrap().focused = node;
                }
                _ => {}
            }
        }
    }

    fn watch_input(
        self,
        root: NativeRef,
        info: Map<String, Value>,
        output: std::process::ChildStdout,
        buffer: Arc<Mutex<Watched>>,
    ) {
        use std::io::BufRead;
        for line in std::io::BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let at_ms = value["at_ms"].as_u64().unwrap_or_else(monotonic_ms);
            match value["event"].as_str() {
                Some("button") => {
                    let point = (
                        value["x"].as_i64().unwrap_or(0) as i32,
                        value["y"].as_i64().unwrap_or(0) as i32,
                    );
                    let node = self
                        .element_at(&root, point)
                        .and_then(|target| self.describe(&target, &info))
                        .unwrap_or(Value::Null);
                    let event = match value["button"].as_u64() {
                        Some(1) => json!({"kind": "click", "button": "left"}),
                        Some(2) => json!({"kind": "click", "button": "middle"}),
                        Some(3) => json!({"kind": "click", "button": "right"}),
                        Some(4) => json!({"kind": "wheel", "direction": "up"}),
                        Some(5) => json!({"kind": "wheel", "direction": "down"}),
                        Some(6) => json!({"kind": "wheel", "direction": "left"}),
                        Some(7) => json!({"kind": "wheel", "direction": "right"}),
                        _ => continue,
                    };
                    let mut event = event;
                    event["at_ms"] = json!(at_ms);
                    event["node"] = node;
                    buffer.lock().unwrap().push(event);
                }
                Some("key") => {
                    if let Some(keys) = chord(&value) {
                        let mut buffer = buffer.lock().unwrap();
                        let node = buffer.focused.clone().unwrap_or(Value::Null);
                        buffer.push(
                            json!({"kind": "key", "keys": keys, "at_ms": at_ms, "node": node}),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn start_watch(
        &self,
        application: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<String, AutomationError> {
        let (root, info) = self.resolve_application(application, deadline)?;
        let process_id = info
            .get("process_id")
            .and_then(Value::as_u64)
            .or_else(|| self.process_id(&root.bus_name).map(u64::from))
            .ok_or_else(|| {
                driver_error(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "the application has no process id",
                )
            })?;
        let helper = self.helper(HelperKind::Input)?;
        let mut command = Command::new(&helper);
        helper_environment(&mut command);
        command
            .args([
                "record-input".to_string(),
                "--expected-pid".into(),
                process_id.to_string(),
                "--deadline-monotonic-ns".into(),
                monotonic_deadline_ns(Instant::now() + WATCH_LIFETIME)?.to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut input = command.spawn().map_err(|error| {
            driver_error(
                "DRIVER.UNAVAILABLE",
                format!("cannot start the input watcher: {error}"),
            )
        })?;
        let output = input.stdout.take().expect("stdout is piped");
        let stop = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(Mutex::new(Watched::default()));
        // Each source gets its own connection, so a slow element read in one
        // never holds up the other or the provider's own requests.
        let signals = LinuxBackend::new()?;
        let pointer = LinuxBackend::new()?;
        {
            let (root, info, stop, buffer) =
                (root.clone(), info.clone(), stop.clone(), buffer.clone());
            std::thread::spawn(move || signals.watch_signals(root, info, stop, buffer));
        }
        {
            let buffer = buffer.clone();
            std::thread::spawn(move || pointer.watch_input(root, info, output, buffer));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.recordings.lock().unwrap().insert(
            id.clone(),
            LinuxRecorder {
                stop,
                buffer,
                input,
            },
        );
        Ok(id)
    }

    fn collect_watch(&self, id: &str) -> Result<Value, AutomationError> {
        let recordings = self.recordings.lock().unwrap();
        let recorder = recordings
            .get(id)
            .ok_or_else(|| driver_error("DRIVER.NOT_FOUND", "no such recording"))?;
        let mut buffer = recorder.buffer.lock().unwrap();
        let mut events: Vec<Value> = buffer.events.drain(..).collect();
        events.sort_by_key(|event| event["at_ms"].as_u64().unwrap_or(0));
        let dropped = std::mem::take(&mut buffer.dropped);
        Ok(json!({"events": events, "dropped": dropped}))
    }

    fn stop_watch(&self, id: &str) -> Result<(), AutomationError> {
        let Some(mut recorder) = self.recordings.lock().unwrap().remove(id) else {
            return Err(driver_error("DRIVER.NOT_FOUND", "no such recording"));
        };
        recorder.stop.store(true, Ordering::SeqCst);
        // Closing stdin tells the helper to stop; it checks every 100 ms.
        drop(recorder.input.stdin.take());
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if matches!(recorder.input.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = recorder.input.kill();
        let _ = recorder.input.wait();
        Ok(())
    }
}

#[cfg(test)]
mod watching {
    use super::*;

    #[test]
    fn only_chords_and_named_keys_become_steps() {
        let key = |name: &str, ctrl: bool| json!({"key": name, "ctrl": ctrl, "shift": false, "alt": false, "super": false});
        assert_eq!(
            chord(&key("s", true)),
            Some(vec!["ctrl".to_string(), "s".to_string()])
        );
        assert_eq!(
            chord(&key("Return", false)),
            Some(vec!["enter".to_string()])
        );
        assert_eq!(chord(&key("F5", false)), Some(vec!["f5".to_string()]));
        assert_eq!(
            chord(&key("a", false)),
            None,
            "plain typing is recorded as text"
        );
        assert_eq!(chord(&key("Left", false)), None);
        assert_eq!(chord(&key("dead_acute", true)), None);
    }
}
