# Complete browser container observations

Use `control_observe` with `view: {"mode":"nodes", "scope_id":"results"}`,
or CLI `aad describe RESOURCE --scope-id results` and
`aad do click RESOURCE --scope-id results --name Save`.
The id must identify exactly one HTML element in the selected frame's document.
This selects the capture root before traversal; `view.region` remains a display
filter and does not change capture completeness.

The answer includes `scope.id`, `scope.revision`, and `scope.complete`.
The root and its descendants, including open shadow trees, share the node and
scan budgets. Truncation still blocks finding and input. Outside names do not
participate in matching. Duplicate matches inside the capture remain errors.

Targets retain session, resource/frame, observation and node identities.
A new observation invalidates old references. A document mutation or mutation
in an observed open shadow tree invalidates scoped references, including a
container replaced with an identical copy. This deliberately also invalidates
on unrelated document updates; reobserve before acting on a dynamic page.

Select an iframe resource separately; scopes never traverse into its document.
To include an open shadow tree, select its host or an ancestor in the document.
Selecting an id inside a shadow root is unsupported and returns scope-not-found.
Closed shadow contents are inaccessible and are not part of the observable DOM.
Native providers return `CONTROL.SCOPE_UNSUPPORTED`.

`tests/windows/verify_scoped_observation.py` maps the range tests to a >2100-node
page, outside duplicates, inside ambiguity, truncated range, root replacement,
old revision, iframe isolation and open shadow descendants. It runs on an
isolated headless browser and loopback fixture.
