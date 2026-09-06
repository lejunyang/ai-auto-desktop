//! SemVer parsing and constraint matching for `requires`.
//!
//! A direct port of the Python `_semver` / `_compare_semver` /
//! `_version_matches` trio, because the two implementations gate the same
//! descriptors and a disagreement would mean a workflow that one runtime accepts
//! and the other refuses.
//!
//! Deliberately hand-rolled rather than taking a `semver` dependency: the Python
//! behaviour being matched is not quite the crate's, in particular the rule that
//! a range does not implicitly opt into prerelease versions unless a comparator
//! names a prerelease on the same major/minor/patch.

/// A parsed semantic version. `prerelease` is `None` for a release version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<Vec<Identifier>>,
}

/// A dot-separated prerelease identifier. Numeric ones order below alphanumeric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identifier {
    Numeric(u64),
    Alphanumeric(String),
}

/// Parse a strict `MAJOR.MINOR.PATCH[-prerelease][+build]`.
///
/// Build metadata is accepted and discarded: SemVer says it takes no part in
/// precedence. Leading zeroes are rejected in both the core numbers and numeric
/// prerelease identifiers, matching the Python regex.
pub fn parse(value: &str) -> Option<Version> {
    let core_end = value.find(['-', '+']).unwrap_or(value.len());
    let (core, rest) = value.split_at(core_end);

    let mut numbers = core.split('.');
    let major = parse_number(numbers.next()?)?;
    let minor = parse_number(numbers.next()?)?;
    let patch = parse_number(numbers.next()?)?;
    if numbers.next().is_some() {
        return None;
    }

    // `-` introduces the prerelease, `+` build metadata; a prerelease may itself
    // be followed by build metadata.
    let prerelease_text = match rest.strip_prefix('-') {
        Some(tail) => Some(tail.split('+').next().unwrap_or("")),
        None => {
            if !rest.is_empty() && !rest.starts_with('+') {
                return None;
            }
            if let Some(build) = rest.strip_prefix('+') {
                if !is_dotted_alphanumeric(build) {
                    return None;
                }
            }
            None
        }
    };

    let prerelease = match prerelease_text {
        None => None,
        Some(text) => {
            if !is_dotted_alphanumeric(text) {
                return None;
            }
            let mut identifiers = Vec::new();
            for part in text.split('.') {
                if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
                    // A numeric identifier must not carry a leading zero.
                    if part.len() > 1 && part.starts_with('0') {
                        return None;
                    }
                    identifiers.push(Identifier::Numeric(part.parse().ok()?));
                } else {
                    identifiers.push(Identifier::Alphanumeric(part.to_string()));
                }
            }
            Some(identifiers)
        }
    };

    Some(Version {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn parse_number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

/// Whether every dot-separated part is a non-empty run of `[0-9A-Za-z-]`.
fn is_dotted_alphanumeric(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// SemVer precedence. A release outranks a prerelease of the same core version.
pub fn compare(left: &Version, right: &Version) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let core = (left.major, left.minor, left.patch).cmp(&(right.major, right.minor, right.patch));
    if core != Ordering::Equal {
        return core;
    }

    match (&left.prerelease, &right.prerelease) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left_pre), Some(right_pre)) => {
            for (left_item, right_item) in left_pre.iter().zip(right_pre.iter()) {
                let ordering = match (left_item, right_item) {
                    (Identifier::Numeric(a), Identifier::Numeric(b)) => a.cmp(b),
                    // Numeric identifiers always have lower precedence.
                    (Identifier::Numeric(_), Identifier::Alphanumeric(_)) => Ordering::Less,
                    (Identifier::Alphanumeric(_), Identifier::Numeric(_)) => Ordering::Greater,
                    (Identifier::Alphanumeric(a), Identifier::Alphanumeric(b)) => a.cmp(b),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            left_pre.len().cmp(&right_pre.len())
        }
    }
}

/// Whether `version` satisfies `constraint`.
///
/// The constraint is whitespace-separated comparators, all of which must hold.
/// Each is `>=`, `<=`, `>`, `<` or `=` (the default) followed by a version, or a
/// caret range. An unparsable version or comparator makes the whole constraint
/// unsatisfied rather than an error, matching the Python `except` path: a
/// malformed `requires` must not be read as "no requirement".
pub fn matches(version: &str, constraint: &str) -> bool {
    let Some(actual) = parse(version) else {
        return false;
    };

    let mut comparators: Vec<(&str, Version)> = Vec::new();
    for token in constraint.split_whitespace() {
        if let Some(floor_text) = token.strip_prefix('^') {
            let Some(floor) = parse(floor_text) else {
                return false;
            };
            // Caret allows changes that do not modify the left-most non-zero
            // number, so below 1.0.0 it tightens to the minor and then the patch.
            let ceiling = if floor.major != 0 {
                Version {
                    major: floor.major + 1,
                    minor: 0,
                    patch: 0,
                    prerelease: None,
                }
            } else if floor.minor != 0 {
                Version {
                    major: 0,
                    minor: floor.minor + 1,
                    patch: 0,
                    prerelease: None,
                }
            } else {
                Version {
                    major: 0,
                    minor: 0,
                    patch: floor.patch + 1,
                    prerelease: None,
                }
            };
            comparators.push((">=", floor));
            comparators.push(("<", ceiling));
            continue;
        }

        let (operator, wanted_text) = if let Some(rest) = token.strip_prefix(">=") {
            (">=", rest)
        } else if let Some(rest) = token.strip_prefix("<=") {
            ("<=", rest)
        } else if let Some(rest) = token.strip_prefix('>') {
            (">", rest)
        } else if let Some(rest) = token.strip_prefix('<') {
            ("<", rest)
        } else if let Some(rest) = token.strip_prefix('=') {
            ("=", rest)
        } else {
            ("=", token)
        };
        let Some(wanted) = parse(wanted_text) else {
            return false;
        };
        comparators.push((operator, wanted));
    }

    if comparators.is_empty() {
        return false;
    }

    // A range does not implicitly opt into prereleases: a prerelease only
    // satisfies a constraint that names a prerelease on the same core version.
    if actual.prerelease.is_some()
        && !comparators.iter().any(|(_, wanted)| {
            wanted.prerelease.is_some()
                && (wanted.major, wanted.minor, wanted.patch)
                    == (actual.major, actual.minor, actual.patch)
        })
    {
        return false;
    }

    use std::cmp::Ordering;
    comparators.iter().all(|(operator, wanted)| {
        let ordering = compare(&actual, wanted);
        match *operator {
            ">=" => ordering != Ordering::Less,
            "<=" => ordering != Ordering::Greater,
            ">" => ordering == Ordering::Greater,
            "<" => ordering == Ordering::Less,
            _ => ordering == Ordering::Equal,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_release_version_parses() {
        let version = parse("1.2.3").expect("1.2.3 is valid");
        assert_eq!((version.major, version.minor, version.patch), (1, 2, 3));
        assert!(version.prerelease.is_none());
    }

    #[test]
    fn build_metadata_is_accepted_and_ignored() {
        // SemVer excludes build metadata from precedence, so the two compare
        // equal rather than one outranking the other.
        let plain = parse("1.2.3").unwrap();
        let built = parse("1.2.3+build.5").unwrap();
        assert_eq!(compare(&plain, &built), std::cmp::Ordering::Equal);
    }

    #[test]
    fn malformed_versions_are_refused() {
        for text in [
            "1.2", "1.2.3.4", "01.2.3", "1.02.3", "v1.2.3", "1.2.x", "", "1.2.3-",
            // A numeric prerelease identifier may not carry a leading zero.
            "1.2.3-01",
        ] {
            assert!(parse(text).is_none(), "{text:?} must not parse");
        }
    }

    #[test]
    fn a_release_outranks_its_own_prerelease() {
        let release = parse("1.0.0").unwrap();
        let prerelease = parse("1.0.0-rc.1").unwrap();
        assert_eq!(compare(&release, &prerelease), std::cmp::Ordering::Greater);
    }

    #[test]
    fn prerelease_identifiers_order_by_semver_rules() {
        // Numeric below alphanumeric, and a shorter prerelease below a longer one
        // that shares its prefix.
        let cases = [
            ("1.0.0-alpha", "1.0.0-alpha.1", std::cmp::Ordering::Less),
            (
                "1.0.0-alpha.1",
                "1.0.0-alpha.beta",
                std::cmp::Ordering::Less,
            ),
            ("1.0.0-beta.2", "1.0.0-beta.11", std::cmp::Ordering::Less),
            ("1.0.0-rc.1", "1.0.0-rc.1", std::cmp::Ordering::Equal),
        ];
        for (left, right, expected) in cases {
            assert_eq!(
                compare(&parse(left).unwrap(), &parse(right).unwrap()),
                expected,
                "{left} vs {right}"
            );
        }
    }

    #[test]
    fn comparators_are_all_required_to_hold() {
        assert!(matches("1.2.3", ">=1.0.0 <2.0.0"));
        assert!(!matches("2.0.0", ">=1.0.0 <2.0.0"));
        assert!(matches("1.2.3", "1.2.3"));
        assert!(matches("1.2.3", "=1.2.3"));
        assert!(!matches("1.2.4", "=1.2.3"));
        assert!(matches("1.2.3", ">1.2.2"));
        assert!(!matches("1.2.3", ">1.2.3"));
        assert!(matches("1.2.3", "<=1.2.3"));
    }

    #[test]
    fn a_caret_range_pins_the_leftmost_nonzero_number() {
        assert!(matches("1.5.0", "^1.2.0"));
        assert!(!matches("2.0.0", "^1.2.0"));
        // Below 1.0.0 the minor acts as the major.
        assert!(matches("0.2.9", "^0.2.0"));
        assert!(!matches("0.3.0", "^0.2.0"));
        // And below 0.1.0 the patch does.
        assert!(matches("0.0.1", "^0.0.1"));
        assert!(!matches("0.0.2", "^0.0.1"));
    }

    #[test]
    fn the_shipped_runtime_version_satisfies_the_tracked_examples() {
        // The guard that would have caught the 0.1.0 -> 0.0.1 change leaving the
        // examples behind.
        assert!(matches(crate::engine::RUNTIME_VERSION, ">=0.0.1"));
    }

    #[test]
    fn a_prerelease_needs_a_comparator_that_names_one() {
        // Otherwise `>=1.0.0` would quietly accept `2.0.0-rc.1`, opting a
        // workflow into a prerelease runtime it never asked for.
        assert!(!matches("2.0.0-rc.1", ">=1.0.0"));
        assert!(matches("2.0.0-rc.1", ">=2.0.0-rc.1"));
        assert!(!matches("2.0.0-rc.1", ">=2.0.0-rc.2"));
    }

    #[test]
    fn a_malformed_constraint_is_unsatisfied_rather_than_ignored() {
        // Reading a broken `requires` as "no requirement" would silently drop
        // the gate the descriptor asked for.
        for constraint in ["", "   ", "not-a-version", ">=", "^", ">=1.0.0 garbage"] {
            assert!(
                !matches("1.2.3", constraint),
                "{constraint:?} must not be satisfied"
            );
        }
    }
}
