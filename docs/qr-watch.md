# QR evidence and bounded polling

`control_watch_qr` takes `resource_id`, a unique `locator` for the QR image or
its container, optional `expired` and `confirmed` locators, and the last returned
`version` as `previous`. It freshly observes, uniquely locates and captures the
element, then decodes QR candidates. Decoded text is discarded. Multiple valid
codes return `ambiguous`; none returns `not_found`. The selected screenshot must
not be clipped. Returned PNG includes a white quiet zone.

Each call polls at 250 ms intervals for up to five seconds, additionally bounded
by the request deadline. Slow screenshot/decoder work counts against the request
deadline; five seconds is the normal polling target, not a network latency SLA.
Use `control_cancel` with the explicit `request_id` to interrupt polling, or
close the session. Resume with another call and the prior version.

`changed` includes an MCP image content block and a version. `unchanged` includes
no image. `expired` requires a fresh unique match for the supplied expired
locator; image rotation alone never establishes expiry. `confirmed` requires the
supplied confirmation locator and returns `stop: true`. The host must stop its
polling loop when stop is true. No background listener remains after a call.
Refresh is an explicit, separately observed and authorized `control_act` action;
this read-only tool never clicks a refresh control automatically.

Embedder contract: keep the version only in the login task's own state; forward
the MCP image privately, to the person who will scan it, using the host's own
image/file messaging. Replace the previous displayed image on changed, mark it expired
on expired, and stop on confirmed or cancellation. Do not log PNG/base64 or QR
challenge contents, post to groups, or retain images in the general knowledge
base. aad contains no messaging credentials and performs no message send.

Generate fixtures with `AAD_QR_FIXTURES=target/qr-fixtures cargo test -p
aad-control qr::tests --lib` (set the environment variable using PowerShell).
Then run `python3 tests/windows/verify_qr_watch.py` after building aad-cli. The
test uses loopback, an independent temporary profile, real scannable images,
explicit expired/confirmed page text, and an in-memory owner-private delivery
mock. The decoder test re-decodes the delivered PNG, checks white padding and
multiple-code ambiguity without exposing the decoded test ticket.


Decode runs in a private same-source aad subprocess (at most two per host).
The host checks cancellation and the request deadline every 10 ms and kills
and reaps the decoder on termination. No PNG or challenge is written to disk.
Embedders must place the matching aad executable beside their host executable;
otherwise watch returns QR.WORKER_UNAVAILABLE. Event publication checks request
state again after capture and decode.

For deterministic decoder cancellation tests, build the CLI with
`cargo build -p aad-cli --features aad-control/qr-test-hooks` before running
verify_qr_watch.py. The opt-in build accepts a fixture-only decoder barrier;
normal builds do not include that hook.


Verified carriers are img, canvas, inline SVG, and a CSS background-image
container. Give the rendered element a unique accessible name and use that
locator. For a same-origin iframe, locate the uniquely named iframe container in
the main page resource; the tool decodes the visible rendered frame screenshot.
Direct capture of an element in a child-frame resource returns
CONTROL.ACTION_UNSUPPORTED ("screenshots are taken of the main frame's
elements only"). Cross-origin iframe QR delivery is not supported by this
workflow; do not interpret absence on the parent resource as absence in the
frame. The fixture checks changed, image delivery and re-decodes each delivered
PNG for all four additional carriers.


Screenshot waiting also checks request cancellation/deadline at 10 ms intervals.
At most eight backend screenshot workers can remain outstanding; each retains
the original request's finite timeout. A cancelled caller never publishes their
later image or backend error. Exhausted screenshot capacity returns CONTROL.BUSY.
