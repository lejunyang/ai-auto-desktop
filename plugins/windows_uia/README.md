# Windows UIA process driver

`desktop.windows_uia` is a Windows-only NDJSON process capability. It uses the
optional `comtypes` package and the generated `UIAutomationClient` type library
for native `SetFocus`, `InvokePattern`, and `ValuePattern` operations.
Workflows must declare `desktop.observe` and, for write actions,
`desktop.input` under `requires.permissions`; the host must grant them
explicitly as well.

Start it on Windows with `run.cmd`, or with an explicit Python argv:

```text
python plugins\windows_uia\windows_uia_driver.py
```

`run.sh` exists only for cross-platform protocol and fake-backend tests. On a
non-Windows host, real actions fail with `DRIVER.UNAVAILABLE` while manifest
negotiation remains available. The driver does not inject keyboard or pointer
input, capture screenshots, or run OCR.
