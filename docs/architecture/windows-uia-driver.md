# Windows UIA process driver

> Status: first vertical slice, 2026-08-24. The capability is Windows-only and
> uses optional `comtypes` bindings to `UIAutomationClient`.

## Contract and boundaries

The provider advertises `metadata.name: desktop.windows_uia` and these v1
actions: `list_windows`, `snapshot`, `find`, `focus`, `invoke`, and
`set_value`. A workflow `uses` value is built from the manifest identity, action
key, and contract major, for example
`desktop.windows_uia.snapshot@1`. The manifest declares only `windows` in
`runtime.platforms`.

The worker reads and writes one UTF-8 JSON object per line. Protocol output is
the only stdout content; bounded diagnostics go to stderr. Requests are limited
to 1 MiB and responses to less than the host's 8 MiB frame ceiling.
`deadline_ms` is an absolute Unix epoch time. The worker translates it once to
a monotonic deadline and checks it during window enumeration, tree traversal,
locator resolution, and immediately before each native write. A single COM call
cannot be preempted safely from Python, so the host process timeout remains the
hard-stop boundary.

The backend is optional. On non-Windows systems, or when `comtypes` /
`UIAutomationCore` cannot be initialized, manifest negotiation still succeeds
and action calls return structured `DRIVER.UNAVAILABLE`. This makes fake-backend
contract tests portable while keeping the capability honest about its platform.

This slice deliberately has no keyboard or pointer injection, screenshot
capture, OCR, clickable-point fallback, or implicit alternate locator. It only
uses native `SetFocus`, `InvokePattern.Invoke`, and `ValuePattern.SetValue`.

## Normalized observations

Each `snapshot` result has this stable outer shape:

```json
{
  "snapshot_id": "<worker-generation>:<revision>",
  "revision": 1,
  "app": {},
  "window": {},
  "nodes": [],
  "truncated": false
}
```

Nodes are a flat, parent-linked list and contain `node_id`, `parent_id`,
`role`, `name`, `value`, `states`, `bounds`, `actions`, and `provenance`.
Bounds use physical screen pixels when supplied by UIA. Password values are not
read. Native COM elements never cross the process boundary. UIA RuntimeId can
contribute to the private replacement fingerprint, but it is not exposed as a
durable or replayable handle.

`snapshot_id`, `revision`, and node IDs are valid only for the current worker
revision. Taking another snapshot invalidates earlier node references. A driver
restart changes the generation portion of `snapshot_id`.

## Locator and write semantics

Locators are structured predicates over role, name, value, automation ID, class
name, framework ID, states, and supported actions. This v0 slice only supports
exact, case-sensitive string comparison. The resolver returns
`DRIVER.NOT_FOUND` for zero matches and `DRIVER.AMBIGUOUS` with bounded candidate
summaries for multiple matches; it never selects the first candidate.

`find` returns a snapshot-scoped target:

```json
{
  "snapshot_id": "...",
  "revision": 1,
  "node_id": "n12"
}
```

Every write action requires both this target and the original locator. Before
dispatch, the worker verifies that the target belongs to the current snapshot,
captures a fresh UIA tree, resolves the locator again, and compares a semantic
fingerprint. Missing, newly ambiguous, or replaced targets return
`DRIVER.STALE_SNAPSHOT` without calling the native pattern. A native write
invalidates the current snapshot even when the backend reports failure, because
the worker cannot prove the tree is unchanged after the dispatch boundary.

The effects are conservative: `invoke` is non-idempotent; `focus` and
`set_value` are contextual. Errors keep stable `DRIVER.*` codes with bounded,
non-sensitive detail. Native failures after entering the pattern call carry an
unknown-effect marker and must not be replayed blindly.

## Launch and qualification

On Windows, use `plugins\windows_uia\run.cmd`, or pass Python an explicit argv
containing `windows_uia_driver.py`. `run.sh` exists only for protocol and fake
contract work on POSIX hosts.

The Linux/macOS test path validates the manifest, unavailable behavior,
normalized snapshots, exact/ambiguous/not-found resolution, stale replacement,
all three native actions through a fake backend, deadlines, structured errors,
and the input frame limit. Windows additionally performs a conservative smoke:
missing dependencies must yield `DRIVER.UNAVAILABLE`; an initialized backend may
only be required to enumerate visible top-level windows. Real application and
elevation/UIPI qualification remain separate platform acceptance work.
