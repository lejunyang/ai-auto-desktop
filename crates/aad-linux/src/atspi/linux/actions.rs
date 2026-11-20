//! AT-SPI GetActions contains localized labels. Only GetName is the
//! machine-readable action identifier; an empty translation is not an index.
use super::*;

const MAX_ACTIONS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NativeAction {
    pub name: String,
    pub localized_name: String,
    pub description: String,
    pub keybinding: String,
}

trait ActionSource {
    fn count(&self) -> Result<i32, AutomationError>;
    fn name(&self, index: i32) -> Result<String, AutomationError>;
    fn localized(&self) -> Result<Vec<atspi::Action>, AutomationError>;
    fn dispatch(&self, index: i32) -> Result<bool, AutomationError>;
}

fn unsupported() -> AutomationError {
    driver_error(
        "DRIVER.ACTION_UNSUPPORTED",
        "target has no unique qualified native action",
    )
}

fn read(source: &impl ActionSource) -> Result<Vec<NativeAction>, AutomationError> {
    let count = usize::try_from(source.count()?)
        .ok()
        .filter(|count| *count <= MAX_ACTIONS)
        .ok_or_else(unsupported)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let localized = source.localized()?;
    if localized.len() != count {
        return Err(unsupported());
    }
    let mut result = Vec::with_capacity(count);
    for (index, label) in localized.into_iter().enumerate() {
        let name = source.name(index as i32)?;
        if name.len() > 128 || name.chars().any(char::is_control) {
            return Err(unsupported());
        }
        result.push(NativeAction {
            name,
            localized_name: label.name,
            description: label.description,
            keybinding: label.keybinding,
        });
    }
    if source.count()? != count as i32 {
        return Err(super::super::stale(
            "native action list changed while reading",
        ));
    }
    Ok(result)
}

fn is_activation(name: &str) -> bool {
    ["click", "press", "activate"]
        .iter()
        .any(|verb| name.eq_ignore_ascii_case(verb))
}

pub(super) fn unique_activation(actions: &[NativeAction]) -> Option<&NativeAction> {
    let mut matches = actions.iter().filter(|action| is_activation(&action.name));
    let selected = matches.next()?;
    matches.next().is_none().then_some(selected)
}

fn dispatch_named(source: &impl ActionSource, expected: &str) -> Result<Value, AutomationError> {
    dispatch_selected(source, expected, true)
}

fn dispatch_selected(
    source: &impl ActionSource,
    expected: &str,
    activation: bool,
) -> Result<Value, AutomationError> {
    // The expected canonical name comes from the provider's fresh target.
    // Re-resolve the live index, never reuse an observed numerical index.
    if !is_activation(expected) {
        return Err(unsupported());
    }
    let actions = read(source)?;
    let selected = if activation {
        unique_activation(&actions).ok_or_else(unsupported)?
    } else {
        // toggle/expand/collapse select a specified verb, not a generic
        // activation. Other activation verbs do not make that verb ambiguous.
        let mut matches = actions
            .iter()
            .filter(|action| action.name.eq_ignore_ascii_case(expected));
        let selected = matches.next().ok_or_else(unsupported)?;
        if matches.next().is_some() {
            return Err(unsupported());
        }
        selected
    };
    if activation && selected.name != expected {
        return Err(super::super::stale(
            "native action changed after target validation",
        ));
    }
    let index = actions
        .iter()
        .position(|action| std::ptr::eq(action, selected))
        .expect("selected action belongs to the list");
    // A same-length reordering must not redirect DoAction to another verb.
    if source.count()? != actions.len() as i32 || source.name(index as i32)? != selected.name {
        return Err(super::super::stale(
            "native action slot changed before dispatch",
        ));
    }
    if !source.dispatch(index as i32)? {
        return Err(dispatch_uncertain(driver_error(
            "DRIVER.ACTION_FAILED",
            "Action.DoAction did not accept the request",
        )));
    }
    Ok(json!({
        "native_interface":"Action.do_action", "native_action_name":selected.name,
        "native_action":{"index":index,"name":selected.name,
            "localized_name":selected.localized_name,"description":selected.description,
            "key_binding":selected.keybinding},
        "accepted":true,"dispatched":true,"no_op":false
    }))
}

fn dispatch_uncertain(error: AutomationError) -> AutomationError {
    error
        .with_effect("unknown")
        .with_detail("dispatch_started", json!(true))
}

struct DbusActions<'a> {
    backend: &'a LinuxBackend,
    target: &'a NativeRef,
    deadline: Instant,
}

fn canonical_count(proxy: &ActionProxy<'_>) -> zbus::Result<i32> {
    // atspi-proxies 0.11's `nactions` accessor maps to `Nactions`, but AT-SPI
    // specifies the case-sensitive wire property `NActions`.
    zbus::block_on(proxy.inner().get_property::<i32>("NActions"))
}

impl DbusActions<'_> {
    fn check(&self, dispatched: bool) -> Result<(), AutomationError> {
        remaining(self.deadline, dispatched)?;
        self.backend.check_cancelled(dispatched)
    }
}

impl ActionSource for DbusActions<'_> {
    fn count(&self) -> Result<i32, AutomationError> {
        self.check(false)?;
        let count = canonical_count(&self.backend.action_proxy(self.target)?)
            .map_err(|error| native_failure("Action.NActions", self.target, error))?;
        self.check(false)?;
        Ok(count)
    }

    fn name(&self, index: i32) -> Result<String, AutomationError> {
        self.check(false)?;
        let name = zbus::block_on(self.backend.action_proxy(self.target)?.get_name(index))
            .map_err(|error| native_failure("Action.GetName", self.target, error))?;
        self.check(false)?;
        Ok(name)
    }

    fn localized(&self) -> Result<Vec<atspi::Action>, AutomationError> {
        self.check(false)?;
        let actions = zbus::block_on(self.backend.action_proxy(self.target)?.get_actions())
            .map_err(|error| native_failure("Action.GetActions", self.target, error))?;
        self.check(false)?;
        Ok(actions)
    }

    fn dispatch(&self, index: i32) -> Result<bool, AutomationError> {
        self.check(false)?;
        let proxy = self.backend.action_proxy(self.target)?;
        self.check(false)?;
        let result = zbus::block_on(proxy.do_action(index)).map_err(|error| {
            dispatch_uncertain(native_failure("Action.DoAction", self.target, error))
        })?;
        self.check(true).map_err(dispatch_uncertain)?;
        Ok(result)
    }
}

impl LinuxBackend {
    pub(super) fn canonical_actions(
        &self,
        target: &NativeRef,
        deadline: Instant,
    ) -> Result<Vec<NativeAction>, AutomationError> {
        read(&DbusActions {
            backend: self,
            target,
            deadline,
        })
    }

    pub(super) fn invoke_canonical(
        &self,
        target: &NativeRef,
        expected: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        dispatch_named(
            &DbusActions {
                backend: self,
                target,
                deadline,
            },
            expected,
        )
    }

    pub(super) fn dispatch_canonical_verb(
        &self,
        target: &NativeRef,
        expected: &str,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        dispatch_selected(
            &DbusActions {
                backend: self,
                target,
                deadline,
            },
            expected,
            false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct WireActions;

    #[zbus::interface(name = "org.a11y.atspi.Action")]
    impl WireActions {
        #[zbus(property, name = "NActions")]
        fn count(&self) -> i32 {
            2
        }
    }

    #[test]
    #[ignore = "run under dbus-run-session; exercises a private real D-Bus service"]
    fn canonical_action_count_uses_exact_atspi_wire_property() {
        let server = zbus::block_on(
            zbus::connection::Builder::session()
                .unwrap()
                .serve_at("/action", WireActions)
                .unwrap()
                .build(),
        )
        .unwrap();
        let client = zbus::block_on(zbus::Connection::session()).unwrap();
        let proxy = zbus::block_on(
            ActionProxy::builder(&client)
                .destination(server.unique_name().unwrap().as_str())
                .unwrap()
                .path("/action")
                .unwrap()
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )
        .unwrap();
        assert_eq!(canonical_count(&proxy).unwrap(), 2);
        assert!(zbus::block_on(proxy.inner().get_property::<i32>("Nactions")).is_err());
    }

    struct Source {
        names: Vec<String>,
        localized: Vec<String>,
        calls: Cell<usize>,
        changed_count: bool,
        changed_slot: bool,
        accepted: bool,
        dispatched: RefCell<Vec<i32>>,
    }

    impl Source {
        fn new(names: &[&str]) -> Self {
            Self {
                names: names.iter().map(|s| (*s).into()).collect(),
                localized: vec![String::new(); names.len()],
                calls: Cell::new(0),
                changed_count: false,
                changed_slot: false,
                accepted: true,
                dispatched: RefCell::new(Vec::new()),
            }
        }
    }

    impl ActionSource for Source {
        fn count(&self) -> Result<i32, AutomationError> {
            let n = self.names.len() as i32;
            Ok(n + i32::from(self.changed_count && self.calls.get() > 0))
        }
        fn name(&self, index: i32) -> Result<String, AutomationError> {
            self.calls.set(self.calls.get() + 1);
            if self.changed_slot && self.calls.get() > self.names.len() {
                return Ok("showContextMenu".into());
            }
            Ok(self.names[index as usize].clone())
        }
        fn localized(&self) -> Result<Vec<atspi::Action>, AutomationError> {
            Ok(self
                .localized
                .iter()
                .map(|name| atspi::Action {
                    name: name.clone(),
                    description: String::new(),
                    keybinding: String::new(),
                })
                .collect())
        }
        fn dispatch(&self, index: i32) -> Result<bool, AutomationError> {
            self.dispatched.borrow_mut().push(index);
            Ok(self.accepted)
        }
    }

    #[test]
    fn canonical_verb_survives_empty_or_unrelated_localized_label() {
        let mut source = Source::new(&["showContextMenu", "press"]);
        source.localized = vec!["press".into(), String::new()];
        let actions = read(&source).unwrap();
        assert_eq!(unique_activation(&actions).unwrap().name, "press");
        let result = dispatch_named(&source, "press").unwrap();
        assert_eq!(result["native_action"]["index"], 1);
        assert_eq!(*source.dispatched.borrow(), vec![1]);
        assert_eq!(result["native_action"]["localized_name"], "");
        let qt = Source::new(&["Press", "SetFocus"]);
        dispatch_named(&qt, "Press").unwrap();
        assert_eq!(*qt.dispatched.borrow(), vec![0]);
    }

    #[test]
    fn unknown_empty_and_multiple_activation_verbs_never_dispatch() {
        for names in [
            vec![""],
            vec!["showContextMenu"],
            vec!["focus"],
            vec!["press", "click"],
            vec!["Press", "press"],
        ] {
            let source = Source::new(&names);
            assert!(dispatch_named(&source, "press").is_err());
            assert!(source.dispatched.borrow().is_empty());
        }
    }

    #[test]
    fn fresh_name_count_or_final_slot_changes_never_dispatch() {
        let changed_verb = Source::new(&["click", "showContextMenu"]);
        assert!(dispatch_named(&changed_verb, "press").is_err());
        assert!(changed_verb.dispatched.borrow().is_empty());
        for (changed_count, changed_slot) in [(true, false), (false, true)] {
            let mut source = Source::new(&["press", "showContextMenu"]);
            source.changed_count = changed_count;
            source.changed_slot = changed_slot;
            assert!(dispatch_named(&source, "press").is_err());
            assert!(source.dispatched.borrow().is_empty());
        }
    }

    #[test]
    fn specific_gtk_verbs_preserve_case_and_ignore_other_activation_verbs() {
        let source = Source::new(&["Click", "Activate"]);
        assert!(dispatch_named(&source, "Click").is_err());
        dispatch_selected(&source, "click", false).unwrap();
        dispatch_selected(&source, "activate", false).unwrap();
        assert_eq!(*source.dispatched.borrow(), vec![0, 1]);
        let duplicate = Source::new(&["click", "Click", "activate"]);
        assert!(dispatch_selected(&duplicate, "click", false).is_err());
        assert!(duplicate.dispatched.borrow().is_empty());
    }

    #[test]
    fn failed_native_dispatch_is_uncertain_and_marks_dispatch_started() {
        let mut source = Source::new(&["press"]);
        source.accepted = false;
        let error = dispatch_named(&source, "press").unwrap_err();
        assert_eq!(error.effect, "unknown");
        assert_eq!(error.details["dispatch_started"], true);
        let transport = dispatch_uncertain(driver_error("DRIVER.ACTION_FAILED", "transport"));
        assert_eq!(transport.effect, "unknown");
        assert_eq!(transport.details["dispatch_started"], true);
    }
}
