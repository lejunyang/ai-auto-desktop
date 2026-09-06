//! Template interpolation over the runtime scope.
//!
//! A string that is *exactly* one `${{ ... }}` template evaluates to the typed
//! value of that expression, so a number stays a number and an object stays an
//! object.  A string with surrounding text interpolates each template and
//! renders the result, which is how paths and messages are built.

use aad_core::expression::compile_expression;
use aad_core::AutomationError;
use serde_json::{Map, Value};

/// Locate `${{ ... }}` templates as `(start, end, inner)` byte spans.
pub fn spans(text: &str) -> Vec<(usize, usize, &str)> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index + 3 < bytes.len() {
        if bytes[index] == b'$' && bytes[index + 1] == b'{' && bytes[index + 2] == b'{' {
            if let Some(offset) = text[index + 3..].find("}}") {
                let start = index + 3;
                let end = start + offset;
                found.push((index, end + 2, &text[start..end]));
                index = end + 2;
                continue;
            }
        }
        index += 1;
    }
    found
}

/// The inner source when the whole string is one template, else `None`.
pub fn whole(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("${{")?.strip_suffix("}}")?;
    // "${{a}} ${{b}}" is interpolation, not a single typed template.
    if inner.contains("}}") {
        return None;
    }
    Some(inner.trim())
}

/// Render a value into the string form used inside interpolated text.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

fn evaluation_error(source: &str, reason: impl std::fmt::Display) -> AutomationError {
    AutomationError::new(
        "EXPRESSION.EVALUATION_FAILED",
        format!("could not evaluate {source:?}: {reason}"),
    )
    .with_category("expression")
    .with_effect("not_applied")
}

/// Evaluate one expression against the scope, returning its typed value.
pub fn evaluate(source: &str, scope: &Map<String, Value>) -> Result<Value, AutomationError> {
    compile_expression(source)
        .map_err(|error| evaluation_error(source, error))?
        .evaluate(scope)
        .map_err(|error| evaluation_error(source, error))
}

/// Resolve templates anywhere inside a JSON value.
pub fn resolve(value: &Value, scope: &Map<String, Value>) -> Result<Value, AutomationError> {
    match value {
        Value::String(text) => {
            if let Some(inner) = whole(text) {
                return evaluate(inner, scope);
            }
            let found = spans(text);
            if found.is_empty() {
                return Ok(Value::String(text.clone()));
            }
            let mut rendered = String::new();
            let mut cursor = 0usize;
            for (start, end, inner) in found {
                rendered.push_str(&text[cursor..start]);
                rendered.push_str(&render(&evaluate(inner.trim(), scope)?));
                cursor = end;
            }
            rendered.push_str(&text[cursor..]);
            Ok(Value::String(rendered))
        }
        Value::Array(items) => Ok(Value::Array(
            items
                .iter()
                .map(|item| resolve(item, scope))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Object(map) => {
            let mut resolved = Map::new();
            for (key, item) in map {
                resolved.insert(key.clone(), resolve(item, scope)?);
            }
            Ok(Value::Object(resolved))
        }
        other => Ok(other.clone()),
    }
}

/// Evaluate a condition field and coerce it to a boolean.
pub fn condition(value: &Value, scope: &Map<String, Value>) -> Result<bool, AutomationError> {
    let resolved = resolve(value, scope)?;
    Ok(match resolved {
        Value::Bool(flag) => flag,
        Value::Null => false,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn a_whole_template_preserves_the_value_type() {
        let scope = scope(json!({"count": 3, "flag": true, "data": {"a": 1}}));

        assert_eq!(resolve(&json!("${{ count }}"), &scope).unwrap(), json!(3));
        assert_eq!(resolve(&json!("${{ flag }}"), &scope).unwrap(), json!(true));
        assert_eq!(
            resolve(&json!("${{ data }}"), &scope).unwrap(),
            json!({"a": 1})
        );
    }

    #[test]
    fn an_interpolated_template_renders_to_a_string() {
        let scope = scope(json!({"name": "Ada", "count": 3}));

        assert_eq!(
            resolve(&json!("hello ${{ name }}, you have ${{ count }}"), &scope).unwrap(),
            json!("hello Ada, you have 3")
        );
    }

    #[test]
    fn python_style_literals_render_for_humans() {
        let scope = scope(json!({"nothing": null, "yes": true}));

        assert_eq!(
            resolve(&json!("<${{ nothing }}|${{ yes }}>"), &scope).unwrap(),
            json!("<None|True>")
        );
    }

    #[test]
    fn templates_are_resolved_at_any_depth() {
        let scope = scope(json!({"value": 7}));
        let resolved = resolve(
            &json!({"outer": [{"inner": "${{ value }}"}, "literal"]}),
            &scope,
        )
        .unwrap();

        assert_eq!(resolved, json!({"outer": [{"inner": 7}, "literal"]}));
    }

    #[test]
    fn a_string_without_templates_is_returned_unchanged() {
        let scope = scope(json!({}));
        assert_eq!(
            resolve(&json!("plain text"), &scope).unwrap(),
            json!("plain text")
        );
    }

    #[test]
    fn an_unknown_variable_is_a_structured_expression_error() {
        let error = resolve(&json!("${{ missing }}"), &scope(json!({}))).unwrap_err();

        assert_eq!(error.code, "EXPRESSION.EVALUATION_FAILED");
        assert_eq!(error.category, "expression");
        // Evaluation happens before any side effect, so nothing was applied.
        assert_eq!(error.effect, "not_applied");
    }

    #[test]
    fn conditions_follow_python_truthiness() {
        let scope = scope(json!({"empty": [], "items": [1], "zero": 0, "text": ""}));

        assert!(!condition(&json!("${{ empty }}"), &scope).unwrap());
        assert!(condition(&json!("${{ items }}"), &scope).unwrap());
        assert!(!condition(&json!("${{ zero }}"), &scope).unwrap());
        assert!(!condition(&json!("${{ text }}"), &scope).unwrap());
    }

    #[test]
    fn whole_distinguishes_a_single_template_from_two() {
        assert_eq!(whole("${{ a }}"), Some("a"));
        assert_eq!(whole("${{ a }} ${{ b }}"), None);
        assert_eq!(whole("x ${{ a }}"), None);
    }
}
