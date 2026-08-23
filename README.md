# ai-auto-desktop

ai-auto-desktop is a Python 3.11 workflow runtime for process-isolated desktop automation plugins. Version 0.1 has a deliberately small trusted core: strict descriptors, a no-calls expression evaluator, bounded control flow, structured failures, and NDJSON plugins.

## Quick start

    python -m ai_auto_desktop validate examples/workflows/ocr-error-response.yaml
    python -m ai_auto_desktop run examples/workflows/ocr-error-response.yaml --plugin fixture=plugins/fixture/run.sh

Both commands write exactly one JSON object to stdout. Invalid descriptors and failed runs have a non-zero exit status. Package installation also exposes the ai-auto-desktop command.

Inputs and plugins use repeatable assignments:

    python -m ai_auto_desktop run workflow.yaml --input account_id='"123456"' --input retries=2 --plugin fixture='python plugins/fixture/fixture_plugin.py'

Each input value is JSON. Plugin commands are parsed as argv and are never sent through a shell.

## Descriptor and runtime

Only the canonical identity is accepted:

    apiVersion: ai-auto-desktop.dev/v1alpha1
    kind: Workflow
    metadata:
      name: hello
    budgets:
      max_duration: 30s
      max_executed_steps: 20
    steps:
      - id: done
        type: return
        value: hello

Core objects reject unknown fields. Step IDs are globally unique, including branches, error handlers, and cleanup. Supported step types are action, set, if, switch, foreach, while, block, script, fail, and return.

Whole expression templates keep their value type; expressions embedded in text are converted to text. Function and method calls are forbidden. Read-only and idempotent actions may retry structured retryable failures. A non-idempotent or contextual action that times out after its request was flushed returns ACTION.UNKNOWN_EFFECT and is never replayed.

## Python API

    from ai_auto_desktop import WorkflowRunner, load_descriptor

    workflow = load_descriptor("workflow.yaml")
    result = WorkflowRunner(
        workflow,
        plugins={"fixture": ["plugins/fixture/run.sh"]},
    ).run({"name": "Ada"})
    print(result.to_dict())

RunResult.status is succeeded, failed, timed_out, cancelled, or unknown_effect. Errors carry stable code, category, retryable, effect, details, cause, suppressed, and location fields.

## Process plugin protocol

Plugins exchange one JSON object per line over stdin and stdout. Startup supports either an unsolicited manifest or a manifest request with a request ID. Invocations carry type, id, action, args, and an absolute deadline_ms. Responses contain the matching ID and either result or a structured error. The host bounds output, drains stderr, and terminates the process group after timeout or protocol failure. A runnable fixture is in plugins/fixture.

## Scripts and security

Script steps are disabled unless `--allow-scripts` or `allow_scripts=True` is explicitly supplied. Version 0.1 runs them only on Linux when bubblewrap and `prlimit` are available: the worker receives JSON on stdin, has no host home or `/etc`, has a private network/PID namespace, and is bounded by wall-clock, CPU, address-space, file-size and output limits. Other platforms fail closed with `SCRIPT.SANDBOX_UNAVAILABLE` until equivalent OS isolation is implemented.

Current scope excludes native UI drivers, persistence and resume, secret storage, concurrent desktop writes, confirmation tokens, and taint enforcement. The v0 host does enforce declared action risk categories/levels and validates process manifests plus action input/output contracts.
