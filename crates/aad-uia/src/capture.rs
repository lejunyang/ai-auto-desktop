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

use crate::model::Node;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::States;
    use std::time::Duration;

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
