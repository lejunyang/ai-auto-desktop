//! Portable canvas read options and graph-relative object selectors.
use crate::{error, Result};
use serde::{Deserialize, Serialize};

/// A visible crop or object box in fractions of the full canvas.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub(crate) struct Region {
    pub fx: f64,
    pub fy: f64,
    pub fw: f64,
    pub fh: f64,
}

impl Region {
    pub(crate) fn read(raw: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(raw.clone())
            .map_err(|_| error("CONTROL.BAD_PROVIDER_OUTPUT", "missing canvas region"))
    }

    pub(crate) fn project(self, b: Self) -> Self {
        Self {
            fx: self.fx + b.fx * self.fw,
            fy: self.fy + b.fy * self.fh,
            fw: b.fw * self.fw,
            fh: b.fh * self.fh,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Auto,
    Editor,
    Flowchart,
    Vision,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub source: Source,
    pub description: Option<String>,
    pub language: Option<String>,
    pub preview: bool,
}

impl Options {
    pub(crate) fn source(&self) -> Result<Source> {
        let source = match (self.source, self.description.as_deref()) {
            (Source::Auto | Source::Vision, Some(description))
                if !description.trim().is_empty() && description.len() <= 1024 =>
            {
                Source::Vision
            }
            (Source::Vision, _) | (_, Some(_)) => {
                return Err(error(
                    "CONTROL.INVALID_REQUEST",
                    "vision requires an object description of 1..1024 bytes",
                ))
            }
            (source, None) => source,
        };
        if self.language.as_ref().is_some_and(|s| {
            !matches!(source, Source::Auto | Source::Flowchart)
                || s.is_empty()
                || s.len() > 35
                || !s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        }) {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "choose a BCP-47 OCR language for flowchart recognition",
            ));
        }
        Ok(source)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Query {
    pub id: Option<String>,
    pub label: Option<String>,
    pub kind: Option<String>,
    /// Exact label of an immediate predecessor connected by a confirmed arrow.
    pub after: Option<String>,
    /// Exact label of an immediate successor connected by a confirmed arrow.
    pub before: Option<String>,
    pub region: Option<Quadrant>,
    pub min_confidence: Option<f64>,
    /// Centre of a previously seen crop. Used to scroll and search once more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_hint: Option<crate::Fraction>,
    /// Pin a candidate selected from a particular read.
    pub digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Quadrant {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Quadrant {
    pub(crate) fn contains(self, bounds: &serde_json::Value) -> bool {
        let centre = |s: &str, n: &str| Some(bounds[s].as_f64()? + bounds[n].as_f64()? / 2.0);
        let (Some(x), Some(y)) = (centre("fx", "fw"), centre("fy", "fh")) else {
            return false;
        };
        let right = matches!(self, Self::TopRight | Self::BottomRight);
        let bottom = matches!(self, Self::BottomLeft | Self::BottomRight);
        (x >= 0.5) == right && (y >= 0.5) == bottom
    }
}
