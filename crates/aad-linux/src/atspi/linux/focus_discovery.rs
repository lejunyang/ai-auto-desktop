//! Discover actual focused editors omitted by an application's child enumeration.
//! An event is a hint, never an input capability or a substitute for a fresh node.
use super::*;

const EVENT: &str = "object:state-changed:focused";
const MAX_FOCUS_ANCESTORS: usize = 32;

pub(super) struct FocusDiscovery {
    application: NativeRef,
    process_id: u32,
    connection: Connection,
    latest: Arc<Mutex<Option<NativeRef>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl FocusDiscovery {
    fn start(address: &str, application: NativeRef, process_id: u32) -> zbus::Result<Self> {
        // A separate connection lets close wake a blocked reader on scope
        // changes and drop; it does not borrow the provider's request queue.
        let connection = connection::Builder::address(address)?
            .method_timeout(Duration::from_millis(500))
            .build()?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(application.bus_name.as_str())?
            .interface("org.a11y.atspi.Event.Object")?
            .member("StateChanged")?
            .add_arg("focused")?
            .build();
        let messages =
            zbus::blocking::MessageIterator::for_match_rule(rule, &connection, Some(16))?;
        Proxy::new(
            &connection,
            REGISTRY_NAME,
            "/org/a11y/atspi/registry",
            "org.a11y.atspi.Registry",
        )?
        .call_method(
            "RegisterEvent",
            &(EVENT, Vec::<String>::new(), application.bus_name.as_str()),
        )?;
        // No fallback to unscoped registration on old registry versions.
        let latest = Arc::new(Mutex::new(None));
        let captured = latest.clone();
        let sender = application.bus_name.clone();
        let reader = std::thread::spawn(move || {
            for message in messages {
                let Ok(message) = message else { break };
                let header = message.header();
                if header.sender().map(|value| value.as_str()) != Some(sender.as_str()) {
                    continue;
                }
                let Some(path) = header.path() else { continue };
                let Some(focused) = focus_state(&message) else {
                    continue;
                };
                let reference = NativeRef {
                    bus_name: sender.clone(),
                    object_path: path.to_string(),
                };
                let mut last = captured.lock().unwrap();
                if focused {
                    *last = Some(reference);
                } else if last.as_ref() == Some(&reference) {
                    *last = None;
                }
            }
        });
        Ok(Self {
            application,
            process_id,
            connection,
            latest,
            reader: Some(reader),
        })
    }
}

impl Drop for FocusDiscovery {
    fn drop(&mut self) {
        if let Ok(registry) = Proxy::new(
            &self.connection,
            REGISTRY_NAME,
            "/org/a11y/atspi/registry",
            "org.a11y.atspi.Registry",
        ) {
            let _ = registry.call_method(
                "DeregisterEvent",
                &(EVENT, self.application.bus_name.as_str()),
            );
        }
        let _ = self.connection.clone().close();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn focus_state(message: &zbus::Message) -> Option<bool> {
    let body = message.body();
    let fields = body
        .deserialize::<(
            String,
            i32,
            i32,
            zbus::zvariant::OwnedValue,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        )>()
        .map(|fields| (fields.0, fields.1))
        .or_else(|_| {
            body.deserialize::<(
                String,
                i32,
                i32,
                zbus::zvariant::OwnedValue,
                (String, zbus::zvariant::OwnedObjectPath),
            )>()
            .map(|fields| (fields.0, fields.1))
        });
    let (kind, detail) = fields.ok()?;
    (kind == "focused" && matches!(detail, 0 | 1)).then_some(detail == 1)
}

fn application_identity(info: &Map<String, Value>) -> Option<(NativeRef, u32)> {
    Some((
        NativeRef {
            bus_name: info.get("bus_name")?.as_str()?.into(),
            object_path: info.get("object_path")?.as_str()?.into(),
        },
        u32::try_from(info.get("process_id")?.as_u64()?)
            .ok()
            .filter(|pid| *pid > 0)?,
    ))
}

fn contains(outer: Bounds, inner: Bounds) -> bool {
    outer.width > 0
        && outer.height > 0
        && inner.width > 0
        && inner.height > 0
        && inner.x >= outer.x
        && inner.y >= outer.y
        && i64::from(inner.x) + i64::from(inner.width)
            <= i64::from(outer.x) + i64::from(outer.width)
        && i64::from(inner.y) + i64::from(inner.height)
            <= i64::from(outer.y) + i64::from(outer.height)
}

fn focused_editor(
    role: atspi::Role,
    interfaces: atspi::InterfaceSet,
    states: atspi::StateSet,
) -> bool {
    matches!(
        role,
        atspi::Role::Text | atspi::Role::Entry | atspi::Role::PasswordText
    ) && interfaces.contains(Interface::EditableText)
        && interfaces.contains(Interface::Component)
        && [
            State::Focused,
            State::Focusable,
            State::Editable,
            State::Enabled,
            State::Sensitive,
        ]
        .into_iter()
        .all(|state| states.contains(state))
        && !states.contains(State::Defunct)
}

/// The nearest listed ancestor must be inside the actual requested scope.
/// Listed ancestors retained merely for context cannot authorize a sibling.
fn attachment(
    nodes: &[BackendNode],
    chain: &[NativeRef],
    root: &NativeRef,
) -> Option<(usize, usize)> {
    if !chain.contains(root) {
        return None;
    }
    let (chain_index, index) =
        chain
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(distance, reference)| {
                nodes
                    .iter()
                    .position(|node| &node.native == reference)
                    .map(|index| (distance, index))
            })?;
    let mut current = index;
    for _ in 0..=nodes.len() {
        if &nodes[current].native == root {
            return Some((chain_index, index));
        }
        current = nodes[current].parent_index?;
    }
    None
}

impl LinuxBackend {
    pub(super) fn ensure_focus_discovery(&self, info: &Map<String, Value>, deadline: Instant) {
        let Some((application, process_id)) = application_identity(info) else {
            return;
        };
        let mut current = self.focus_discovery.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|watch| watch.application == application && watch.process_id == process_id)
        {
            return;
        }
        // Disconnect the old app before registering the new one.
        *current = None;
        if remaining(deadline, false).is_ok()
            && self.process_id(&application.bus_name) == Some(process_id)
        {
            *current =
                FocusDiscovery::start(&self.accessibility_address, application, process_id).ok();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn append_focused_branch(
        &self,
        scope: &NativeRef,
        snapshot: &mut BackendSnapshot,
        max_depth: u32,
        max_nodes: usize,
        input: Option<&InputSupport>,
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        let Some((application, process_id)) = application_identity(&snapshot.application) else {
            return Ok(());
        };
        let target = {
            let watch = self.focus_discovery.lock().unwrap();
            watch
                .as_ref()
                .filter(|watch| watch.application == application && watch.process_id == process_id)
                .and_then(|watch| watch.latest.lock().unwrap().clone())
        };
        let Some(target) = target else { return Ok(()) };
        if snapshot.nodes.iter().any(|node| node.native == target) {
            return Ok(());
        }
        // Invalid/stale event hints cannot make an ordinary observation fail.
        // Identity and geometry are checked before any editor value is read.
        let Ok(Some((chain, bounds))) =
            self.focus_branch_identity(&target, &application, process_id, deadline)
        else {
            return Ok(());
        };
        let Some((distance, attach)) = attachment(&snapshot.nodes, &chain, scope) else {
            return Ok(());
        };
        let mut visible_ancestor = Some(attach);
        let mut geometry_proven = false;
        while let Some(index) = visible_ancestor {
            if let Some(ancestor_bounds) = snapshot.nodes[index]
                .bounds
                .filter(|bounds| bounds.width > 0 && bounds.height > 0)
            {
                geometry_proven = contains(ancestor_bounds, bounds);
                break;
            }
            visible_ancestor = snapshot.nodes[index].parent_index;
        }
        if !geometry_proven {
            return Ok(());
        }
        let mut depth = distance as u32;
        let mut ancestor = attach;
        while &snapshot.nodes[ancestor].native != scope {
            depth += 1;
            let Some(parent) = snapshot.nodes[ancestor].parent_index else {
                return Ok(());
            };
            ancestor = parent;
        }
        let needed = distance; // missing context ancestors plus the editor root
        if snapshot.nodes.len() + needed > max_nodes || depth > max_depth {
            snapshot.truncated = true;
            capture_issue(
                &mut snapshot.truncation_reasons,
                json!({"code": "focused_branch_limit", "max_nodes": max_nodes, "max_depth": max_depth}),
            );
            return Ok(());
        }
        let mut additions = Vec::new();
        let mut parent = attach;
        for reference in chain[1..distance].iter().rev() {
            let Ok((mut node, _)) = self.read_node(
                reference.clone(),
                Some(parent),
                &snapshot.application,
                input,
                deadline,
            ) else {
                return Ok(());
            };
            // Their children were not enumerated; never grant parent actions.
            node.actions.clear();
            node.provenance.insert("focus_context".into(), json!(true));
            parent = snapshot.nodes.len() + additions.len();
            additions.push(node);
        }
        let available = max_nodes - snapshot.nodes.len() - additions.len();
        let Ok(mut branch) = self.capture_tree_nodes(
            target,
            snapshot.application.clone(),
            Vec::new(),
            max_depth - depth,
            available,
            input,
            deadline,
        ) else {
            return Ok(());
        };
        let Some(editor) = branch.nodes.first_mut() else {
            return Ok(());
        };
        if editor.states.get("focused") != Some(&Some(true)) || editor.bounds != Some(bounds) {
            return Ok(());
        }
        editor
            .provenance
            .insert("focus_discovery".into(), json!("application_scoped_event"));
        let offset = snapshot.nodes.len() + additions.len();
        for (index, mut node) in branch.nodes.into_iter().enumerate() {
            node.parent_index = if index == 0 {
                Some(parent)
            } else {
                node.parent_index.map(|index| index + offset)
            };
            additions.push(node);
        }
        snapshot.nodes.extend(additions);
        if branch.truncated {
            snapshot.truncated = true;
            capture_issue(
                &mut snapshot.truncation_reasons,
                json!({"code": "focused_branch_truncated"}),
            );
        }
        Ok(())
    }

    fn focus_branch_identity(
        &self,
        target: &NativeRef,
        application: &NativeRef,
        process_id: u32,
        deadline: Instant,
    ) -> Result<Option<(Vec<NativeRef>, Bounds)>, AutomationError> {
        remaining(deadline, false)?;
        if target.bus_name != application.bus_name
            || self.process_id(&target.bus_name) != Some(process_id)
        {
            return Ok(None);
        }
        let accessible = self.accessible(target)?;
        let role = zbus::block_on(accessible.get_role())
            .map_err(|error| native_failure("focus role", target, error))?;
        let states = zbus::block_on(accessible.get_state())
            .map_err(|error| native_failure("focus state", target, error))?;
        let interfaces = zbus::block_on(accessible.get_interfaces())
            .map_err(|error| native_failure("focus interfaces", target, error))?;
        if !focused_editor(role, interfaces, states) {
            return Ok(None);
        }
        let app = Self::native(
            zbus::block_on(accessible.get_application())
                .map_err(|error| native_failure("focus application", target, error))?,
        );
        if app != *application {
            return Ok(None);
        }
        let (x, y, width, height) =
            zbus::block_on(self.component_proxy(target)?.get_extents(CoordType::Screen))
                .map_err(|error| native_failure("focus bounds", target, error))?;
        let bounds = Bounds {
            x,
            y,
            width,
            height,
        };
        if width <= 0 || height <= 0 {
            return Ok(None);
        }
        let mut chain = vec![target.clone()];
        while chain.last() != Some(application) {
            let Some(parent) = self.parent(chain.last().expect("target exists"), deadline)? else {
                return Ok(None);
            };
            if parent.bus_name != application.bus_name
                || chain.contains(&parent)
                || chain.len() >= MAX_FOCUS_ANCESTORS
            {
                return Ok(None);
            }
            chain.push(parent);
        }
        Ok(Some((chain, bounds)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(path: &str) -> NativeRef {
        NativeRef {
            bus_name: ":1.42".into(),
            object_path: path.into(),
        }
    }

    fn node(path: &str, parent: Option<usize>) -> BackendNode {
        BackendNode {
            native: reference(path),
            parent_index: parent,
            role: "filler".into(),
            name: None,
            description: None,
            value: None,
            attributes: BTreeMap::new(),
            states: BTreeMap::new(),
            bounds: None,
            actions: Vec::new(),
            provenance: Map::new(),
        }
    }

    #[test]
    fn focus_event_payloads_only_contribute_a_focus_bit() {
        for (kind, detail, expected) in [
            ("focused", 1, Some(true)),
            ("focused", 0, Some(false)),
            ("focused", 2, None),
            ("checked", 1, None),
        ] {
            let message =
                zbus::Message::signal("/editor", "org.a11y.atspi.Event.Object", "StateChanged")
                    .unwrap()
                    .build(&(
                        kind,
                        detail,
                        0i32,
                        zbus::zvariant::OwnedValue::from(0i32),
                        std::collections::HashMap::<String, zbus::zvariant::OwnedValue>::new(),
                    ))
                    .unwrap();
            assert_eq!(focus_state(&message), expected);
        }
        let qt = zbus::Message::signal("/editor", "org.a11y.atspi.Event.Object", "StateChanged")
            .unwrap()
            .build(&(
                "focused",
                1i32,
                0i32,
                zbus::zvariant::OwnedValue::from(0i32),
                (
                    ":1.42",
                    zbus::zvariant::OwnedObjectPath::try_from("/root").unwrap(),
                ),
            ))
            .unwrap();
        assert_eq!(focus_state(&qt), Some(true));
    }

    #[test]
    fn event_source_requires_real_editable_role_interface_and_live_focus() {
        let required = [
            State::Focused,
            State::Focusable,
            State::Editable,
            State::Enabled,
            State::Sensitive,
        ];
        let interfaces = [Interface::EditableText, Interface::Component]
            .into_iter()
            .collect();
        let states = required.into_iter().collect();
        assert!(focused_editor(atspi::Role::Text, interfaces, states));
        assert!(focused_editor(
            atspi::Role::PasswordText,
            interfaces,
            states
        ));
        assert!(!focused_editor(atspi::Role::TableCell, interfaces, states));
        for removed in required {
            assert!(!focused_editor(
                atspi::Role::Text,
                interfaces,
                required
                    .into_iter()
                    .filter(|state| *state != removed)
                    .collect()
            ));
        }
        assert!(!focused_editor(
            atspi::Role::Text,
            [Interface::Component].into_iter().collect(),
            states
        ));
        assert!(!focused_editor(
            atspi::Role::Text,
            interfaces,
            required.into_iter().chain([State::Defunct]).collect()
        ));
    }

    #[test]
    fn event_attachment_cannot_escape_the_requested_scope_through_context() {
        let nodes = vec![
            node("/app", None),
            node("/frame", Some(0)),
            node("/scope", Some(1)),
            node("/child", Some(2)),
        ];
        let valid = ["/editor", "/missing", "/child", "/scope", "/frame", "/app"].map(reference);
        assert_eq!(
            attachment(&nodes, &valid, &reference("/scope")),
            Some((2, 3))
        );
        let outside = ["/editor", "/frame", "/app"].map(reference);
        assert_eq!(attachment(&nodes, &outside, &reference("/scope")), None);
        // A forged tail containing scope does not fix the nearest ancestor's
        // structural position outside the observed scope.
        let forged = ["/editor", "/frame", "/scope", "/app"].map(reference);
        assert_eq!(attachment(&nodes, &forged, &reference("/scope")), None);
        assert_eq!(
            attachment(&nodes, &outside, &reference("/app")),
            Some((1, 1))
        );
    }

    #[test]
    fn focus_geometry_must_be_positive_and_contained_without_integer_overflow() {
        let outer = Bounds {
            x: 10,
            y: 20,
            width: 100,
            height: 50,
        };
        let inner = Bounds {
            x: 20,
            y: 25,
            width: 50,
            height: 10,
        };
        assert!(contains(outer, inner));
        assert!(!contains(outer, Bounds { width: 0, ..inner }));
        assert!(!contains(outer, Bounds { x: 100, ..inner }));
        assert!(!contains(
            outer,
            Bounds {
                x: i32::MAX,
                ..inner
            }
        ));
    }
}
