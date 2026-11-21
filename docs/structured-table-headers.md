# Selecting columns under grouped headers

Use the same column strings for `control_read_table`, collection specs and CSV headers. For example, a two-row header with a vertically merged `ID` and two `Net` columns under different years accepts this selection in export order.

```json
{"columns":["/2026/Net","ID","/2025/Net"],"row_key":"ID"}
```

The text view exposes `tables[].column_keys` for a valid multi-row header. Each path contains the rendered labels from top to bottom. A header spanning several rows contributes its label once. Two distinct headers with the same text each contribute a segment. Within a segment, encode `~` as `~0` and `/` as `~1`. On a multi-row header, the standalone label `/ID` becomes `/~1ID`. On a single-row header, names stay literal. Each requested key must occur exactly once.

For HTML, put all header tiers in `thead`. Without it, a spanning first header can introduce additional consecutive all-`th` rows in the same row group. Values spanning multiple logical columns or rows appear in each covered position. Hidden rows count toward a native rowspan, and `rowspan=0` ends at the row-group boundary. Missing cells return JSON null. Overlapping cells or a child header crossing parent groups prevent named projection.

For a DOM-owned ARIA `grid` or `table`, use leading rows of `columnheader`, followed by rows of `rowheader`, `gridcell` or `cell`. Row groups and presentation wrappers are supported. Explicit column indexes must agree with consecutive logical slots, including occupied rowspans. Row-index gaps mark the result as partially rendered. External `aria-owns` and contradictory indexes prevent named projection. Horizontal virtualization with a declared column count beyond the available slots reports `columns_not_rendered`.

Each response covers up to 200 rendered rows and 50 logical columns per table, with 500 UTF-16 code units per cell. Each repeated value and each emitted multi-row key consumes the shared table character budget. Inspection stops at 2,000 physical rows per table, with an additional 100,000-element traversal bound for ARIA structure. Inspect `truncated` and `truncation_reasons` on a read, and `complete` and `stop_reason` on a collection. [Chromium regression fixtures](../tests/browser/table-layout.test.mjs)
