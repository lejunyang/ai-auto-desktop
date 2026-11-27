use crate::{error, Context, Expect, Locator, Observation, Result, Session, MAX_FIND_LOCATORS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReadField {
    #[serde(rename = "as")]
    pub alias: String,
    pub locator: Locator,
    pub property: Property,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Property {
    Name,
    Value,
    Present,
    Checked,
    Enabled,
    Selected,
}

impl Property {
    const ALL: [Self; 6] = [
        Self::Name,
        Self::Value,
        Self::Present,
        Self::Checked,
        Self::Enabled,
        Self::Selected,
    ];

    fn read(self, found: &Value) -> std::result::Result<&Value, PropertyError> {
        let node = &found["node"];
        let value = match self {
            Self::Present => &found["found"],
            Self::Name => &node["name"],
            Self::Value if node["states"]["protected"] == true => {
                return Err(PropertyError::Protected);
            }
            Self::Value if node["value_truncated"] == true => {
                return Err(PropertyError::Truncated);
            }
            Self::Value => &node["value"],
            Self::Checked => &node["states"]["checked"],
            Self::Enabled => &node["states"]["enabled"],
            Self::Selected => &node["states"]["selected"],
        };
        match value {
            Value::String(text) if text.len() > 4096 => Err(PropertyError::TooLarge),
            Value::String(_) | Value::Bool(_) | Value::Number(_) => Ok(value),
            _ => Err(PropertyError::Unavailable),
        }
    }
}

enum PropertyError {
    Protected,
    Truncated,
    TooLarge,
    Unavailable,
}

pub(crate) fn validate(fields: &[ReadField]) -> Result<()> {
    let mut names = BTreeSet::new();
    if !(1..=MAX_FIND_LOCATORS).contains(&fields.len()) {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "read requires 1..32 fields",
        ));
    }
    for field in fields {
        if field.alias.len() > 64
            || !field
                .alias
                .starts_with(|first: char| first.is_ascii_lowercase())
            || !field
                .alias
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || !names.insert(&field.alias)
            || field.locator.is_empty()
        {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "each field needs a distinct lowercase identifier and a locator",
            )
            .with_detail("field", json!(field.alias)));
        }
    }
    Ok(())
}

pub(super) fn project(
    session: &Session,
    id: &str,
    observation: &Observation,
    fields: &[ReadField],
    context: &Context,
) -> Result<Value> {
    let mut values = Map::new();
    for field in fields {
        let present = field.property == Property::Present;
        let found = session
            .find_in(
                id,
                observation,
                &field.locator,
                if present {
                    Expect::Optional
                } else {
                    Expect::One
                },
                context,
            )
            .map_err(|err| err.with_detail("field", json!(field.alias)))?;
        let value = field.property.read(&found).map_err(|failure| {
            let alias = &field.alias;
            let err = match failure {
                PropertyError::Protected => {
                    error("POLICY.DENIED", format!("field {alias} is protected"))
                }
                PropertyError::Truncated => error(
                    "CONTROL.FIELD_TOO_LARGE",
                    format!("field {alias} was truncated in the observation"),
                ),
                PropertyError::TooLarge => error(
                    "CONTROL.FIELD_TOO_LARGE",
                    format!("field {alias} exceeds 4096 bytes"),
                ),
                PropertyError::Unavailable => {
                    let mut err = error(
                        "CONTROL.FIELD_UNAVAILABLE",
                        format!("field {alias} does not expose {:?}", field.property),
                    )
                    .with_detail("property", json!(field.property));
                    if found["found"] == true {
                        let available: Vec<_> = Property::ALL
                            .into_iter()
                            .filter(|property| property.read(&found).is_ok())
                            .collect();
                        err = err.with_detail("available_properties", json!(available));
                    }
                    err
                }
            };
            err.with_detail("field", json!(alias))
        })?;
        values.insert(field.alias.clone(), value.clone());
    }
    Ok(json!({"values":values}))
}
