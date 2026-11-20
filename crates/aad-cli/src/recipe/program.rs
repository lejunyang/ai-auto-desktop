use super::library::{Call, Library};
use super::scope::{identifier, Scope};
use super::{resolve_targets, App, Locator, Phase, Step};
use aad_control::steps;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const FORMAT: &str = "recipe/2";
const MAX_NODES: usize = 500;
const MAX_DEPTH: usize = 16;
const MAX_EXECUTIONS: u64 = 100_000;

type Result<T> = std::result::Result<T, String>;
type Targets = BTreeMap<String, Locator>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Program {
    kind: String,
    format: String,
    name: String,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    targets: Targets,
    phases: Vec<Phase>,
    #[serde(default)]
    outputs: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Branch {
    #[serde(rename = "if")]
    condition: Value,
    then: Vec<Value>,
    #[serde(rename = "else", default)]
    otherwise: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Foreach {
    foreach: Value,
    #[serde(rename = "as")]
    item: String,
    index_as: Option<String>,
    max_items: u64,
    steps: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct While {
    #[serde(rename = "while")]
    condition: Value,
    max_iterations: u64,
    steps: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Set {
    set: BTreeMap<String, Value>,
}

fn decode<T: DeserializeOwned>(raw: Value) -> Result<T> {
    serde_json::from_value(raw).map_err(|err| err.to_string())
}

#[derive(Default)]
struct Compiler {
    library: Library,
    nodes: usize,
    executions: u64,
    variables: Map<String, Value>,
    control: bool,
}

#[derive(Clone, Copy)]
struct Context<'a> {
    app: &'a App,
    targets: &'a Targets,
    depth: usize,
    // Upper bound on visits, including every enclosing loop.
    visits: u64,
}

pub(super) fn expand(raw: Value, source: Option<&std::path::Path>) -> Result<Value> {
    let program: Program = decode(raw)?;
    if program.format != FORMAT || program.kind != "Recipe" {
        return Err(format!("expected kind Recipe and format {FORMAT}"));
    }
    let mut compiler = Compiler {
        library: Library::new(source)?,
        ..Default::default()
    };
    let mut scope = Scope::default();
    let mut inputs = Map::new();
    for name in program.inputs {
        scope.bind(&name, format!("inputs.{name}"), false)?;
        inputs.insert(name, json!({"schema":true,"required":true}));
    }
    let mut lowered = compiler.phases(program.phases, &program.targets, &mut scope, 0, 1)?;
    // Evaluate outputs in a normal, fallible runtime step. The reporting path
    // alone cannot propagate a missing property or a failed expression.
    let mut outputs = Map::new();
    let mut assign = Map::new();
    for (name, mut value) in program.outputs {
        identifier(&name)?;
        scope.resolve(&mut value)?;
        let slot = compiler.slot();
        outputs.insert(name, json!({"value":format!("${{{{ {slot} }}}}")}));
        assign.insert(slot, value);
    }
    if !assign.is_empty() {
        let id = compiler.id(Some("outputs"), 1)?;
        lowered.push(json!({"type":"set","id":id,"assign":assign}));
    }
    let mut workflow = json!({
        "apiVersion":aad_core::compiler::API_VERSION,"kind":"Workflow",
        "metadata":{"name": program.name,"version":"0.1.0"},
        "inputs": inputs,"outputs": outputs,"variables":compiler.variables,
        "steps":lowered,
        // The while engine checks its budget once more at loop exit.
        "budgets":{"max_duration":"10m","max_executed_steps":compiler.executions+1},
    });
    if compiler.control {
        workflow["requires"] = json!({"capabilities":[{"name":steps::PROVIDER,"version":">=0.1.0"}],
            "permissions":["desktop.input","desktop.observe"]});
    }
    aad_core::compile_descriptor(workflow.clone(), None)
        .map_err(|err| format!("{}: {}", err.message, json!(err.details["issues"])))?;
    Ok(workflow)
}

impl Compiler {
    fn slot(&mut self) -> String {
        let name = format!("v{}", self.variables.len() + 1);
        self.variables
            .insert(name.clone(), json!({"schema":true,"mutable":true}));
        format!("vars.{name}")
    }

    fn id(&mut self, label: Option<&str>, visits: u64) -> Result<String> {
        self.nodes += 1;
        self.executions = self.executions.saturating_add(visits);
        if self.nodes > MAX_NODES || self.executions > MAX_EXECUTIONS {
            return Err(format!(
                "recipe exceeds {MAX_NODES} compiled steps or {MAX_EXECUTIONS} bounded executions"
            ));
        }
        let mut id = format!("r{}", self.nodes);
        if let Some(label) = label {
            identifier(label)?;
            id.push('_');
            id.push_str(label);
        }
        Ok(id)
    }

    fn phases(
        &mut self,
        phases: Vec<Phase>,
        targets: &Targets,
        scope: &mut Scope,
        depth: usize,
        visits: u64,
    ) -> Result<Vec<Value>> {
        if phases.is_empty() {
            return Err("expected at least one phase".into());
        }
        let mut steps = Vec::new();
        for (i, phase) in phases.into_iter().enumerate() {
            let ctx = Context {
                app: &phase.app,
                targets,
                depth,
                visits,
            };
            steps.extend(
                self.body(phase.steps, scope, ctx)
                    .map_err(|err| format!("phase {}: {err}", i + 1))?,
            );
        }
        Ok(steps)
    }

    fn body(
        &mut self,
        statements: Vec<Value>,
        scope: &mut Scope,
        ctx: Context,
    ) -> Result<Vec<Value>> {
        if ctx.depth > MAX_DEPTH || statements.is_empty() {
            return Err(format!("expected nonempty steps within {MAX_DEPTH} levels"));
        }
        let mut steps = Vec::new();
        let mut labels = BTreeSet::new();
        for (i, mut raw) in statements.into_iter().enumerate() {
            let obj = raw.as_object_mut().ok_or("expected a recipe step object")?;
            let label: Option<String> = obj.remove("id").map(decode).transpose()?;
            if let Some(name) = &label {
                if !labels.insert(name.clone()) {
                    return Err(format!("duplicate step id {name}"));
                }
            }
            let id = self.id(label.as_deref(), ctx.visits)?;
            let step = self
                .statement(raw, &id, label.as_deref(), scope, ctx)
                .map_err(|err| format!("step {} ({id}): {err}", i + 1))?;
            steps.push(step);
        }
        Ok(steps)
    }

    fn statement(
        &mut self,
        mut raw: Value,
        id: &str,
        label: Option<&str>,
        scope: &mut Scope,
        ctx: Context,
    ) -> Result<Value> {
        let child = Context {
            depth: ctx.depth + 1,
            ..ctx
        };
        if raw.get("call").is_some() {
            return self.call(decode(raw)?, id, scope, child);
        }
        if raw.get("if").is_some() {
            let mut branch: Branch = decode(raw)?;
            scope.resolve(&mut branch.condition)?;
            let yes = self.body(branch.then, &mut scope.clone(), child)?;
            let no = if branch.otherwise.is_empty() {
                vec![]
            } else {
                self.body(branch.otherwise, &mut scope.clone(), child)?
            };
            return Ok(
                json!({"type":"if","id":id,"condition":branch.condition,"then":yes,"else":no}),
            );
        }
        if raw.get("foreach").is_some() {
            let mut each: Foreach = decode(raw)?;
            let visits = bounded_visits(ctx.visits, each.max_items)?;
            scope.resolve(&mut each.foreach)?;
            let mut local = scope.clone();
            let item = format!("{id}_item");
            local.loop_binding(&each.item, item.clone())?;
            let ctx = Context { visits, ..child };
            let mut step = json!({"type":"foreach","id":id,"items":each.foreach,"as":item,"max_items":each.max_items});
            if let Some(index) = each.index_as {
                if index == each.item {
                    return Err("as and index_as need distinct names".into());
                }
                let binding = format!("{id}_index");
                local.loop_binding(&index, binding.clone())?;
                step["index_as"] = json!(binding);
            }
            step["steps"] = json!(self.body(each.steps, &mut local, ctx)?);
            return Ok(step);
        }
        if raw.get("while").is_some() {
            let mut repeat: While = decode(raw)?;
            let visits = bounded_visits(ctx.visits, repeat.max_iterations)?;
            scope.resolve(&mut repeat.condition)?;
            let body = self.body(
                repeat.steps,
                &mut scope.clone(),
                Context { visits, ..child },
            )?;
            return Ok(json!({"type":"while","id":id,"condition":repeat.condition,
                "max_iterations":repeat.max_iterations,"timeout":"10m","steps":body}));
        }
        if raw.get("set").is_some() {
            let mut statement: Set = decode(raw)?;
            // All RHS expressions see the bindings before this set.
            for value in statement.set.values_mut() {
                scope.resolve(value)?;
            }
            let mut assign = Map::new();
            for (name, value) in statement.set {
                let slot = match scope.assignment(&name)? {
                    Some(bound) => bound.to_owned(),
                    None => {
                        let new = self.slot();
                        scope.bind(&name, new.clone(), true)?;
                        new
                    }
                };
                assign.insert(slot, value);
            }
            return Ok(json!({"type":"set","id":id,"assign":assign}));
        }

        if raw.get("app").is_some() || raw.get("input").is_some() {
            return Err(
                "use the phase app and reference declared recipe inputs in expressions".into(),
            );
        }
        raw["app"] = json!(ctx.app);
        raw["id"] = json!(id);
        resolve_targets(&mut raw, ctx.targets)?;
        if let Some(fields) = raw.get_mut("read").and_then(Value::as_array_mut) {
            for field in fields {
                resolve_targets(field, ctx.targets)?;
            }
        }
        scope.resolve(&mut raw)?;
        let step: Step = decode(raw)?;
        let mut doc = steps::workflow("recipe", &[step]).map_err(|err| err.message)?;
        self.control = true;
        if let Some(name) = label {
            scope.bind(name, format!("steps.{id}.output"), false)?;
        }
        Ok(doc["steps"][0].take())
    }

    fn call(&mut self, call: Call, id: &str, caller: &mut Scope, ctx: Context) -> Result<Value> {
        let spec = self.library.enter(call.call)?;
        let params: BTreeSet<_> = spec.parameters.iter().collect();
        if params.len() != spec.parameters.len() || params != call.with.keys().collect() {
            return Err(format!(
                "call arguments must match parameters {:?}",
                spec.parameters
            ));
        }
        let mut local = Scope::default();
        let mut args = Map::new();
        for (name, mut value) in call.with {
            caller.resolve(&mut value)?;
            let slot = self.slot();
            local.bind(&name, slot.clone(), false)?;
            args.insert(slot, value);
        }
        let mut body = Vec::new();
        if !args.is_empty() {
            let begin = self.id(Some("arguments"), ctx.visits)?;
            body.push(json!({"id":begin,"type":"set","assign":args}));
        }
        body.extend(self.phases(
            spec.phases.clone(),
            &spec.targets,
            &mut local,
            ctx.depth,
            ctx.visits,
        )?);
        let mut results = Map::new();
        let mut destinations = BTreeSet::new();
        for (key, name) in &call.save {
            if !spec.results.contains_key(key) {
                return Err(format!("unknown subrecipe result {key}"));
            }
            if !destinations.insert(name) {
                return Err(format!("duplicate result destination {name}"));
            }
        }
        for (key, mut value) in spec.results.clone() {
            identifier(&key)?;
            local.resolve(&mut value)?;
            let slot = if let Some(name) = call.save.get(&key) {
                match caller.assignment(name)? {
                    Some(bound) => bound.to_owned(),
                    None => {
                        let slot = self.slot();
                        caller.bind(name, slot.clone(), false)?;
                        slot
                    }
                }
            } else {
                self.slot()
            };
            results.insert(slot, value);
        }
        if !results.is_empty() {
            let end = self.id(Some("results"), ctx.visits)?;
            body.push(json!({"id":end,"type":"set","assign":results}));
        }
        self.library.leave();
        Ok(json!({"id":id,"type":"block","steps":body}))
    }
}

fn bounded_visits(visits: u64, limit: u64) -> Result<u64> {
    let total = visits.saturating_mul(limit);
    if !(1..=MAX_EXECUTIONS).contains(&total) {
        return Err(format!(
            "loop bounds must allow 1..{MAX_EXECUTIONS} total visits"
        ));
    }
    Ok(total)
}
