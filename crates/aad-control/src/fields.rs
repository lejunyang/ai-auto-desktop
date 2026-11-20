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
            ));
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
        let node = &found["node"];
        let value = match field.property {
            Property::Present => &found["found"],
            Property::Name => &node["name"],
            Property::Value if node["states"]["protected"] == true => {
                return Err(error(
                    "POLICY.DENIED",
                    format!("field {} is protected", field.alias),
                ));
            }
            Property::Value => &node["value"],
            Property::Checked => &node["states"]["checked"],
            Property::Enabled => &node["states"]["enabled"],
            Property::Selected => &node["states"]["selected"],
        };
        if !matches!(value, Value::String(_) | Value::Bool(_) | Value::Number(_)) {
            return Err(error(
                "CONTROL.FIELD_UNAVAILABLE",
                format!("field {} does not expose {:?}", field.alias, field.property),
            ));
        }
        if value.as_str().is_some_and(|text| text.len() > 4096) {
            return Err(error(
                "CONTROL.FIELD_TOO_LARGE",
                format!("field {} exceeds 4096 bytes", field.alias),
            ));
        }
        values.insert(field.alias.clone(), value.clone());
    }
    Ok(json!({"values":values}))
}
