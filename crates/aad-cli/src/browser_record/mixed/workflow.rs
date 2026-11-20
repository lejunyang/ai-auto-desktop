use super::{
    native::{Watch, Window, PROVIDER},
    Result,
};
use aad_control::steps::{self, Step};
use serde_json::{json, Value};

pub fn compile(watch: &Watch, steps: &[Step], windows: &[Option<Window>]) -> Result<Value> {
    // Workflow inputs are interpolated by the runtime. Captured page and
    // application text is literal data, including when it resembles a template.
    if json!([watch.app_name, steps, windows])
        .to_string()
        .contains("${{")
    {
        return Err(
            "captured literal contains workflow template syntax; cannot export safely".into(),
        );
    }
    let mut workflow = steps::workflow("mixed-recording", steps)?;
    let original = workflow["steps"].as_array_mut().unwrap();
    let mut compiled = Vec::new();
    for ((step, window), control_step) in steps.iter().zip(windows).zip(original.drain(..)) {
        let Some(window) = window else {
            compiled.push(control_step);
            continue;
        };
        watch.compile_action(step, window, &mut compiled)?;
    }
    workflow["budgets"]["max_executed_steps"] = json!(compiled.len());
    workflow["steps"] = json!(compiled);
    workflow["requires"]["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":PROVIDER,"version":">=0.1.0"}));
    aad_core::compile_descriptor(workflow.clone(), None)?;
    Ok(workflow)
}
