use super::*;
use atspi::proxy::table::TableProxy;
use atspi::proxy::table_cell::TableCellProxy;
use atspi::Role;

fn unsupported(reason: &str) -> AutomationError {
    driver_error("DRIVER.ACTION_UNSUPPORTED", reason)
}

pub(super) fn protected_attributes(attrs: &std::collections::HashMap<String, String>) -> bool {
    attrs.iter().any(|(key, value)| {
        matches!(key.as_str(), "protected" | "password" | "hidden-text")
            && !value.eq_ignore_ascii_case("false")
            || key == "text-input-type" && value.eq_ignore_ascii_case("password")
    })
}

impl LinuxBackend {
    fn table_time(&self, deadline: Instant) -> Result<(), AutomationError> {
        remaining(deadline, false)?;
        if let Some(token) = &self.cancellation {
            token.check()?;
        }
        Ok(())
    }

    fn safe_table_leaf<'a>(
        &'a self,
        table: &NativeRef,
        leaf: &'a NativeRef,
        header: bool,
    ) -> Result<AccessibleProxy<'a>, AutomationError> {
        if table.bus_name != leaf.bus_name {
            return Err(unsupported("cell is outside the observed application"));
        }
        let accessible = self.accessible(leaf)?;
        let query = |err| native_failure("read table leaf", leaf, err);
        let role = zbus::block_on(accessible.get_role()).map_err(query)?;
        let allowed = if header {
            matches!(role, Role::ColumnHeader | Role::TableColumnHeader)
        } else {
            role == Role::TableCell
        };
        if !allowed
            || zbus::block_on(accessible.child_count()).map_err(query)? != 0
            || zbus::block_on(accessible.get_state())
                .map_err(query)?
                .contains(State::Defunct)
            || &Self::native(zbus::block_on(accessible.parent()).map_err(query)?) != table
        {
            return Err(unsupported(
                "cell is not a live, unambiguous unprotected leaf of the table",
            ));
        }
        let attrs = zbus::block_on(accessible.get_attributes()).map_err(query)?;
        if protected_attributes(&attrs) {
            return Err(unsupported("protected table content is not readable"));
        }
        Ok(accessible)
    }

    fn unit_cell_extent(
        &self,
        table: &TableProxy<'_>,
        cell: &NativeRef,
        row: i32,
        col: i32,
        has_cell_interface: bool,
    ) -> Result<bool, AutomationError> {
        let query = |err| native_failure("read cell extent", cell, err);
        let height = zbus::block_on(table.get_row_extent_at(row, col)).map_err(query)?;
        let width = zbus::block_on(table.get_column_extent_at(row, col)).map_err(query)?;
        if height > 1 || width > 1 {
            return Ok(false);
        }
        if !has_cell_interface {
            return Ok((height, width) == (1, 1));
        }
        // GTK3 supplies zero extents on Table but the actual position and spans on TableCell.
        let proxy = zbus::block_on(
            TableCellProxy::builder(self.connection.inner())
                .destination(cell.bus_name.as_str())
                .and_then(|builder| builder.path(cell.object_path.as_str()))
                .map_err(query)?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )
        .map_err(query)?;
        let reply =
            zbus::block_on(proxy.inner().call_method("GetRowColumnSpan", &())).map_err(query)?;
        let body = reply.body();
        // Bridges expose four integers, optionally preceded by a success flag.
        if let Ok(span) = body.deserialize::<(i32, i32, i32, i32)>() {
            return Ok(span == (row, col, 1, 1));
        }
        Ok(body
            .deserialize::<(bool, i32, i32, i32, i32)>()
            .map_err(query)?
            == (true, row, col, 1, 1))
    }

    pub(super) fn table_records(
        &self,
        target: &NativeRef,
        columns: &[String],
        mut budget: usize,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        self.table_time(deadline)?;
        let root = self.accessible(target)?;
        let query = |err| native_failure("read AT-SPI Table", target, err);
        if !matches!(
            zbus::block_on(root.get_role()).map_err(query)?,
            Role::Table | Role::TreeTable
        ) || !zbus::block_on(root.get_interfaces())
            .map_err(query)?
            .contains(Interface::Table)
        {
            return Err(unsupported("observed node does not expose an AT-SPI table"));
        }
        let table = zbus::block_on(
            TableProxy::builder(self.connection.inner())
                .destination(target.bus_name.as_str())
                .and_then(|builder| builder.path(target.object_path.as_str()))
                .map_err(query)?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )
        .map_err(query)?;
        let shape = || -> Result<(i32, i32), AutomationError> {
            if protected_attributes(&zbus::block_on(root.get_attributes()).map_err(query)?)
                || zbus::block_on(root.get_state())
                    .map_err(query)?
                    .contains(State::Defunct)
            {
                return Err(unsupported("table is protected or no longer live"));
            }
            Ok((
                zbus::block_on(table.inner().get_property("NRows")).map_err(query)?,
                zbus::block_on(table.inner().get_property("NColumns")).map_err(query)?,
            ))
        };
        let (rows, width) = shape()?;
        if rows < 0 || !(1..=128).contains(&width) {
            return Err(unsupported("unknown table dimensions or too many columns"));
        }
        let headers = || -> Result<Vec<String>, AutomationError> {
            (0..width)
                .map(|col| {
                    self.table_time(deadline)?;
                    let header =
                        Self::native(zbus::block_on(table.get_column_header(col)).map_err(query)?);
                    let leaf = self.safe_table_leaf(target, &header, true)?;
                    let name = zbus::block_on(leaf.name()).map_err(query)?;
                    if name.len() > 256 {
                        return Err(unsupported("column header exceeds its bound"));
                    }
                    Ok(name)
                })
                .collect()
        };
        let names = headers()?;
        let selected = columns
            .iter()
            .map(|name| {
                let mut matches = names.iter().enumerate().filter(|(_, label)| *label == name);
                let index = matches
                    .next()
                    .ok_or_else(|| unsupported("selected column header not found"))?
                    .0;
                if matches.next().is_some() {
                    return Err(unsupported("selected column header is ambiguous"));
                }
                Ok(index as i32)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut records = Vec::new();
        let mut reason = None;
        let mut seen = BTreeSet::new();
        'rows: for row in 0..rows {
            if row >= 500 || (row as usize + 1) * selected.len() > 2000 {
                reason = Some("cell_limit");
                break;
            }
            let mut record = Map::new();
            for (column, &col) in columns.iter().zip(&selected) {
                self.table_time(deadline)?;
                let cell_at = || {
                    zbus::block_on(table.get_accessible_at(row, col))
                        .map(Self::native)
                        .map_err(query)
                };
                let cell = cell_at()?;
                if !seen.insert(cell.object_path.clone()) {
                    return Err(unsupported("table positions share a cell"));
                }
                let leaf = self.safe_table_leaf(target, &cell, false)?;
                let interfaces = zbus::block_on(leaf.get_interfaces()).map_err(query)?;
                if !interfaces.contains(Interface::Text) {
                    return Err(unsupported("table cell has no plain Text interface"));
                }
                let unit_extent = || {
                    self.unit_cell_extent(
                        &table,
                        &cell,
                        row,
                        col,
                        interfaces.contains(Interface::TableCell),
                    )
                };
                if !unit_extent()? {
                    return Err(unsupported("merged or unknown table cell extent"));
                }
                let text = zbus::block_on(
                    TextProxy::builder(self.connection.inner())
                        .destination(cell.bus_name.as_str())
                        .and_then(|builder| builder.path(cell.object_path.as_str()))
                        .map_err(query)?
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build(),
                )
                .map_err(query)?;
                let count = zbus::block_on(text.character_count()).map_err(query)?;
                let cost = usize::try_from(count)
                    .map_err(|_| unsupported("unknown cell length"))?
                    .saturating_add(column.chars().count() + 1);
                if cost > budget {
                    reason = Some("max_characters");
                    break 'rows;
                }
                let value = zbus::block_on(text.get_text(0, count)).map_err(query)?;
                if value.chars().count() != count as usize
                    || zbus::block_on(text.character_count()).map_err(query)? != count
                    || cell_at()? != cell
                    || !unit_extent()?
                {
                    return Err(super::super::stale("table cell changed while reading"));
                }
                self.safe_table_leaf(target, &cell, false)?;
                budget -= cost;
                record.insert(column.clone(), json!(value));
            }
            records.push(record);
        }
        if (rows, width) != shape()? || names != headers()? {
            return Err(super::super::stale(
                "table shape or headers changed while reading",
            ));
        }
        self.table_time(deadline)?;
        Ok(
            json!({"scope":"atspi_table_rows","records":records,"row_count":rows,"truncated":reason.is_some(),"truncation_reasons":reason.into_iter().collect::<Vec<_>>() }),
        )
    }
}
