# Single-step verification

`control_act` accepts an optional `verify` locator, for example
`{"target": TARGET, "action": {"type": "click"}, "verify": {"name": "Saved"}, "timeout_ms": 5000}`.
The CLI equivalent is `aad do click --name Save --verify '{"name":"Saved"}'`.

Input is dispatched once. The existing read-only wait observes the same resource
until exactly one matching element appears, within the request deadline. An
empty locator is rejected before input. Ambiguous or truncated observations do
not prove success. The verification does not retry input.

`submitted` describes dispatch; `verified` describes the observed condition.
`verification` is `verified`, `timeout_unconfirmed`, or `unconfirmed`.
Failure includes the wait error and a bounded summary of the last observation (20 nodes, 128 characters per name) and the last 20 network requests as aggregate counts, with collection start and dropped counts; network bodies and credentials are excluded;
`backend_result` preserves the actual dispatch method. No verification option
preserves the existing response contract. A matching element already present
can satisfy the condition: choose a locator that represents the desired result.

Validation: `tests/windows/verify_single_step.py` uses an isolated headless
browser and loopback page, waits for a delayed DOM update, then checks a timeout
and the exact click count. `single_step_verification_never_replays_input` in
`crates/aad-control/tests/session.rs` covers success and timeout dispatch counts.
