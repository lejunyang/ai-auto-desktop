//! Turning observed events into something worth recording.
//!
//! Two native mechanisms feed this module, because measurement showed neither
//! sees everything (see `docs/plan/rust-port-status.md` §2.17–2.18): UI
//! Automation event handlers are blind to clicks in WinForms and anything else
//! behind the MSAA bridge, while WinEvent hooks see those but resolve web
//! content only as far as its container. Whichever mechanism observed an
//! interaction, it arrives here as a [`CapturedEvent`].
//!
//! Everything in this file is platform independent so it can be tested without
//! a desktop. The native subscription lives in `windows.rs`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::model::{Locator, Node};

/// What a user did, as far as the platform was able to tell us.
///
/// Deliberately smaller than the set of events the platform emits: a recorder
/// that faithfully replayed every notification would produce a workflow full of
/// focus changes and repaint noise that no person would recognise as what they
/// did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A control was activated: a button press, a menu choice, a link.
    Invoked,
    /// A control's value became something else, such as text being typed.
    ValueChanged,
    /// A control was switched on or off, or a selection moved.
    StateChanged,
    /// Keyboard focus arrived somewhere new.
    ///
    /// Kept because it is the only signal some toolkits give before an
    /// interaction, and because it disambiguates which control a later value
    /// change belongs to. It is not, by itself, worth a recorded step.
    Focused,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Invoked => "invoked",
            EventKind::ValueChanged => "value_changed",
            EventKind::StateChanged => "state_changed",
            EventKind::Focused => "focused",
        }
    }

    /// Which recorded action replays this interaction.
    ///
    /// `Focused` has none: focus is context, not something a person set out to
    /// do, and replaying it would add steps nobody performed.
    pub fn action(self) -> Option<&'static str> {
        match self {
            EventKind::Invoked => Some("invoke"),
            EventKind::ValueChanged => Some("set_value"),
            // A toggle or a selection is activated the same way it was reached.
            EventKind::StateChanged => Some("invoke"),
            EventKind::Focused => None,
        }
    }

    /// Whether this event on its own justifies a step in the recording.
    pub fn is_recordable(self) -> bool {
        self.action().is_some()
    }
}

/// One observed interaction, with the element it happened to.
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    /// Position in the observed order. Never reused, and gaps are meaningful:
    /// they mark events dropped under pressure.
    pub sequence: u64,
    pub kind: EventKind,
    /// The element as it looked when the event fired.
    ///
    /// Absent when the platform could not resolve one -- which happens for web
    /// content reached through a WinEvent hook. Recorded rather than discarded,
    /// because "something happened that we could not attribute" is a fact the
    /// user needs, and silently dropping it is what makes a recorder appear to
    /// work while missing steps.
    pub node: Option<Node>,
    /// Which mechanism observed it, so a gap can be diagnosed later.
    pub source: &'static str,
    /// When it was observed.
    ///
    /// Needed to tell one click reported several times from several clicks:
    /// the platform gives no other way to distinguish them.
    pub observed_at: Instant,
}

impl CapturedEvent {
    pub fn to_json(&self) -> Value {
        json!({
            "sequence": self.sequence,
            "kind": self.kind.as_str(),
            "action": self.kind.action(),
            "source": self.source,
            "node": self.node.as_ref().map(|node| node.to_json()),
        })
    }

    /// Whether two events concern the same control.
    ///
    /// Compares identity, not the value, so a run of edits to one field is
    /// recognisable as one interaction. An unresolved element matches nothing,
    /// including another unresolved one: two things we could not identify are
    /// not thereby the same thing.
    fn same_element_as(&self, other: &CapturedEvent) -> bool {
        match (&self.node, &other.node) {
            (Some(left), Some(right)) => {
                left.role == right.role
                    && left.name == right.name
                    && left.automation_id == right.automation_id
                    && left.class_name == right.class_name
            }
            _ => false,
        }
    }
}

/// How close together repeated notifications must be to count as one click.
///
/// Measured: a single WinForms button click raises three state changes, all
/// within a few milliseconds. A person cannot click twice that fast -- the
/// fastest deliberate double click is around 100ms between presses, and even a
/// mis-fired double click is well outside this window once the second press is
/// a separate intent. Chosen an order of magnitude below that so a genuine
/// rapid double click is still recorded as two.
const BURST_WINDOW: Duration = Duration::from_millis(40);

/// Collapse consecutive value changes to one control into a single event.
///
/// Typing raises a value change per keystroke, and some toolkits raise several
/// per edit. One step per event would replay the text a character at a time, or
/// several times over.
///
/// Only *consecutive* runs collapse: anything else in between ends the run, so
/// edit / click / edit stays three interactions in the order they happened.
///
/// Clicks collapse too, but only within [`BURST_WINDOW`], because a single
/// WinForms click was measured raising three state changes while two
/// deliberate clicks are two interactions the user meant to make.
///
/// The **last** event of a run is kept, not the first, because the value a
/// person ended up with is the one they meant. Keeping the first would record
/// `h` when they typed `hello`.
pub fn coalesce(events: Vec<CapturedEvent>) -> Vec<CapturedEvent> {
    let mut collapsed: Vec<CapturedEvent> = Vec::with_capacity(events.len());
    for event in events {
        let extends_run = collapsed.last().is_some_and(|previous| {
            if !previous.same_element_as(&event) || previous.kind != event.kind {
                return false;
            }
            match event.kind {
                // Typing: every keystroke reports, however long the user takes.
                EventKind::ValueChanged => true,
                // Clicking: one click reports several times, but two clicks are
                // two interactions. Only a tight burst is the former.
                EventKind::StateChanged => event
                    .observed_at
                    .duration_since(previous.observed_at)
                    <= BURST_WINDOW,
                _ => false,
            }
        });
        if extends_run {
            // Replace: the final value is the one the user settled on. The
            // sequence number comes along with it, so ordering still reflects
            // when the interaction finished.
            collapsed.pop();
        }
        collapsed.push(event);
    }
    collapsed
}

/// Drop the events that are context rather than actions.
///
/// Focus changes dominate the raw stream -- a single click can produce several
/// -- and none of them is something a person would say they did.
pub fn recordable(events: Vec<CapturedEvent>) -> Vec<CapturedEvent> {
    events
        .into_iter()
        .filter(|event| event.kind.is_recordable())
        .collect()
}

/// A bounded, thread-safe buffer between native callbacks and the reader.
///
/// Callbacks arrive on threads we do not own -- measured: a UI Automation
/// callback landed on a different thread from the one that subscribed -- and
/// concurrently with whoever is draining. Hence the lock.
///
/// The buffer has a limit. When it overflows the oldest events are discarded
/// **and counted**: an unreported drop is indistinguishable from the user not
/// having done anything, which is precisely the failure this whole design
/// exists to prevent.
#[derive(Debug)]
pub struct EventBuffer {
    inner: Mutex<BufferState>,
    limit: usize,
}

#[derive(Debug, Default)]
struct BufferState {
    events: VecDeque<CapturedEvent>,
    dropped: u64,
    next_sequence: u64,
}

impl EventBuffer {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BufferState::default()),
            limit: limit.max(1),
        })
    }

    /// Record one observed event.
    ///
    /// Called from native callback threads. Must not panic into a COM caller:
    /// an exception crossing that boundary can tear down the subscription, so a
    /// poisoned lock is recovered from rather than propagated.
    pub fn push(&self, kind: EventKind, node: Option<Node>, source: &'static str) {
        let mut state = match self.inner.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let sequence = state.next_sequence;
        state.next_sequence += 1;
        state.events.push_back(CapturedEvent {
            sequence,
            kind,
            node,
            source,
            observed_at: Instant::now(),
        });
        while state.events.len() > self.limit {
            state.events.pop_front();
            state.dropped += 1;
        }
    }

    /// Take up to `max` events, oldest first, with the drop count since the
    /// last drain.
    pub fn drain(&self, max: usize) -> (Vec<CapturedEvent>, u64) {
        let mut state = match self.inner.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let take = max.min(state.events.len());
        let events: Vec<CapturedEvent> = state.events.drain(..take).collect();
        let dropped = state.dropped;
        state.dropped = 0;
        (events, dropped)
    }

    /// How many events are waiting, without taking them.
    pub fn pending(&self) -> usize {
        match self.inner.lock() {
            Ok(state) => state.events.len(),
            Err(poisoned) => poisoned.into_inner().events.len(),
        }
    }
}

// ---------------------------------------------------------------------------
// Turning captured events into recorded steps
//
// A step has to survive being written to a file and reopened tomorrow, so it
// carries a locator rather than a reference. Synthesising one needs the whole
// node table to judge uniqueness, but an event carries a single element -- so
// the caller supplies a snapshot of the window and captured elements are
// matched back into it.
// ---------------------------------------------------------------------------

/// One interaction, described so it can be replayed later.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedStep {
    /// The driver action to replay: `invoke`, `set_value`, `focus`.
    pub action: &'static str,
    /// How to find the element again. `None` when it could not be described,
    /// which is reported rather than guessed at.
    pub locator: Option<Locator>,
    /// A human-readable line describing what was interacted with.
    pub summary: String,
    /// The text to write, for actions that need one.
    pub argument: Option<String>,
    /// True when the element's contents are withheld by the platform.
    pub protected: bool,
    /// Why this step cannot replay, when it cannot.
    pub unresolved: Option<&'static str>,
}

impl RecordedStep {
    /// Whether this step could actually run.
    pub fn is_replayable(&self) -> bool {
        self.unresolved.is_none() && self.locator.is_some()
    }
}

/// Describe captured events as steps, using `nodes` to make locators unique.
///
/// `nodes` is normally a snapshot of the window taken when recording started.
/// Matching against it, rather than snapshotting per event, is what makes this
/// affordable and free of races: by the time a fresh snapshot came back the UI
/// would have moved on, and a dialog that was just dismissed would be gone.
///
/// Events whose element cannot be found in the snapshot still produce a step,
/// marked unresolved. Dropping them would be worse: the recording would look
/// complete while silently missing an interaction, and someone would replay it
/// expecting the steps they performed.
pub fn to_steps(events: Vec<CapturedEvent>, nodes: &[Node]) -> Vec<RecordedStep> {
    coalesce(recordable(events))
        .into_iter()
        .filter_map(|event| {
            // `recordable` already dropped the events that describe context
            // rather than an action, so anything without one here is a bug.
            let action = event.kind.action()?;

            // An element that cannot perform the action is not the element the
            // action happened to. Measured on Chromium: every write produced a
            // second event aimed at "Chrome Legacy Window", the Win32 host the
            // WinEvent hook reports, which offers only
            // `["invoke", "pointer_click"]` and so cannot have been what received
            // the text. It does not appear in the UI Automation snapshot either,
            // so it arrived as an unreplayable step beside each real one -- and
            // in a twelve-second recording one of those displaced a genuine
            // write.
            //
            // Judged on capability rather than on the name: names differ by
            // browser, by language and by Chromium version, while "an element
            // that does not support set_value is not the one just written to"
            // holds everywhere. An element that appeared after recording started
            // is still kept: it does support the action, it is merely absent from
            // the snapshot the locators are judged against, and dropping it would
            // make the recording silently miss a step.
            if let Some(node) = &event.node {
                if !node.actions.is_empty() && !node.actions.iter().any(|name| name == action) {
                    return None;
                }
            }
            Some(match &event.node {
                None => RecordedStep {
                    action,
                    locator: None,
                    summary: format!("an unidentified element ({})", event.kind.as_str()),
                    argument: None,
                    protected: false,
                    // The interaction happened; we simply could not say to what.
                    unresolved: Some("the element could not be identified"),
                },
                Some(node) => describe_step(action, node, nodes),
            })
        })
        .collect()
}

/// Build one step for an interaction with a known element.
fn describe_step(action: &'static str, node: &Node, nodes: &[Node]) -> RecordedStep {
    let protected = node.states.protected == Some(true);
    // The text a write replays. A protected field's contents are deliberately
    // absent -- the platform withholds them -- so the step records that a value
    // was entered without inventing one; it becomes a run-time input.
    let argument = if action == "set_value" && !protected {
        node.value.clone()
    } else {
        None
    };

    // Match the captured element back into the snapshot. Identity, not the
    // node_id: the captured element was described independently and its id
    // belongs to no snapshot.
    let found = nodes.iter().find(|candidate| same_element(candidate, node));

    let (locator, unresolved) = match found {
        Some(candidate) => match Locator::synthesize(candidate, nodes) {
            Some(locator) => (Some(locator), None),
            // Real outcome: several elements share every durable attribute.
            None => (
                None,
                Some("this element cannot be told apart from its siblings"),
            ),
        },
        None => (
            None,
            // Usually means the element appeared after recording started, so it
            // is not in the snapshot the locators are being judged against.
            Some("this element was not in the window when recording started"),
        ),
    };

    RecordedStep {
        action,
        locator,
        summary: node.summary(),
        argument,
        protected,
        unresolved,
    }
}

/// Whether two independently-described nodes are the same element.
///
/// Compared on identity rather than on every field, because a value changes as
/// it is typed into: the element that received the text is the same element it
/// was before, and requiring the value to match would fail to find any edited
/// field at all.
fn same_element(candidate: &Node, captured: &Node) -> bool {
    candidate.role == captured.role
        && candidate.name == captured.name
        && candidate.automation_id == captured.automation_id
        && candidate.class_name == captured.class_name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::States;
    use std::time::Duration;

    // -----------------------------------------------------------------------
    // Turning events into replayable steps
    //
    // The failure this guards against is a recording that looks complete and
    // replays nothing: a locator built from a per-run identifier, or a step
    // whose element was never identified but is marked ready to run.
    // -----------------------------------------------------------------------

    /// A node as the platform would describe it, with the fields that matter.
    fn full(
        id: &str,
        role: &str,
        name: Option<&str>,
        automation_id: Option<&str>,
        class_name: Option<&str>,
    ) -> Node {
        let mut node = node(name.unwrap_or(""), role);
        node.node_id = id.to_string();
        node.name = name.map(str::to_string);
        node.automation_id = automation_id.map(str::to_string);
        node.class_name = class_name.map(str::to_string);
        node.actions = vec![
            "focus".into(),
            "invoke".into(),
            "set_value".into(),
            "type_text".into(),
        ];
        node
    }

    fn captured(kind: EventKind, node: Node) -> CapturedEvent {
        CapturedEvent {
            sequence: 0,
            kind,
            node: Some(node),
            source: "test",
            observed_at: Instant::now(),
        }
    }

    #[test]
    fn an_event_aimed_at_something_that_cannot_perform_it_is_not_a_step() {
        // Measured on Chromium: every write produced a second event aimed at the
        // Win32 host window, which offers only invoke and pointer_click. It is
        // not the element that received the text, it is absent from the UI
        // Automation snapshot, and in a twelve-second recording the unreplayable
        // step it produced displaced a genuine write.
        //
        // The judgement is capability, not the name -- names differ by browser,
        // language and version.
        let mut host = full("host", "pane", Some("Chrome Legacy Window"), Some("739"), None);
        host.actions = vec!["invoke".into(), "pointer_click".into()];
        let field = full("e1", "edit", Some("City"), Some("billing-city"), None);
        let nodes = vec![field.clone()];

        let steps = to_steps(
            vec![
                captured(EventKind::ValueChanged, field),
                captured(EventKind::ValueChanged, host),
            ],
            &nodes,
        );

        assert_eq!(steps.len(), 1, "only the write itself: {steps:#?}");
        assert_eq!(steps[0].action, "set_value");
        assert!(steps[0].is_replayable());
    }

    #[test]
    fn an_element_that_appeared_mid_recording_is_still_recorded() {
        // It supports the action, so the interaction did happen -- it is simply
        // missing from the snapshot the locators are judged against. Dropping it
        // would make the recording silently miss a step, which is the failure
        // this design exists to prevent, so it is kept and marked instead.
        let button = full("late", "button", Some("Confirm"), None, None);
        let nodes = vec![full("e1", "edit", Some("City"), Some("billing-city"), None)];

        let steps = to_steps(vec![captured(EventKind::Invoked, button)], &nodes);

        assert_eq!(steps.len(), 1);
        assert!(!steps[0].is_replayable());
        assert!(steps[0].unresolved.is_some(), "the reason has to be stated");
    }

    #[test]
    fn an_interaction_becomes_a_step_that_can_find_its_element_again() {
        let button = full("e1", "button", Some("Submit"), None, None);
        let nodes = vec![button.clone(), full("e2", "edit", Some("Name"), None, None)];

        let steps = to_steps(vec![captured(EventKind::Invoked, button)], &nodes);

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].action, "invoke");
        assert!(steps[0].is_replayable());
        // And the locator has to actually select it, not merely exist.
        let locator = steps[0].locator.as_ref().expect("a locator");
        let selected = locator.resolve(&nodes);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].node_id, "e1");
    }

    #[test]
    fn a_locator_never_holds_an_identifier_the_toolkit_regenerates() {
        // Measured on WinForms: automation_id 7473382 became 15534534 and the
        // class name's tail changed too, on nothing more than a restart. A step
        // built from either replays perfectly today and fails tomorrow.
        //
        // Two elements alike except for those two fields, so a synthesiser that
        // reached for them would be able to tell these apart -- and would.
        let first = full(
            "e1",
            "edit",
            None,
            Some("7473382"),
            Some("WindowsForms10.EDIT.app.0.34473a7_r14_ad1"),
        );
        let second = full(
            "e2",
            "edit",
            None,
            Some("9178646"),
            Some("WindowsForms10.EDIT.app.0.34473a7_r14_ad1"),
        );
        let nodes = vec![first.clone(), second];

        let steps = to_steps(vec![captured(EventKind::ValueChanged, first)], &nodes);

        assert_eq!(steps.len(), 1);
        // Unresolvable is the honest answer here, and it is the right one: the
        // alternative is a step that quietly stops working.
        assert!(!steps[0].is_replayable(), "got {:?}", steps[0].locator);
        assert!(steps[0].unresolved.is_some());
    }

    #[test]
    fn an_author_written_identifier_is_still_used() {
        // The other half. Of the 64 automation ids across the applications open
        // on this machine, 62 are names like `view_1` and `MenuBar`, and they
        // showed no drift -- discarding the field wholesale would leave far more
        // elements undescribable than it protects.
        let first = full("e1", "edit", None, Some("searchBox"), None);
        let nodes = vec![first.clone(), full("e2", "edit", None, Some("filterBox"), None)];

        let steps = to_steps(vec![captured(EventKind::ValueChanged, first)], &nodes);

        assert!(steps[0].is_replayable());
        assert_eq!(
            steps[0].locator.as_ref().unwrap().automation_id.as_deref(),
            Some("searchBox")
        );
    }

    #[test]
    fn an_unidentified_interaction_is_recorded_but_not_marked_replayable() {
        // The interaction happened. Dropping it would leave a recording that
        // looks complete while missing a step someone performed; marking it
        // replayable would produce a workflow that fails at run time.
        let event = CapturedEvent {
            sequence: 0,
            kind: EventKind::Invoked,
            node: None,
            source: "test",
            observed_at: Instant::now(),
        };

        let steps = to_steps(vec![event], &[]);

        assert_eq!(steps.len(), 1, "the interaction must not vanish");
        assert!(!steps[0].is_replayable());
        assert!(steps[0].unresolved.is_some());
    }

    #[test]
    fn an_element_that_appeared_after_recording_started_says_so() {
        // Locators are judged against the snapshot taken at the start, so a
        // control that opened later is genuinely not in it. The distinction
        // matters to whoever fixes the step.
        let latecomer = full("e9", "button", Some("Confirm"), None, None);
        let nodes = vec![full("e1", "button", Some("Open"), None, None)];

        let steps = to_steps(vec![captured(EventKind::Invoked, latecomer)], &nodes);

        assert!(!steps[0].is_replayable());
        assert!(
            steps[0].unresolved.unwrap().contains("not in the window"),
            "{:?}",
            steps[0].unresolved
        );
    }

    #[test]
    fn typing_records_the_finished_text_not_the_first_keystroke() {
        // Every keystroke raises its own event. Keeping the first would record
        // `h` where the user typed `hello` -- and the recording would look fine.
        let mut field = full("e1", "edit", Some("Name"), None, None);
        let nodes = vec![field.clone()];

        let mut events = Vec::new();
        for (index, text) in ["h", "he", "hel", "hell", "hello"].iter().enumerate() {
            field.value = Some((*text).to_string());
            events.push(CapturedEvent {
                sequence: index as u64,
                kind: EventKind::ValueChanged,
                node: Some(field.clone()),
                source: "test",
                observed_at: Instant::now(),
            });
        }

        let steps = to_steps(events, &nodes);

        assert_eq!(steps.len(), 1, "one edit, not one step per keystroke");
        assert_eq!(steps[0].argument.as_deref(), Some("hello"));
    }

    #[test]
    fn a_password_is_recorded_as_a_step_without_its_value() {
        // The field is still worth recording -- automating a login is the point
        // -- but the text must not land in the file. It becomes a run-time input.
        let mut secret = full("e1", "edit", Some("Password"), Some("pwd"), None);
        secret.states.protected = Some(true);
        secret.value = None;
        let nodes = vec![secret.clone()];

        let steps = to_steps(vec![captured(EventKind::ValueChanged, secret)], &nodes);

        assert!(steps[0].is_replayable(), "a login step has to be usable");
        assert!(steps[0].protected);
        assert_eq!(steps[0].argument, None);
    }

    #[test]
    fn an_ordinary_field_keeps_the_text_that_was_typed() {
        // The counterpart to the password case: withholding ordinary values
        // would make a recording unable to reproduce what it recorded.
        let mut field = full("e1", "edit", Some("Search"), Some("search"), None);
        field.value = Some("quarterly report".into());
        let nodes = vec![field.clone()];

        let steps = to_steps(vec![captured(EventKind::ValueChanged, field)], &nodes);

        assert_eq!(steps[0].argument.as_deref(), Some("quarterly report"));
        assert!(!steps[0].protected);
    }

    #[test]
    fn an_edited_field_is_still_matched_after_its_value_changed() {
        // The captured element carries the new text while the start-of-recording
        // snapshot holds the old. Comparing values when matching would fail to
        // find any field that was actually typed into -- that is, all of them.
        let mut before = full("e1", "edit", Some("Name"), Some("nameBox"), None);
        before.value = Some(String::new());
        let mut after = before.clone();
        after.value = Some("Ada".into());

        let steps = to_steps(vec![captured(EventKind::ValueChanged, after)], &[before]);

        assert!(steps[0].is_replayable(), "{:?}", steps[0].unresolved);
        assert_eq!(steps[0].argument.as_deref(), Some("Ada"));
    }

    #[test]
    fn context_events_do_not_become_steps() {
        // Focus tells us where the user is looking, not what they did. Replaying
        // it would add clicks nobody made.
        let field = full("e1", "edit", Some("Name"), None, None);
        let events = vec![
            captured(EventKind::Focused, field.clone()),
            captured(EventKind::Invoked, field.clone()),
        ];

        let steps = to_steps(events, &[field]);

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].action, "invoke");
    }


    fn node(name: &str, role: &str) -> Node {
        Node {
            node_id: format!("n_{name}"),
            role: role.to_string(),
            name: Some(name.to_string()),
            value: None,
            automation_id: Some(format!("id_{name}")),
            class_name: None,
            framework_id: None,
            bounds: None,
            states: States::default(),
            actions: vec![],
            depth: 1,
            parent_id: None,
            children: vec![],
        }
    }

    fn event(sequence: u64, kind: EventKind, name: &str) -> CapturedEvent {
        // Spaced well beyond the burst window, so a test that does not care
        // about timing gets the "separate interactions" reading.
        at(sequence, kind, name, Duration::from_millis(500) * sequence as u32)
    }

    /// An event observed a given interval after a fixed origin.
    ///
    /// Timing decides whether repeated notifications are one click or several,
    /// so tests need to place events precisely rather than depend on how fast
    /// the test itself runs.
    fn at(sequence: u64, kind: EventKind, name: &str, after: Duration) -> CapturedEvent {
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let origin = *ORIGIN.get_or_init(Instant::now);
        CapturedEvent {
            sequence,
            kind,
            node: Some(node(name, "button")),
            source: "test",
            observed_at: origin + after,
        }
    }

    #[test]
    fn typing_into_one_field_becomes_a_single_step() {
        // Every keystroke raises an event; replaying each would type the text
        // one character at a time.
        let events = vec![
            event(0, EventKind::ValueChanged, "search"),
            event(1, EventKind::ValueChanged, "search"),
            event(2, EventKind::ValueChanged, "search"),
        ];
        assert_eq!(coalesce(events).len(), 1);
    }

    #[test]
    fn the_value_kept_is_the_one_the_user_ended_on() {
        // Keeping the first event of the run would record "h" for "hello".
        let mut first = event(0, EventKind::ValueChanged, "search");
        first.node.as_mut().unwrap().value = Some("h".into());
        let mut last = event(1, EventKind::ValueChanged, "search");
        last.node.as_mut().unwrap().value = Some("hello".into());

        let collapsed = coalesce(vec![first, last]);

        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            collapsed[0].node.as_ref().unwrap().value.as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn edits_to_different_fields_stay_separate() {
        let events = vec![
            event(0, EventKind::ValueChanged, "first"),
            event(1, EventKind::ValueChanged, "second"),
        ];
        assert_eq!(coalesce(events).len(), 2);
    }

    #[test]
    fn an_interaction_in_between_ends_the_run() {
        // edit / click / edit is three things the user did, in that order.
        let events = vec![
            event(0, EventKind::ValueChanged, "search"),
            event(1, EventKind::Invoked, "submit"),
            event(2, EventKind::ValueChanged, "search"),
        ];
        let collapsed = coalesce(events);
        assert_eq!(collapsed.len(), 3);
        assert_eq!(collapsed[1].kind, EventKind::Invoked);
    }

    #[test]
    fn repeated_clicks_are_all_kept() {
        // Two presses the user meant are two interactions.
        let events = vec![
            event(0, EventKind::Invoked, "submit"),
            event(1, EventKind::Invoked, "submit"),
        ];
        assert_eq!(coalesce(events).len(), 2);
    }

    #[test]
    fn one_click_reported_three_times_becomes_one_step() {
        // Found on a real WinForms window: a single button click raises three
        // state changes, so two clicks were being recorded as six steps. On a
        // Submit button that is three submissions.
        let events = vec![
            at(0, EventKind::StateChanged, "submit", Duration::ZERO),
            at(1, EventKind::StateChanged, "submit", Duration::from_millis(3)),
            at(2, EventKind::StateChanged, "submit", Duration::from_millis(7)),
        ];
        assert_eq!(coalesce(events).len(), 1);
    }

    #[test]
    fn two_deliberate_clicks_stay_two_steps() {
        // The other half of the rule. Without this, "collapse everything"
        // would pass the test above while losing real interactions.
        let events = vec![
            at(0, EventKind::StateChanged, "submit", Duration::ZERO),
            at(1, EventKind::StateChanged, "submit", Duration::from_millis(400)),
        ];
        assert_eq!(coalesce(events).len(), 2);
    }

    #[test]
    fn a_burst_on_one_control_does_not_swallow_another() {
        // Clicking two controls in quick succession is still two steps: the
        // burst rule is per element, not per moment.
        let events = vec![
            at(0, EventKind::StateChanged, "submit", Duration::ZERO),
            at(1, EventKind::StateChanged, "submit", Duration::from_millis(4)),
            at(2, EventKind::StateChanged, "cancel", Duration::from_millis(8)),
        ];
        let collapsed = coalesce(events);
        assert_eq!(collapsed.len(), 2);
        assert_eq!(
            collapsed[1].node.as_ref().unwrap().name.as_deref(),
            Some("cancel")
        );
    }

    #[test]
    fn typing_collapses_however_slowly_it_is_done() {
        // Timing gates clicks, not edits: someone typing thoughtfully still
        // produced one edit, and it must not become one step per keystroke.
        let events = vec![
            at(0, EventKind::ValueChanged, "search", Duration::ZERO),
            at(1, EventKind::ValueChanged, "search", Duration::from_secs(3)),
            at(2, EventKind::ValueChanged, "search", Duration::from_secs(9)),
        ];
        assert_eq!(coalesce(events).len(), 1);
    }

    #[test]
    fn unattributed_events_never_merge_with_each_other() {
        // Two things we could not identify are not thereby the same thing.
        let events = vec![
            CapturedEvent {
                sequence: 0,
                kind: EventKind::ValueChanged,
                node: None,
                source: "test",
                observed_at: Instant::now(),
            },
            CapturedEvent {
                sequence: 1,
                kind: EventKind::ValueChanged,
                node: None,
                source: "test",
                observed_at: Instant::now(),
            },
        ];
        assert_eq!(coalesce(events).len(), 2);
    }

    #[test]
    fn focus_changes_do_not_become_steps() {
        // A single click produces several; none is something a person did.
        let events = vec![
            event(0, EventKind::Focused, "search"),
            event(1, EventKind::Invoked, "submit"),
            event(2, EventKind::Focused, "submit"),
        ];
        let kept = recordable(events);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, EventKind::Invoked);
    }

    #[test]
    fn every_recordable_kind_maps_to_an_action() {
        // A kind that is recordable but has no action would produce a step the
        // runtime cannot execute.
        for kind in [
            EventKind::Invoked,
            EventKind::ValueChanged,
            EventKind::StateChanged,
            EventKind::Focused,
        ] {
            assert_eq!(kind.is_recordable(), kind.action().is_some(), "{kind:?}");
        }
    }

    #[test]
    fn a_full_buffer_reports_what_it_discarded() {
        // A silently dropped event looks exactly like the user doing nothing.
        let buffer = EventBuffer::new(2);
        for _ in 0..5 {
            buffer.push(EventKind::Invoked, None, "test");
        }
        let (events, dropped) = buffer.drain(10);
        assert_eq!(events.len(), 2);
        assert_eq!(dropped, 3);
    }

    #[test]
    fn the_events_kept_under_pressure_are_the_most_recent() {
        let buffer = EventBuffer::new(2);
        for _ in 0..4 {
            buffer.push(EventKind::Invoked, None, "test");
        }
        let (events, _) = buffer.drain(10);
        assert_eq!(
            events.iter().map(|event| event.sequence).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn sequence_numbers_survive_draining() {
        // Gaps let a reader detect loss; restarting the count would hide it.
        let buffer = EventBuffer::new(10);
        buffer.push(EventKind::Invoked, None, "test");
        let _ = buffer.drain(10);
        buffer.push(EventKind::Invoked, None, "test");
        let (events, _) = buffer.drain(10);
        assert_eq!(events[0].sequence, 1);
    }

    #[test]
    fn draining_less_than_is_pending_leaves_the_rest_in_order() {
        let buffer = EventBuffer::new(10);
        for _ in 0..3 {
            buffer.push(EventKind::Invoked, None, "test");
        }
        let (first, _) = buffer.drain(2);
        let (rest, _) = buffer.drain(10);
        assert_eq!(first.len(), 2);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].sequence, 2);
    }

    #[test]
    fn the_drop_count_resets_once_reported() {
        // Otherwise one overflow would be reported for ever.
        let buffer = EventBuffer::new(1);
        buffer.push(EventKind::Invoked, None, "test");
        buffer.push(EventKind::Invoked, None, "test");
        let (_, first) = buffer.drain(10);
        let (_, second) = buffer.drain(10);
        assert_eq!(first, 1);
        assert_eq!(second, 0);
    }
}
