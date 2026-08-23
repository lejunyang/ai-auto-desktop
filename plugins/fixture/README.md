# Fixture capability plugin

fixture is a deterministic, standard-library-only process plugin for runtime
integration tests. It reads NDJSON from stdin, writes exactly one response per
non-blank request line to stdout, and sends all diagnostics to stderr.

## Run it

The launcher resolves its own directory, so it can be called from any working
directory:

~~~sh
printf '%s\n' \
  '{"type":"manifest","id":"m1"}' \
  '{"type":"invoke","id":"o1","action":"fixture.ocr@1","args":{"text":"Hello"}}' \
  | plugins/fixture/run.sh
~~~

Pass --manifest only when testing proactive negotiation. It emits a
type=manifest envelope containing the manifest before reading stdin. Without
the flag, the host can request the same manifest with a type=manifest request.

## NDJSON envelope

The canonical invocation request is:

~~~json
{"id":"request-1","action":"fixture.invoke@1","args":{"target":"save"}}
~~~

The fixture also ignores harmless envelope extensions such as type=invoke and
deadline_ms, and accepts JSON-RPC-style method/params. A successful call
returns an object with id and result. A failed call returns this shape:

~~~json
{
  "id": "request-1",
  "error": {
    "code": "FIXTURE.REQUESTED",
    "message": "fixture requested an error",
    "retryable": false,
    "data": {}
  }
}
~~~

The data member is omitted when no error data was supplied. Invalid JSON and
invalid envelopes receive protocol errors with an id of null; processing then
continues with later input lines.

## Capability manifest and actions

The handshake result is a canonical ai-auto-desktop.dev/v1alpha1
CapabilityManifest. Its metadata name is fixture, version is 1.0.0, and the
action map keys below have contract_major 1. The host therefore resolves their
full uses identifiers as follows:

| Manifest key | Full action ID | Input | Result |
| --- | --- | --- | --- |
| ocr | fixture.ocr@1 | text?, language?, confidence?, blocks?, result? | Mock text, language, confidence, and blocks, or the exact result override |
| invoke | fixture.invoke@1 | target?, operation?, result?, plus values to echo | An acknowledgement with ok, invoked, operation, target and args, or result |
| transient | fixture.transient@1 | key?, failures?, code?, message?, result? | Fails the first N attempts per key, then succeeds |
| error | fixture.error@1 | code?, message?, retryable?, data? | Always returns the requested structured error |
| sleep | fixture.sleep@1 | seconds?, milliseconds?, ms?, result? | Sleeps, then returns ok and sleptSeconds, or result |

transient defaults to one failure under key default. Its failures use
FIXTURE.TRANSIENT, set retryable true, and include key, attempt and failures in
error data. Different keys have independent counters for one process lifetime.

sleep accepts non-negative numeric seconds or milliseconds; seconds takes
precedence when more than one duration field is present. Short action names and
several legacy aliases remain accepted for host-protocol experiments, but only
the full action IDs above are part of the declared contract.
