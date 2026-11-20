//! Bounded traversal of a single observed HTML table.
use super::*;
use serde::{Deserialize, Serialize};

pub(crate) fn validate_columns(columns: &[String]) -> Result<()> {
    if !(1..=32).contains(&columns.len())
        || columns
            .iter()
            .any(|column| column.trim().is_empty() || column.len() > 256)
        || columns.iter().collect::<BTreeSet<_>>().len() != columns.len()
    {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "choose 1..32 distinct column names",
        ));
    }
    Ok(())
}

pub(crate) fn project(content: &Value, node_id: &Value, columns: &[String]) -> Result<Value> {
    let table = &content["tables"][0];
    if &table["node_id"] != node_id || table["simple_header"] != true {
        return Err(error(
            "CONTROL.TABLE_HEADER",
            "expected a table with one visible header row and no spanning cells",
        ));
    }
    let rows = table["rows"]
        .as_array()
        .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "missing table rows"))?;
    let header = rows
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| error("CONTROL.TABLE_HEADER", "missing table headers"))?;
    let mut indexes = Vec::new();
    for column in columns {
        let matches: Vec<_> = header
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.as_str() == Some(column))
            .collect();
        if matches.len() != 1 {
            return Err(error(
                "CONTROL.TABLE_HEADER",
                format!(
                    "column {column:?} occurs {} times in the returned header",
                    matches.len()
                ),
            ));
        }
        indexes.push(matches[0].0);
    }
    let records: Vec<_> = rows
        .iter()
        .skip(1)
        .map(|row| {
            columns
                .iter()
                .zip(&indexes)
                .map(|(name, index)| {
                    (
                        name.clone(),
                        row.get(*index).cloned().unwrap_or(Value::Null),
                    )
                })
                .collect::<serde_json::Map<_, _>>()
        })
        .collect();
    Ok(
        json!({"scope":"rendered_rows","records":records,"visible_data_rows":table["row_count"].as_u64().unwrap_or(0).saturating_sub(1),
            "truncated":table["truncated"],"truncation_reasons":table["truncation_reasons"]}),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    pub table: Locator,
    pub columns: Vec<String>,
    pub row_key: String,
    pub traversal: Traversal,
    #[serde(default)]
    pub expected_rows: Option<usize>,
    #[serde(default = "default_pages")]
    pub max_pages: usize,
    #[serde(default = "default_rows")]
    pub max_rows: usize,
}

fn default_pages() -> usize {
    64
}
fn default_rows() -> usize {
    10_000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Traversal {
    Pagination {
        next: Locator,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first: Option<Box<Locator>>,
    },
    VirtualScroll {
        container: Locator,
    },
}

impl Collection {
    fn control(&self) -> &Locator {
        match &self.traversal {
            Traversal::Pagination { next, .. } => next,
            Traversal::VirtualScroll { container } => container,
        }
    }

    fn first(&self) -> Option<&Locator> {
        match &self.traversal {
            Traversal::Pagination { first, .. } => first.as_deref(),
            Traversal::VirtualScroll { .. } => None,
        }
    }

    /// Check the collection contract before a workflow performs any actions.
    pub fn validate(&self) -> Result<()> {
        validate_columns(&self.columns)?;
        if !self.columns.contains(&self.row_key) {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "row_key must name one requested column",
            ));
        }
        if !(1..=256).contains(&self.max_pages) || !(1..=10_000).contains(&self.max_rows) {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "max_pages must be 1..256 and max_rows 1..10000",
            ));
        }
        for locator in [&self.table, self.control()]
            .into_iter()
            .chain(self.first())
        {
            if locator.is_empty() || contains_node_id(&serde_json::to_value(locator).unwrap()) {
                return Err(error(
                    "CONTROL.INVALID_REQUEST",
                    "table traversal requires reusable locators without observation-local node IDs",
                ));
            }
        }
        Ok(())
    }
}

fn contains_node_id(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields.contains_key("node_id") || fields.values().any(contains_node_id)
        }
        Value::Array(values) => values.iter().any(contains_node_id),
        _ => false,
    }
}

enum TableMove {
    First,
    Next,
    Scroll,
}

struct Page {
    records: Vec<Value>,
    limits: Vec<String>,
    control: Value,
    first: Value,
    viewport: Value,
    declared: Option<usize>,
}

impl Page {
    fn same_window(&self, other: &Page) -> bool {
        self.records == other.records
            && self.viewport == other.viewport
            && self.limits == other.limits
            && self.declared == other.declared
            && self.first["node"]["states"]["enabled"] == other.first["node"]["states"]["enabled"]
            && self.control["found"] == other.control["found"]
            && self.control["node"]["states"]["enabled"]
                == other.control["node"]["states"]["enabled"]
    }
}

#[derive(Default)]
struct Rows {
    records: Vec<Value>,
    indexes: BTreeMap<String, usize>,
    bytes: usize,
}

impl Rows {
    fn merge(&mut self, page: &Page, spec: &Collection) -> Result<Option<&'static str>> {
        let mut current = BTreeSet::new();
        for row in &page.records {
            let key = row[&spec.row_key]
                .as_str()
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| error("CONTROL.TABLE_KEY", "missing or empty stable row key"))?;
            if !current.insert(key) {
                return Err(error(
                    "CONTROL.TABLE_KEY",
                    "duplicate stable row key in one window",
                ));
            }
            if let Some(index) = self.indexes.get(key) {
                if &self.records[*index] != row {
                    return Err(error(
                        "CONTROL.TABLE_CHANGED",
                        "a previously collected row changed",
                    ));
                }
                continue;
            }
            if self.records.len() == spec.max_rows {
                return Ok(Some("row_limit"));
            }
            self.bytes += serde_json::to_vec(row).unwrap().len() + 1;
            if self.bytes > 2 * 1024 * 1024 {
                return Ok(Some("output_byte_limit"));
            }
            self.indexes.insert(key.to_owned(), self.records.len());
            self.records.push(row.clone());
        }
        Ok(None)
    }
}

impl Session {
    fn table_page(&self, resource: &str, spec: &Collection, context: &Context) -> Result<Page> {
        let (id, observation) = self.observe_resource(resource, MAX_NODES, context)?;
        if observation.provider != browser::PROVIDER {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "table collection requires Chromium",
            ));
        }
        let table = self.find_in(&id, &observation, &spec.table, Expect::One, context)?;
        if table["node"]["tag"] != "table" {
            return Err(error(
                "CONTROL.TABLE_HEADER",
                "table locator must identify an HTML table",
            ));
        }
        let scrolling = matches!(spec.traversal, Traversal::VirtualScroll { .. });
        let control = self.find_in(
            &id,
            &observation,
            spec.control(),
            if scrolling {
                Expect::One
            } else {
                Expect::Optional
            },
            context,
        )?;
        let first = match spec.first() {
            Some(locator) => self.find_in(&id, &observation, locator, Expect::Optional, context)?,
            None => Value::Null,
        };
        if first["found"] == true && first["target"] == control["target"] {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "First and Next must identify distinct controls",
            ));
        }
        let table_id = &table["target"]["node_id"];
        let content = self.backends[&observation.provider].table_content(
            &observation.raw,
            table_id.as_str().unwrap(),
            if scrolling {
                control["target"]["node_id"].as_str()
            } else {
                None
            },
            context.remaining()?,
        )?;
        let projected = project(&content, table_id, &spec.columns)?;
        let limits = serde_json::from_value::<Vec<String>>(projected["truncation_reasons"].clone())
            .map_err(|_| error("CONTROL.BAD_PROVIDER_OUTPUT", "invalid table limits"))?;
        Ok(Page {
            records: projected["records"].as_array().unwrap().clone(),
            limits,
            first,
            control,
            viewport: content["viewport"].clone(),
            declared: content["tables"][0]["declared_data_rows"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok()),
        })
    }

    fn advance_table(
        &self,
        resource: &str,
        spec: &Collection,
        page: &Page,
        movement: TableMove,
        context: &Context,
    ) -> Result<Option<Page>> {
        let control = if matches!(movement, TableMove::First) {
            &page.first
        } else {
            &page.control
        };
        let target = serde_json::from_value(control["target"].clone()).map_err(|_| {
            error(
                "CONTROL.BAD_PROVIDER_OUTPUT",
                "invalid traversal control target",
            )
        })?;
        let action = if matches!(movement, TableMove::Scroll) {
            Action::Scroll {
                direction: Direction::Down,
                notches: Some(1),
                fraction: None,
            }
        } else {
            Action::click()
        };
        self.dispatch(
            &Operation::Act {
                target,
                action,
                verify: None,
            },
            context,
        )?;

        // Wait for a changed row window (or a moved virtual viewport).
        // Never submit a second click to make a delayed first click "work".
        let started = Instant::now();
        let mut candidate: Option<Page> = None;
        loop {
            std::thread::sleep(context.remaining()?.min(Duration::from_millis(80)));
            let elapsed = started.elapsed();
            let fresh = match self.table_page(resource, spec, context) {
                Ok(fresh) => fresh,
                Err(e)
                    if elapsed < Duration::from_secs(2)
                        && matches!(
                            e.code.as_str(),
                            "CONTROL.STALE_TARGET" | "CONTROL.NOT_FOUND" | "CONTROL.TABLE_HEADER"
                        ) =>
                {
                    candidate = None;
                    continue;
                }
                Err(e) => return Err(e),
            };
            // Applications may clear rows and disable Next while loading.
            // A compositor scroll can likewise precede the recycled DOM.
            let rows_changed = !fresh.records.is_empty() && fresh.records != page.records;
            let moved = matches!(movement, TableMove::Scroll) && fresh.viewport != page.viewport;
            let settle = if fresh.viewport["at_bottom"] == true {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(500)
            };
            let first_ready = !matches!(movement, TableMove::First)
                || fresh.first["node"]["states"]["enabled"] == false;
            if first_ready
                && (rows_changed || (moved && elapsed >= settle))
                && candidate
                    .as_ref()
                    .is_some_and(|last| fresh.same_window(last))
            {
                return Ok(Some(fresh));
            }
            if elapsed >= Duration::from_secs(2) {
                return Ok(None);
            }
            candidate = Some(fresh);
        }
    }

    pub(crate) fn collect_table(
        &self,
        resource: &str,
        spec: &Collection,
        context: &Context,
    ) -> Result<Value> {
        spec.validate()?;
        if !self.policy.allow_input {
            return Err(error(
                "POLICY.DENIED",
                "table traversal requires input permission",
            ));
        }
        let scrolling = matches!(spec.traversal, Traversal::VirtualScroll { .. });
        let mut page = self.table_page(resource, spec, context)?;
        let mut rows = Rows::default();
        let mut pages = 0;
        let mut actions = 0;
        // A paginated site's aria-rowcount may describe just this HTML table
        // on the last page. Only a caller-supplied business total proves the
        // entire paginated dataset by count alone.
        let declared = page.declared;
        let expected = spec
            .expected_rows
            .or(if scrolling { declared } else { None });
        let mut first_verified = page.first["node"]["states"]["enabled"] == false;
        let mut seek_first = page.first["node"]["states"]["enabled"] == true;
        let at_top = scrolling && page.viewport["at_top"] == true;
        let mut unrendered = false;
        let (complete, reason) = loop {
            if seek_first {
                actions += 1;
                let Some(first) =
                    self.advance_table(resource, spec, &page, TableMove::First, context)?
                else {
                    break (false, "first_unconfirmed".into());
                };
                page = first;
                first_verified = true;
                seek_first = false;
            }
            pages += 1;
            if let Some(limit) = page
                .limits
                .iter()
                .find(|limit| limit.as_str() != "rows_not_rendered")
            {
                break (false, limit.clone());
            }
            if scrolling
                && (page.declared != declared
                    || (spec.expected_rows.is_some()
                        && declared.is_some()
                        && spec.expected_rows != declared))
            {
                break (false, "row_count_changed".into());
            }
            unrendered |= page
                .limits
                .iter()
                .any(|reason| reason == "rows_not_rendered");
            let before = rows.records.len();
            if let Some(reason) = rows.merge(&page, spec)? {
                break (false, reason.into());
            }
            if expected.is_some_and(|n| rows.records.len() > n) {
                break (false, "row_count_mismatch".into());
            }
            if scrolling && pages == 1 && !at_top {
                break (false, "start_not_at_top".into());
            }
            let terminal = if scrolling {
                page.viewport["at_bottom"] == true
            } else {
                page.control["node"]["states"]["enabled"] == false
            };
            if terminal {
                if expected.is_some_and(|n| rows.records.len() != n) {
                    break (false, "row_count_mismatch".into());
                }
                if expected.is_none() && !scrolling && !first_verified {
                    break (false, "start_unverified".into());
                }
                if expected.is_none() && (scrolling || unrendered) {
                    break (false, "total_unverified".into());
                }
                break (
                    true,
                    if scrolling {
                        "bottom_and_row_count"
                    } else {
                        "next_disabled"
                    }
                    .into(),
                );
            }
            if page.control["found"] != true {
                let verified = expected == Some(rows.records.len());
                break (
                    verified,
                    if verified {
                        "next_absent_and_row_count"
                    } else {
                        "next_absent"
                    }
                    .into(),
                );
            }
            if !scrolling && pages > 1 && rows.records.len() == before {
                break (false, "repeated_page".into());
            }
            if pages == spec.max_pages {
                break (false, "page_limit".into());
            }
            let movement = if scrolling {
                TableMove::Scroll
            } else {
                TableMove::Next
            };
            actions += 1;
            match self.advance_table(resource, spec, &page, movement, context)? {
                Some(next) => page = next,
                None => break (false, "no_progress".into()),
            }
        };
        context.remaining()?;
        Ok(json!({
            "scope":if at_top {"virtual_scroll_from_top"} else if scrolling {"virtual_scroll_from_current_position"} else if first_verified {"pagination_from_first_page"} else {"pagination_from_current_page"},
            "columns":spec.columns,"row_key":spec.row_key,"records":rows.records,
            "row_count":rows.records.len(),"expected_rows":expected,"pages":pages,"actions":actions,
            "complete":complete,"stop_reason":reason
        }))
    }
}
