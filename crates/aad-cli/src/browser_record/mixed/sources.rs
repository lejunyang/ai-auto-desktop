use super::{
    native, portable,
    timeline::{Source, Timeline},
    Result,
};
use aad_control::Session;
use serde_json::{json, Value};

pub enum Watch {
    Linux(native::Watch),
    Portable(portable::Watch),
}

impl Watch {
    pub fn start(
        session: &Session,
        resource: &Value,
        window: &str,
        dialogs: &[String],
    ) -> Result<Self> {
        if resource["provider"] == native::PROVIDER {
            Ok(Self::Linux(native::Watch::start(
                session, resource, window, dialogs,
            )?))
        } else {
            Ok(Self::Portable(portable::Watch::start(
                session, resource, window, dialogs,
            )?))
        }
    }

    pub fn precision_ms(&self) -> u64 {
        match self {
            Self::Linux(_) => 1,
            Self::Portable(watch) => watch.precision_ms(),
        }
    }

    pub fn collect(&mut self, timeline: &mut Timeline, offset: u64, index: usize) -> Result<()> {
        match self {
            Self::Linux(watch) => watch.collect(timeline, offset, index),
            Self::Portable(watch) => {
                for (at, event) in watch.collect()? {
                    match event {
                        portable::Event::Step(step) => timeline.push(
                            at,
                            Source::Portable(index),
                            serde_json::to_value(step)?,
                        )?,
                        portable::Event::Gap(reason) => timeline.gap(
                            at,
                            json!({"source":"native","source_index":index,"reason":reason}),
                        )?,
                        portable::Event::Filtered(reason) => timeline.filter(at, reason)?,
                    }
                }
                Ok(())
            }
        }
    }

    pub fn release(&mut self) -> Result<()> {
        match self {
            Self::Linux(watch) => watch.release(),
            Self::Portable(watch) => watch.release(),
        }
    }
}
