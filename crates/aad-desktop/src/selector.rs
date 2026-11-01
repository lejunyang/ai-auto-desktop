//! Turning an observed window into a selector that still matches later.
//!
//! A recording holds a window id, and an id is assigned per session: a workflow
//! using one runs in the session that recorded it and never again. So the window
//! has to be named by what persists -- the process, and the part of the title that
//! does not move.
//!
//! Both halves of the title work are needed together. Measured on the fixture:
//! the longest shared substring of two observations gives
//! `"AAD Complex Fixture | last="`, which drags in the prefix of a state label,
//! and narrowing that to whole words gives `"AAD Complex Fixture"`. Porting only
//! the first half would have produced a selector that looks stable and is not.

use crate::model::WindowInfo;
use serde_json::{json, Value};

/// Word-like runs of a title fragment, longest first.
///
/// Splitting on whitespace and the punctuation titles use as separators keeps
/// candidates to things a reader would recognise as a name, rather than a letter
/// that happens to be unique among today's windows.
fn word_runs(fragment: &str) -> Vec<String> {
    // The separators are boundaries, not candidates. A run also has to carry a
    // letter or digit: "|" on its own distinguishes nothing.
    let tokens: Vec<&str> = fragment
        .split(|c: char| c.is_whitespace() || "|-–—:：".contains(c))
        .map(|part| part.trim_end_matches(|c: char| !c.is_alphanumeric()))
        .filter(|part| part.chars().any(char::is_alphanumeric))
        .collect();

    let mut runs: Vec<String> = Vec::new();
    for length in 1..=tokens.len() {
        for start in 0..=tokens.len().saturating_sub(length) {
            let run = tokens[start..start + length].join(" ");
            let run = run.trim().to_string();
            // Only runs the title really contains: joining with a single space
            // can invent a phrase the original spelled differently.
            if !run.is_empty() && fragment.contains(&run) {
                runs.push(run);
            }
        }
    }

    // Longest first. Length was tried the other way round on this fixture and
    // picked "last", a fragment of a state label that happened to be short --
    // length says nothing about identifying value. The parts observed to change
    // have already been removed, so within what remains the fuller phrase is the
    // better name.
    runs.sort_by_key(|run| std::cmp::Reverse(run.chars().count()));
    runs.dedup();
    runs
}

/// The longest fragment every observed title shares.
///
/// Which part of a title is stable differs by application, so it is observed
/// rather than guessed. `"AGENTS.md - sweepx - Visual Studio Code"` keeps its tail
/// and changes its head; `"AAD Complex Fixture | last=save - Edge"` keeps its head
/// and changes its tail. Taking the first segment would pick the file name in the
/// first case, which is the very part that moves.
///
/// A contiguous run is what is wanted because the driver compares titles with
/// `contains`.
pub fn stable_title(observed: &[String]) -> String {
    let seen: Vec<&String> = observed.iter().filter(|title| !title.is_empty()).collect();
    if seen.is_empty() {
        return String::new();
    }
    // One observation is no evidence of what changes. Narrowing on it would be
    // guessing, and too narrow fails loudly while matching the wrong window is
    // silent.
    if seen.len() == 1 {
        return seen[0].clone();
    }

    let shortest = seen
        .iter()
        .min_by_key(|title| title.chars().count())
        .expect("seen is not empty");
    let chars: Vec<char> = shortest.chars().collect();

    for length in (1..=chars.len()).rev() {
        for start in 0..=chars.len() - length {
            let candidate: String = chars[start..start + length].iter().collect();
            if seen.iter().all(|title| title.contains(&candidate)) {
                return candidate;
            }
        }
    }
    String::new()
}

/// Name a window so the selector still finds it in a later session.
///
/// `open` is every window visible when the selector was made: a selector is only
/// worth saving if it picked out one window among them. Without that check a
/// title fragment shared by two windows looks fine now and acts on whichever one
/// the driver happens to see first later.
///
/// `observed` are the titles seen during the recording. More than one is what
/// licenses narrowing.
pub fn selector_for(
    target: &WindowInfo,
    open: &[WindowInfo],
    observed: &[String],
) -> Option<Value> {
    let pool: Vec<&WindowInfo> = if open.is_empty() {
        vec![target]
    } else {
        open.iter().collect()
    };

    let matches = |selector: &Value, candidate: &WindowInfo| -> bool {
        let process_ok = selector
            .get("process_name")
            .and_then(Value::as_str)
            .is_none_or(|want| {
                candidate
                    .process_name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(want))
            });
        // Substring, mirroring how the driver compares titles.
        let title_ok = selector
            .get("title")
            .and_then(Value::as_str)
            .is_none_or(|want| candidate.title.contains(want));
        process_ok && title_ok
    };

    let unique = |selector: &Value| -> bool {
        let hits: Vec<&&WindowInfo> = pool
            .iter()
            .filter(|candidate| matches(selector, candidate))
            .collect();
        hits.len() == 1 && hits[0].window_id == target.window_id
    };

    let mut selector = serde_json::Map::new();
    if let Some(process) = &target.process_name {
        selector.insert("process_name".into(), json!(process));
    }

    // Process alone, when it is enough. Fewer conditions is fewer things that can
    // drift between sessions.
    if !selector.is_empty() && unique(&Value::Object(selector.clone())) {
        return Some(Value::Object(selector));
    }

    let shared = stable_title(observed);
    let shared = if shared.is_empty() {
        target.title.clone()
    } else {
        shared
    };

    // Whole words first, longest first: those read as names. The full fragment is
    // the fallback, since it is what was actually seen.
    let mut candidates = word_runs(&shared);
    if !candidates.iter().any(|run| run == &shared) {
        candidates.push(shared.clone());
    }

    for candidate in candidates {
        let mut attempt = selector.clone();
        attempt.insert("title".into(), json!(candidate));
        let attempt = Value::Object(attempt);
        if unique(&attempt) {
            return Some(attempt);
        }
    }

    // Nothing distinguished it. Reporting that is better than returning a
    // selector that matches several windows: the caller can say so, where a
    // wrong selector acts on the wrong window without a word.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, title: &str, process: &str) -> WindowInfo {
        WindowInfo {
            window_id: id.into(),
            title: title.into(),
            process_id: 1,
            process_name: Some(process.into()),
            class_name: Some("Chrome_WidgetWin_1".into()),
            bounds: None,
            is_foreground: false,
            is_minimized: false,
        }
    }

    #[test]
    fn a_state_label_is_left_out_of_the_name() {
        // Measured on the fixture: the shared substring of two observations is
        // "AAD Complex Fixture | last=", which carries the prefix of a label that
        // changes on every interaction. Whole words remove it.
        let observed = vec![
            "AAD Complex Fixture | last=none - Edge".to_string(),
            "AAD Complex Fixture | last=page-save: - Edge".to_string(),
        ];
        let shared = stable_title(&observed);
        assert!(
            shared.contains("last="),
            "the substring really does carry it"
        );

        let runs = word_runs(&shared);
        assert_eq!(
            runs.first().map(String::as_str),
            Some("AAD Complex Fixture")
        );
        assert!(
            !runs.iter().any(|run| run.contains("last=")),
            "a run must not end in punctuation: {runs:?}"
        );
    }

    #[test]
    fn a_title_that_changes_at_the_front_keeps_its_tail() {
        // The other shape, and the reason "take the first segment" is wrong: the
        // first segment here is the file name, which is what moves.
        let observed = vec![
            "AGENTS.md - sweepx - Visual Studio Code".to_string(),
            "main.rs - sweepx - Visual Studio Code".to_string(),
        ];
        let shared = stable_title(&observed);
        assert!(shared.contains("Visual Studio Code"));
        assert!(!shared.contains("AGENTS.md"));
    }

    #[test]
    fn one_observation_is_not_narrowed() {
        // Too narrow fails loudly; matching the wrong window is silent. With one
        // observation there is no evidence about what changes.
        let observed = vec!["Untitled - Notepad".to_string()];
        assert_eq!(stable_title(&observed), "Untitled - Notepad");
    }

    #[test]
    fn the_process_alone_is_used_when_it_identifies_the_window() {
        let target = window("w1", "AAD Complex Fixture | last=none", "msedge.exe");
        let open = vec![
            target.clone(),
            window("w2", "Untitled - Notepad", "notepad.exe"),
        ];
        let selector = selector_for(&target, &open, &[]).expect("a selector");
        assert_eq!(selector["process_name"], "msedge.exe");
        assert!(
            selector.get("title").is_none(),
            "fewer conditions drift less: {selector}"
        );
    }

    #[test]
    fn a_title_is_added_when_the_process_has_several_windows() {
        let target = window("w1", "AAD Complex Fixture | last=none - Edge", "msedge.exe");
        let open = vec![
            target.clone(),
            window("w2", "Inbox - Outlook Web - Edge", "msedge.exe"),
        ];
        let observed = vec![
            "AAD Complex Fixture | last=none - Edge".to_string(),
            "AAD Complex Fixture | last=save - Edge".to_string(),
        ];
        let selector = selector_for(&target, &open, &observed).expect("a selector");
        assert_eq!(selector["process_name"], "msedge.exe");
        assert_eq!(selector["title"], "AAD Complex Fixture");
    }

    #[test]
    fn a_window_that_cannot_be_told_apart_is_reported_rather_than_guessed() {
        // Two windows with the same process and the same title. A selector would
        // match both and act on whichever the driver saw first -- silently.
        let target = window("w1", "Document - Editor", "editor.exe");
        let open = vec![
            target.clone(),
            window("w2", "Document - Editor", "editor.exe"),
        ];
        assert!(selector_for(&target, &open, &[]).is_none());
    }

    #[test]
    fn a_run_has_to_appear_in_the_title_as_written() {
        // Joining tokens with one space can invent a phrase the title spelled
        // with different spacing, and a selector is compared against the real
        // title.
        let runs = word_runs("Alpha   Beta - Gamma");
        for run in &runs {
            assert!(
                "Alpha   Beta - Gamma".contains(run.as_str()),
                "invented {run:?}"
            );
        }
    }
}
