use aad_runtime::template;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub(super) struct Scope {
    names: BTreeMap<String, Binding>,
}

#[derive(Clone)]
struct Binding {
    expression: String,
    writable: bool,
}

pub(super) fn identifier(name: &str) -> Result<(), String> {
    if name.len() <= 48
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && !name.contains("__")
        && !keyword(name)
    {
        Ok(())
    } else {
        Err(format!("invalid recipe identifier {name:?}"))
    }
}

fn keyword(word: &str) -> bool {
    matches!(
        word,
        "True"
            | "False"
            | "None"
            | "and"
            | "or"
            | "not"
            | "in"
            | "is"
            | "if"
            | "else"
            | "for"
            | "lambda"
    )
}

impl Scope {
    pub fn bind(&mut self, name: &str, expression: String, writable: bool) -> Result<(), String> {
        identifier(name)?;
        if self.names.contains_key(name) {
            return Err(format!("name {name} is already bound"));
        }
        self.names.insert(
            name.into(),
            Binding {
                expression,
                writable,
            },
        );
        Ok(())
    }

    pub fn loop_binding(&mut self, name: &str, expression: String) -> Result<(), String> {
        self.names.remove(name);
        self.bind(name, expression, false)
    }

    pub fn assignment(&self, name: &str) -> Result<Option<&str>, String> {
        match self.names.get(name) {
            Some(binding) if binding.writable => Ok(Some(&binding.expression)),
            Some(_) => Err(format!("cannot assign to read-only name {name}")),
            None => Ok(None),
        }
    }

    pub fn resolve(&self, value: &mut Value) -> Result<(), String> {
        match value {
            Value::String(s) if s.contains("${{") => {
                let mut result = String::new();
                let mut cursor = 0;
                for (start, end, inner) in template::spans(s) {
                    result.push_str(&s[cursor..start]);
                    result.push_str("${{ ");
                    result.push_str(&self.expression(inner.trim())?);
                    result.push_str(" }}");
                    cursor = end;
                }
                if s[cursor..].contains("${{") {
                    return Err("unterminated recipe expression".into());
                }
                result.push_str(&s[cursor..]);
                *s = result;
            }
            Value::Object(map) => {
                for item in map.values_mut() {
                    self.resolve(item)?;
                }
            }
            Value::Array(array) => {
                for item in array {
                    self.resolve(item)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // The descriptor compiler parses the resulting expression. Here we only translate
    // free names to their lexical bindings; quoted keys and attribute names are data.
    fn expression(&self, source: &str) -> Result<String, String> {
        let mut chars = source.char_indices().peekable();
        let mut lowered = String::new();
        let mut after_dot = false;
        while let Some((start, ch)) = chars.next() {
            if ch == '\'' || ch == '"' {
                while let Some((_, next)) = chars.next() {
                    if next == '\\' {
                        chars.next();
                    } else if next == ch {
                        break;
                    }
                }
            } else if ch.is_ascii_digit()
                || (ch == '.' && chars.peek().is_some_and(|(_, c)| c.is_ascii_digit()))
            {
                // Keep exponent markers together with numeric literals, e.g. 1e-3.
                while chars
                    .next_if(|(_, c)| c.is_ascii_digit() || matches!(c, '_' | '.'))
                    .is_some()
                {}
                if chars.next_if(|(_, c)| matches!(c, 'e' | 'E')).is_some() {
                    chars.next_if(|(_, c)| matches!(c, '+' | '-'));
                    while chars
                        .next_if(|(_, c)| c.is_ascii_digit() || *c == '_')
                        .is_some()
                    {}
                }
            } else if ch.is_alphabetic() || ch == '_' {
                while chars
                    .next_if(|(_, c)| c.is_alphanumeric() || *c == '_')
                    .is_some()
                {}
                let word = &source[start..chars.peek().map_or(source.len(), |(i, _)| *i)];
                if !after_dot && !keyword(word) {
                    let binding = self
                        .names
                        .get(word)
                        .ok_or_else(|| format!("unknown recipe name {word}"))?;
                    lowered.push_str(&binding.expression);
                    after_dot = false;
                    continue;
                }
            }
            let end = chars.peek().map_or(source.len(), |(i, _)| *i);
            lowered.push_str(&source[start..end]);
            if !ch.is_whitespace() {
                after_dot = ch == '.';
            }
        }
        Ok(lowered)
    }
}
