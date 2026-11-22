use super::{Recorder, Result};
use crate::{
    cdp::Client,
    picker::{self, Download, FilePicker},
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

struct Pending {
    page: String,
    trigger: usize,
    enter: Option<usize>,
    download: Download,
    terminal: Option<(bool, Instant)>,
    since: Instant,
    next_probe: Instant,
}

#[derive(Default)]
pub(super) struct Downloads {
    active: HashSet<String>,
    after: usize,
    pending: Option<Pending>,
    lost: bool,
}

// Native accessibility handles stay on the run thread, including the final drain.
pub(super) struct Held {
    dialog: FilePicker,
    guid: String,
    checkpoint: usize,
    owner: (i64, [i32; 4]),
}

impl Downloads {
    pub(super) fn invalidate(&mut self, after: usize) {
        self.pending = None;
        self.after = after;
    }
    pub(super) fn lost_events(&mut self) {
        self.lost = true;
        self.pending = None;
    }
    pub(super) fn waiting(&self) -> bool {
        self.pending.is_some()
    }
    pub(super) fn progress(&mut self, page: &str, p: &Value) {
        let Some(guid) = p["guid"].as_str() else {
            self.lost_events();
            return;
        };
        if p["state"] == "inProgress" && !self.active.contains(guid) {
            // A transfer that predates this page subscription can also own a prompt.
            if self.active.len() >= 128 {
                self.lost_events();
            } else {
                self.active.insert(guid.into());
                self.pending = None;
            }
        }
        let completed = match p["state"].as_str() {
            Some("completed") => true,
            Some("canceled") => false,
            _ => return,
        };
        self.active.remove(guid);
        if let Some(pending) = self
            .pending
            .as_mut()
            .filter(|d| d.page == page && d.download.guid == guid)
        {
            pending.terminal = Some((completed, Instant::now()));
        }
    }
}

impl Recorder {
    pub(super) fn download_begin(&mut self, index: usize, p: &Value) -> Result<()> {
        let start = self.downloads.after;
        self.downloads.invalidate(self.steps.len());
        let Some(guid) = p["guid"].as_str() else {
            self.downloads.lost_events();
            return Ok(());
        };
        let overlapped = !self.downloads.active.is_empty();
        if self.downloads.active.len() >= 128 {
            self.downloads.lost_events();
        }
        if self.downloads.lost {
            return Ok(());
        }
        self.downloads.active.insert(guid.into());
        if overlapped {
            return Ok(());
        }
        let page = &self.pages[index].id;
        let mut actions = self.steps[start..]
            .iter()
            .rev()
            .filter(|s| s.page_id == *page && s.kind == "action");
        let Some(click) = actions.next() else {
            return Ok(());
        };
        if click.action["kind"] != "click"
            || click.action["trusted"] != true
            || click.action["download_link"] != true
            || super::now().abs_diff(click.action["at_ms"].as_u64().unwrap_or(0)) > 15000
        {
            return Ok(());
        }
        let href = click.action["href"].as_str().unwrap_or("");
        let Some(download) = Download::from_begin(p, &click.frame_id, href) else {
            return Ok(());
        };
        // A delayed response from an earlier activation of the same link
        // cannot be distinguished from the last click by frame and URL.
        let enter = click.action["enter_sequence"]
            .as_u64()
            .and_then(|sequence| {
                actions
                    .clone()
                    .next()
                    .filter(|s| {
                        s.action["kind"] == "enter"
                            && s.action["trusted"] == true
                            && s.frame_id == click.frame_id
                            && s.action["document_id"] == click.action["document_id"]
                            && s.action["node_id"] == click.action["node_id"]
                            && s.action["sequence"] == sequence
                    })
                    .map(|s| s.id)
            });
        if actions.any(|s| {
            s.frame_id == click.frame_id && s.action["kind"] == "click" && s.action["href"] == href
        }) {
            return Ok(());
        }
        self.downloads.pending = Some(Pending {
            page: page.clone(),
            trigger: click.id,
            enter,
            download,
            terminal: None,
            since: Instant::now(),
            next_probe: Instant::now(),
        });
        Ok(())
    }

    fn download_window(
        &mut self,
        index: usize,
        trigger: usize,
        deadline: Instant,
    ) -> Result<(Client, i64, [i32; 4])> {
        let action = &self.steps[trigger];
        let page = &mut self.pages[index];
        let context = page
            .contexts
            .iter()
            .find_map(|((session, id), ctx)| {
                (session.is_empty() && ctx.frame == action.frame_id).then_some(*id)
            })
            .ok_or("download document disappeared")?;
        let live = super::eval(
            &mut page.client,
            "",
            context,
            "window === top && length === 0 && globalThis.__aadCdp?.documentId",
        )?;
        if !live.is_string() || live != action.action["document_id"] {
            return Err("download document changed".into());
        }
        let mut browser = Client::browser_until(self.options.port, deadline)?;
        browser.set_deadline(deadline);
        let (id, bounds) = picker::window(&mut browser, &page.id)?;
        Ok((browser, id, bounds))
    }

    pub(super) fn observe_download(&mut self, index: usize, held: &mut Option<Held>) -> Result<()> {
        if held.as_ref().is_some_and(|h| {
            self.downloads
                .pending
                .as_ref()
                .is_none_or(|p| p.download.guid != h.guid)
        }) {
            *held = None;
        }
        let Some(pending) = self
            .downloads
            .pending
            .as_mut()
            .filter(|p| p.page == self.pages[index].id)
        else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        let page = pending.page.clone();
        let trigger = pending.trigger;
        let enter = pending.enter;
        let frame = self.steps[trigger].frame_id.clone();
        if let Some((completed, at)) = pending.terminal {
            let Some(native) = held.as_ref() else {
                self.downloads.pending = None;
                return Ok(());
            };
            let row = native.checkpoint;
            let owner = native.owner;
            let closed = native.dialog.save_closed(deadline);
            if matches!(closed, Ok(false)) && at.elapsed() < Duration::from_secs(1) {
                return Ok(());
            }
            let confirmed = closed.unwrap_or(false)
                && self.download_window(index, trigger, deadline).is_ok_and(
                    |(mut browser, id, bounds)| {
                        (id, bounds) == owner
                            && picker::verify_owner(
                                &mut browser,
                                &page,
                                owner,
                                native.dialog.process_only(),
                            )
                            .is_ok()
                    },
                );
            let outcome = if !confirmed {
                "save_failed"
            } else if completed {
                "save_as"
            } else {
                "download_canceled"
            };
            let checkpoint = &mut self.steps[row];
            checkpoint.kind = "save_picker_finished".into();
            checkpoint.action["outcome"] = json!(outcome);
            self.storage.append(
                "evidence/actions.jsonl",
                &serde_json::to_value(&*checkpoint)?,
            )?;
            self.steps[trigger].action["kind"] = json!(outcome);
            self.downloads.pending = None;
            *held = None;
            return Ok(());
        }
        if held.is_some()
            || pending.since.elapsed() > Duration::from_secs(15)
            || Instant::now() < pending.next_probe
        {
            return Ok(());
        }
        pending.next_probe = Instant::now() + Duration::from_millis(100);
        let guid = pending.download.guid.clone();
        let suggestion = pending.download.suggestion.clone();
        let observed = (|| -> Result<_> {
            let (mut browser, id, bounds) = self.download_window(index, trigger, deadline)?;
            let pid = picker::pid(&mut browser)?;
            let dialog = picker::download_dialog(pid, bounds, &suggestion, deadline)?;
            picker::verify_owner(&mut browser, &page, (id, bounds), dialog.process_only())?;
            Ok((dialog, (id, bounds)))
        })();
        if let Ok((dialog, owner)) = observed {
            let gesture = if enter.is_some() { "enter" } else { "click" };
            let row=self.new_step(&page,&frame,"save_picker_pending",json!({"source":"download","trigger_id":trigger,"enter_id":enter,"trigger":gesture}))?;
            if let Some(id) = enter {
                self.steps[id].action["kind"] = json!("file_trigger");
            }
            let action = &mut self.steps[trigger].action;
            action["kind"] = json!("save_pending");
            action["trigger"] = json!(gesture);
            action["source"] = json!("download");
            *held = Some(Held {
                dialog,
                owner,
                checkpoint: row,
                guid,
            });
        }
        Ok(())
    }
}
