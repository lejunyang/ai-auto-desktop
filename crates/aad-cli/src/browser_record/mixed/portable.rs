//! Window-attributed capture for the Windows UIA and macOS AX providers.
//! The mixed recorder checks the selected PID against CDP's browser process
//! list before starting either subscription.

use aad_browser::recording::Result;
use aad_control::{steps::Step, Session};
use aad_runtime::Provider;
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc, time::Duration};

mod ax;
#[cfg(test)]
mod tests;
mod windows;

const WINDOWS: &str = "desktop.windows_uia";
const MACOS: &str = "desktop.macos_ax";

#[derive(Debug)]
pub enum Event {
    Step(Box<Step>),
    Gap(String),
    Filtered(&'static str),
}

type Rows = Vec<(u64, Event)>;

#[derive(Clone)]
struct Client(Arc<dyn Provider>);

impl Client {
    fn call(&self, action: &str, args: Value) -> Result<Value> {
        let name = &self.0.manifest().name;
        Ok(self.0.invoke(
            &format!("{name}.{action}@1"),
            args,
            Some(Duration::from_secs(10)),
        )?)
    }

    fn subscribe(&self, args: Value) -> Result<(Capture, Value)> {
        let result = self.call("watch", args)?;
        let id = string(&result, "capture_id")?.to_owned();
        Ok((
            Capture {
                client: self.clone(),
                id: Some(id),
            },
            result,
        ))
    }
}

struct Capture {
    client: Client,
    id: Option<String>,
}

impl Capture {
    fn release(&mut self) -> Result<()> {
        if let Some(id) = &self.id {
            self.client.call("release", json!({"capture_id":id}))?;
            self.id = None;
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn string<'a>(v: &'a Value, field: &str) -> Result<&'a str> {
    v[field]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("native capture omitted {field}").into())
}

fn number(v: &Value, field: &str) -> Result<u64> {
    v[field]
        .as_u64()
        .ok_or_else(|| format!("native capture omitted {field}").into())
}

enum Source {
    Windows(windows::Windows),
    Ax(ax::Ax),
}

pub struct Watch {
    source: Source,
    next: usize,
    released: bool,
}

impl Watch {
    pub fn start(
        session: &Session,
        resource: &Value,
        window: &str,
        dialogs: &[String],
    ) -> Result<Self> {
        let name = string(resource, "provider")?;
        let mut titles = HashSet::new();
        for title in std::iter::once(window).chain(dialogs.iter().map(String::as_str)) {
            if title.trim().is_empty() || !titles.insert(title) {
                return Err("window and dialog titles must be nonempty and distinct".into());
            }
        }
        let provider = session
            .providers()
            .iter()
            .find(|p| p.manifest().name == name)
            .ok_or("recording provider unavailable")?
            .clone();
        let client = Client(provider);
        let meta = &resource["metadata"];
        if number(meta, "process_id")? == 0 {
            return Err("native target has no process identity".into());
        }
        let source = match name {
            WINDOWS => Source::Windows(windows::Windows::start(client, meta, window, dialogs)?),
            MACOS => Source::Ax(ax::Ax::start(client, meta, window, dialogs)?),
            _ => return Err(format!("unsupported native capture provider {name}").into()),
        };
        Ok(Self {
            source,
            next: 1,
            released: false,
        })
    }

    pub fn collect(&mut self) -> Result<Vec<(u64, Event)>> {
        if self.released {
            return Err("native capture is already released".into());
        }
        let mut rows = match &mut self.source {
            Source::Windows(w) => w.collect(&mut self.next)?,
            Source::Ax(w) => w.collect(&mut self.next)?,
        };
        rows.sort_by_key(|(at, _)| *at);
        Ok(rows)
    }

    pub fn release(&mut self) -> Result<()> {
        self.released = true;
        match &mut self.source {
            Source::Windows(w) => w.release(),
            Source::Ax(w) => w.capture.release(),
        }
    }

    /// Use this bound when reporting cross-source ties on the mixed timeline.
    pub fn precision_ms(&self) -> u64 {
        match &self.source {
            Source::Windows(w) => w.precision_ms,
            Source::Ax(w) => w.precision_ms,
        }
    }

    pub fn compile_action(
        &self,
        step: &Step,
        out: &mut Vec<Value>,
        capabilities: &mut Vec<Value>,
    ) -> Result<()> {
        step.validate()?;
        reject_template(step)?;
        let (provider, actions) = match &self.source {
            Source::Ax(ax) => (MACOS, ax.actions(step)?),
            Source::Windows(_) if step.app.provider.as_deref() == Some(WINDOWS) => {
                let mut doc =
                    aad_control::steps::workflow("mixed-recording", std::slice::from_ref(step))?;
                (
                    WINDOWS,
                    std::mem::take(doc["steps"].as_array_mut().unwrap()),
                )
            }
            _ => return Err("native step belongs to another provider".into()),
        };
        if !capabilities.iter().any(|c| c["name"] == provider) {
            capabilities.push(json!({"name":provider,"version":">=0.1.0"}));
        }
        if provider == WINDOWS
            && !capabilities
                .iter()
                .any(|c| c["name"] == aad_control::steps::PROVIDER)
        {
            capabilities.push(json!({"name":aad_control::steps::PROVIDER,"version":">=0.1.0"}));
        }
        out.extend(actions);
        Ok(())
    }
}

fn reject_template(step: &Step) -> Result<()> {
    if serde_json::to_string(step)?.contains("${{") {
        return Err("captured literal contains workflow template syntax".into());
    }
    Ok(())
}
