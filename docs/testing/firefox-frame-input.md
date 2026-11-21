# Firefox BiDi input in child frames

Use `aad control serve --provider browser.firefox_bidi --browser-bidi-port PORT --allow-input` with a Firefox BiDi endpoint on loopback. List resources with `targets`, select the child's `resource_id` by URL and parent context, then `observe` that resource and `find` the element. Submit `act` with the returned target. Check the observed node's `actions` list for availability.

| Action | Example action object |
| --- | --- |
| Select all in the observed field | `{"type":"key","keys":["ctrl","a"]}` |
| Type through the keyboard | `{"type":"type_text","text":"Order 17","mode":"keys"}` |
| Click a quarter of the way across the observed button | `{"type":"click","fraction":{"fx":0.25,"fy":0.5}}` |
| Move the pointer to an observed control | `{"type":"hover"}` |
| Scroll the observed panel | `{"type":"scroll","direction":"down","notches":1}` |

Physical input requires a visible, focused top-level page and a chain of same-origin HTTP(S) HTML iframes. Each embedding viewport must map to its parent by a whole CSS-pixel offset. The provider accounts for borders and padding. A child action needs a target point already in view. Cross-origin or transformed embeddings, shadow-host ancestry, and unprovable geometry retain the existing element-local DOM actions where supported.

The observation binds the documents, iframe elements, embedding ancestors and geometry. Before dispatch, Firefox must still hit the target in the child and the corresponding iframe at every parent hit test. Keys also require the observed element and its entire frame chain to hold focus after focus preparation. The provider rechecks between input phases and during a hold. If a handler changes focus or covers the frame after a press, the operation can report a partial effect and attempt input release. Inspect the error before retrying. A moved or reloaded frame requires a fresh observation.

Child `capture` returns `CONTROL.ACTION_UNSUPPORTED`. A point hit test proves input routing at one location. A screenshot would need separate protection and visibility checks for the entire pixel region across the embedding documents.

## Linux acceptance

Set `AAD_TEST_FIREFOX` to the Firefox executable and `AAD_TEST_AAD` to the built CLI. Set `AAD_BIDI_ARTIFACTS` to a private output directory for the results and control transcript. Run from the repository root with PowerShell 7.

```powershell
cargo build --locked -p aad-cli
Remove-Item Env:DISPLAY, Env:WAYLAND_DISPLAY -ErrorAction SilentlyContinue
node tests/browser/firefox_bidi_e2e.mjs
```

The fixture launches a temporary headless profile. Its HTTP server independently receives DOM event trust, click position, text selection, final field value and scroll offset. The regression includes an identically labelled sibling frame, overlays at both ancestor levels, an overlay introduced by hover, stale geometry, focus loss during a held key, a background tab, and a closed shadow slot introduced after observation. Verified on Linux Firefox 159.0.1.
