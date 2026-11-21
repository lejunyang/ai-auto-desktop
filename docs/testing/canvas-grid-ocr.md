# Read a painted canvas grid

## Pointer actions on a canvas

Chromium exposes click, hover, scroll and drag on an observed canvas surface. You can drag between two places on the same canvas or supply a path to draw a polyline. Each point is a fraction inside its observed element. In the browser fixture, trusted input events and the pixels in the resulting PNG independently verify the drawing. Choosing a diagram node by its business name still requires the page to expose that object or the caller to locate it visually. [Pointer actions and paths](../control-sessions.md) · [Chromium canvas tests](../../crates/aad-control/tests/browser_e2e.rs)

## Read regular cells

`aad browser read --canvas-grid` reads single-line labels and numbers from one visible Chromium canvas. Supply the layout that the page paints. For example, if the canvas named `Ledger` contains three rows of data and three equal-width columns, run this in PowerShell 7 with the ledger page already open.

```powershell
$spec = '{"locator":{"name":"Ledger"},"rows":3,"columns":["region","amount","state"]}'
aad browser read --browser-port 9222 --url 'https://ledger.example/report' --canvas-grid $spec
```

Use the page's exact URL or select its observed browser resource with `--resource-id`. The locator must match exactly one canvas in a fresh observation. Column names label the output in left-to-right order. Rows run from the top of the bitmap to the bottom. Include any painted header in the row count. The caller supplies all row and column boundaries implicitly by declaring an equal-cell layout across the full canvas.

## Read the result

Each cell includes its one-based row, column name, text, confidence, uncertainty flag, and bounds. Bounds are measured in pixels from the top left of the canvas. The crop omits one pixel on each edge to exclude thin grid rules. For example, the second cell of a 600 × 180 three-by-three canvas occupies the following rectangle.

```json
{"left":200,"top":0,"width":200,"height":60}
```

`confidence` is the lowest Tesseract word score in the cell, on a scale of 0 to 1. A score below 0.80, missing text, or a truncated value sets `uncertain` to true. A uniform or unreadable cell has null text and confidence. Numbers remain strings so callers can inspect the original OCR output.

The spec accepts 1–256 rows and 1–32 distinct column names. Each cell must measure at least 4 × 4 pixels. The result contains up to 64 cells in row-major order and up to 256 characters per cell. Compare `declared_cells`, `returned_cells`, and `truncation_reasons` (`cell_limit` or `cell_text_limit`). Even when `truncated` is false, `dataset_complete` is null. The read covers one visible bitmap and has no evidence about additional rows in the application.

## Capture requirements

The canvas must be opaque, origin-clean, in the main document, and entirely visible. Its bitmap must map directly to the screen at one device pixel per CSS pixel, with integral placement, ordinary untransformed layout, and no canvas padding or border. The maximum is 2,048 pixels per side and 2,000,000 pixels in total. Chromium must support compositor visibility tracking and Web Crypto, available in secure contexts such as HTTPS or loopback.

The reader checks the observed element and capture protections before taking a viewport clip, then rechecks the element and visibility. OCR starts only when the decoded pixels match the digest of the observed canvas bitmap. A protected overlap or an unprovable clip returns an error. The page stays at its current scroll position.

## Linux acceptance

With Tesseract and `prlimit` installed, set `AAD_TEST_CHROME` to a Chrome for Testing executable. From the repository root run the following commands.

```powershell
cargo build --locked -p aad-cli
node --test tests/browser/canvas-grid.test.mjs
```

The headless fixture compares the returned text and cell bounds with the expected ledger, including an unreadable cell. It also checks ambiguity, offscreen placement, pointer-transparent occlusion, a password overlap, bitmap transparency, a cross-origin image, and truncation at 64 cells. [Browser fixture](../../tests/browser/canvas-grid.test.mjs)
