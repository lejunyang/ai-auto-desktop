//! Server-owned secret bindings. Only their names cross the control protocol.
use crate::{error, Result};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct SecretBindings(Arc<BTreeMap<String, String>>);

impl fmt::Debug for SecretBindings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretBindings(<redacted>)")
    }
}

/// A public binding name, never an environment variable expression or a value.
pub fn valid_secret_ref(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

impl SecretBindings {
    /// Build a grant from values supplied by the trusted server operator.
    /// Values are not serializable and are omitted even from Debug output.
    pub fn new(bindings: impl IntoIterator<Item = (String, String)>) -> Result<Self> {
        let mut values = BTreeMap::new();
        for (name, value) in bindings {
            if !valid_secret_ref(&name)
                || value.chars().count() > 1024
                || value.contains(['\0', '\r', '\n'])
            {
                return Err(error("CONTROL.INVALID_REQUEST", "invalid secret binding"));
            }
            if values.insert(name, value).is_some() {
                return Err(error("CONTROL.INVALID_REQUEST", "duplicate secret binding"));
            }
        }
        Ok(Self(Arc::new(values)))
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    pub(crate) fn resolve(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_resolve_only_exact_names_and_never_debug_values() {
        let bindings = SecretBindings::new([("login".into(), "synthetic-secret".into())]).unwrap();
        assert_eq!(bindings.resolve("login"), Some("synthetic-secret"));
        assert_eq!(bindings.resolve("LOGIN"), None);
        assert_eq!(bindings.names(), ["login"]);
        assert_eq!(format!("{bindings:?}"), "SecretBindings(<redacted>)");
    }

    #[test]
    fn duplicate_or_unbounded_bindings_are_rejected_without_values_in_errors() {
        for values in [
            vec![
                ("login".into(), "secret".into()),
                ("login".into(), "other".into()),
            ],
            vec![("$(cat private)".into(), "secret".into())],
            vec![("login".into(), "x".repeat(1025))],
            vec![("login".into(), "secret\0suffix".into())],
            vec![("login".into(), "secret\nsubmit".into())],
        ] {
            let error = SecretBindings::new(values).unwrap_err();
            assert_eq!(error.effect, "not_applied");
            let output = serde_json::to_string(&error.to_json()).unwrap();
            assert!(!output.contains("suffix") && !output.contains("private"));
            assert!(output.len() < 1024);
        }
    }
}
