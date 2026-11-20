# Record a browser and native windows together

Open the Chromium page and the native application. Use a browser with a local CDP endpoint. On Linux the native application must be visible to AT-SPI in a native X11 session.

```powershell
aad apps
aad browser-record targets --cdp-port 9222
aad describe '<native resource_id>'
aad browser-record mixed start --native '<invoice resource_id>' --window 'Invoice' --dialog 'Approval' --also-native '<ledger resource_id>' 'Ledger' --page '<page id>' --cdp-port 9222 --out ./invoice-session
```

Give the full native resource ID, the main window's exact accessible name, and the exact browser page ID. Repeat `--dialog` for other named dialogs of the primary application, and `--also-native RESOURCE WINDOW` for windows in additional applications. Open all selected main windows before starting. Dialogs can open later. After **Mixed recording ready**, perform the task across the selected applications. Press Ctrl+C or run this in another terminal and wait for the start process to finish.

```powershell
aad browser-record mixed stop --out ./invoice-session
```

The result names **workflow.json**, a self-contained workflow with capture gaps in its metadata. **capture.json** contains the action count, filters, gap details and final status. The browser recorder also keeps its own evidence in the `browser` subdirectory. Reset the applications to their starting state and replay the workflow through the normal CLI.

```powershell
$env:AAD_CDP_PORT = '9222'
aad run ./invoice-session/workflow.json --permission desktop.observe --permission desktop.input
```

## Capture and replay scope

- The recorder registers the selected Chromium page, its frames and each named native application. Each native application must have a distinct process identity, separate from the browser processes. A file manager opened as a separate application can participate. Browser-owned file pickers, undeclared sibling windows, browser popups and unrelated tabs are outside this selection.
- Linux uses the provider's Watch, XRecord and AT-SPI. It checks provider capabilities, the AT-SPI bus and object path, window ancestry, and the observed control's supported actions. Toolkit selection is automatic. The native helper requires X11, a same-user PID, XRecord and window PID ownership. Wayland sessions are outside the helper's supported scope. The Linux provider starts its event listeners asynchronously and does not expose per-listener readiness or continuing liveness.
- Linux replay resolves the native application by name, finds the window, takes a fresh scoped snapshot, finds the element inside it, and dispatches the revalidated action. A main window and dialog can have identically named fields. The window locator must be unique in the application and the element locator must be unique within its window at replay. The Linux capture requires a complete application snapshot bounded at 2,000 nodes. Controls that disappear before attribution can produce gaps.
- Native capture covers replayable text replacements, pointer clicks, wheel and key chords reported by Watch. Protected native controls and unsupported native actions produce gaps. An AT-SPI edit represents an observed value change, which can also originate in application code. Browser capture accepts trusted DOM activity from the selected target and frames. It uses the browser recorder's existing role/name locators and HTTP(S) page-path selectors. Replay needs unambiguous, already-open resources.
- Ordering uses DOM event wall time and Linux Watch observation timestamps. The recorder brackets a Chromium monotonic-clock sample to align the two on the same host. It stably orders the common capture interval, reports adjacent cross-source events inside the alignment uncertainty, and orders native events first on exact ties. Native accessibility notification latency can affect observed order.
- Consecutive Linux native and browser edits coalesce within a resource. A window, document or source switch breaks the run. Individual browser edit checkpoints preserve both sides of a native interruption. Event sequence IDs, redundant DOM change notifications and consequent form submits prevent duplicate actions.
- `needs_review` means capture produced a known gap such as an unsupported or truncated edit, dropped events, ambiguous ordering or an unresolved native target. Review these before replaying a partial workflow. An empty session has no workflow. A disconnect or fatal storage/collection error ends the session with an error and an incomplete capture status.
- `--seconds` bounds capture to 1–3,600 seconds. The combined timeline accepts up to 20,000 events and the workflow compiler accepts up to 500 logical actions. Browser evidence uses a 64 MiB budget, network filtering, disabled body capture and disabled screenshots. Captured page text and normal input values can appear in the output.

## Verification

From the repository root, build the CLI and run the isolated fixture. It requires PowerShell 7, Xvfb, D-Bus, an EWMH window manager (openbox or xfwm4), xdotool, Qt 5 Widgets and GTK 3 development files, the X11 helper build dependencies, Node with WebSocket support, and Edge. Set `AAD_TEST_CHROME` or `AAD_TEST_CLI` to override the executable paths.

```powershell
osdk exec --tool rust -- cargo build --locked -p aad-cli
./tests/mixed-recording/run.ps1
```

The Qt and GTK fixtures run serially on a private display. Each types into a browser field, edits the invoice and ledger in separate processes, returns to the browser field, edits a newly opened dialog, and clicks a browser button. All native windows contain a field called Reference. The fixture restarts both native processes, reloads the page and compares application notifications and HTTP requests during replay against the six expected effects. A second session types a 501-character browser value and verifies that the gap survives copying the replay artifact.

Windows UIA watches each selected window and stamps events when the callback runs. It reports a gap for a newly attached dialog because input may have preceded its subscription. macOS AX captures the process, window or sheet, and timestamp, then replays through a fresh observation of that application. Both adapters report missing observation sources and buffer overflow in the capture report.

**[待验证]** Live mixed recording and replay on a Windows interactive desktop and on macOS with Accessibility and Input Monitoring permissions. The Linux fixture above covers AT-SPI/X11.
