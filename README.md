# ai-auto-desktop

ai-auto-desktop is a Python 3.11 workflow runtime for process-isolated desktop automation plugins. Version 0.1 has a deliberately small trusted core: strict descriptors, a no-calls expression evaluator, bounded control flow, structured failures, and NDJSON plugins.

## Quick start

    python -m ai_auto_desktop validate examples/workflows/ocr-error-response.yaml
    python -m ai_auto_desktop run examples/workflows/ocr-error-response.yaml --plugin fixture=plugins/fixture/run.sh
    python -m ai_auto_desktop probe

Both commands write exactly one JSON object to stdout. Invalid descriptors and failed runs have a non-zero exit status. Package installation also exposes the ai-auto-desktop command.

Inputs and plugins use repeatable assignments:

    python -m ai_auto_desktop run workflow.yaml --input account_id='"123456"' --input retries=2 --plugin fixture='python plugins/fixture/fixture_plugin.py'

Each input value is JSON. Plugin commands are parsed as argv and are never sent through a shell.
The included example deliberately returns `OCR.LOW_CONFIDENCE`; its non-zero
exit demonstrates structured error propagation. If a workflow declares
`requires.permissions`, pass each permission explicitly with `--permission NAME`;
the host never treats a descriptor request as a grant.

`probe` performs conservative, read-only prerequisite checks for UIA on
Windows, Accessibility and Screen Capture authorization on macOS, and the
separate AT-SPI/X11/Wayland/portal/libei/uinput surfaces on Linux. An
unavailable check is reported in JSON and does not make the probe command
fail. The report is diagnostic evidence, not a claim that UI automation has
succeeded.

## Optional Tesseract OCR

The process plugin in `plugins/ocr_tesseract` implements
`vision.ocr.recognize@1`. It only accepts an explicit absolute image/artifact
path and never captures the screen. It can crop a declared pixel region, run
Tesseract with selected languages, enforce a minimum confidence, and return
line bounds plus named literal-text matches. Register it explicitly; the
workflow must also declare `filesystem.read` under `requires.permissions`:

    python -m ai_auto_desktop run workflow.yaml \
      --permission filesystem.read \
      --plugin vision.ocr=plugins/ocr_tesseract/run.sh

Tesseract is an optional system dependency. Pillow is only needed for region
cropping. Missing dependencies and low confidence are structured `OCR.*`
errors; OCR output remains untrusted data and only a later explicit `if` or
`switch` can choose a response action.

## Windows UIA driver

`plugins/windows_uia` is the first real native desktop driver. On Windows it
uses the optional `comtypes` binding to enumerate windows and normalize a
bounded UIA Control View. It exposes exact locator lookup plus native
`SetFocus`, `InvokePattern.Invoke`, and `ValuePattern.SetValue`; every write
re-snapshots and resolves the target before dispatch. Install with
`pip install .[windows-uia]` and register `plugins\windows_uia\run.cmd`.
Workflows declare `desktop.observe`, and write actions additionally declare
`desktop.input`; both still require explicit host grants. The driver does not
capture screenshots, run OCR, or inject keyboard/pointer input.

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

Current scope includes a first Windows UIA vertical slice but excludes qualified
macOS/Linux native drivers, persistence and resume, secret storage, concurrent
desktop writes, confirmation tokens, and taint enforcement. The v0 host does
enforce declared action risk categories/levels and validates process manifests
plus action input/output contracts.
