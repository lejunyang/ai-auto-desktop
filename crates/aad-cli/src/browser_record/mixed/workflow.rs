use super::{native::PROVIDER, sources::Watch, timeline::ReplaySource, Result};
use aad_control::steps::{self, Step};
use serde_json::{json, Value};

pub fn compile(watches: &[Watch], steps: &[Step], windows: &[ReplaySource]) -> Result<Value> {
    // Workflow inputs are interpolated by the runtime. Captured page and
    // application text is literal data, including when it resembles a template.
    if watches
        .iter()
        .any(|watch| matches!(watch, Watch::Linux(w) if w.app_name.contains("${{")))
        || json!([steps, windows]).to_string().contains("${{")
    {
        return Err(
            "captured literal contains workflow template syntax; cannot export safely".into(),
        );
    }
    let mut workflow = steps::workflow("mixed-recording", steps)?;
    let original = std::mem::take(workflow["steps"].as_array_mut().unwrap());
    let capabilities = workflow["requires"]["capabilities"].as_array_mut().unwrap();
    let mut compiled = Vec::new();
    for ((step, source), control_step) in steps.iter().zip(windows).zip(original) {
        match source {
            ReplaySource::Browser => compiled.push(control_step),
            ReplaySource::Linux(index, window) => {
                let Some(Watch::Linux(watch)) = watches.get(*index) else {
                    return Err("unknown AT-SPI capture source".into());
                };
                watch.compile_action(step, window, &mut compiled)?;
                if !capabilities.iter().any(|c| c["name"] == PROVIDER) {
                    capabilities.push(json!({"name":PROVIDER,"version":">=0.1.0"}));
                }
            }
            ReplaySource::Portable(index) => {
                let Some(Watch::Portable(watch)) = watches.get(*index) else {
                    return Err("unknown native capture source".into());
                };
                watch.compile_action(step, &mut compiled, capabilities)?;
            }
        }
    }
    workflow["budgets"]["max_executed_steps"] = json!(compiled.len());
    workflow["steps"] = json!(compiled);
    aad_core::compile_descriptor(workflow.clone(), None)?;
    Ok(workflow)
}
