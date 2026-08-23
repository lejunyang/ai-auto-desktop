# Tesseract OCR process plugin

This optional provider exposes `vision.ocr.recognize@1` over the repository's
NDJSON process protocol. It only reads a caller-supplied absolute image path;
it never takes screenshots or silently falls back to screen capture.

Requirements are the `tesseract` CLI on `PATH` and Python 3.11+. Region crops
also require Pillow (`python -m pip install Pillow`). Operators may point at a
specific binary with `TESSERACT_CMD`, or set `OCR_TESSERACT_COMMAND` to a trusted
JSON argv array (useful for wrappers and hermetic tests). This setting is
privileged operator configuration, not workflow input.

The provider manifest requires `filesystem.read`. A workflow must declare that
permission under `requires.permissions`, and the host operator must grant it
explicitly with `--permission filesystem.read`; declaration alone is never a
grant. The permission covers the caller-supplied absolute path. The provider
copies the file into a private request directory, hashes that snapshot, and
only gives the snapshot (or its crop) to Tesseract.

Register it explicitly:

~~~sh
python -m ai_auto_desktop run workflow.yaml \
  --permission filesystem.read \
  --plugin vision.ocr=plugins/ocr_tesseract/run.sh
~~~

On Windows, register the Python launcher directly (this also avoids depending
on a POSIX shell):

~~~powershell
py -3 -m ai_auto_desktop run workflow.yaml `
  --permission filesystem.read `
  --plugin "vision.ocr=py -3 plugins/ocr_tesseract/ocr_tesseract_plugin.py"
~~~

`run.cmd` is also provided for shells that accept a `.cmd` plugin command. It
uses `py -3` when the Python Launcher is registered, and falls back to
`python` from `PATH`.

The action accepts exactly one of `image: {path}` or
`artifact: {path, media_type?}`. Paths must be absolute regular image files no
larger than 64 MiB. Optional fields are pixel `region: {x,y,width,height}`, a
`languages` array of Tesseract language IDs, `minimum_confidence` from zero to
one, and `patterns: [{id,value}]`. A region is cropped before Tesseract runs,
and returned bounds remain relative to the original image. Pattern values are
literal, case-sensitive substrings; regular-expression syntax is intentionally
not executed.

Results contain provider/version provenance, the resolved source path and
SHA-256 digest, the requested source region, aggregate text/confidence, line
bounds, and pattern matches. Missing Tesseract is reported as
`OCR.ENGINE_UNAVAILABLE`; invalid inputs, unreadable or signature-invalid
images, engine failures, empty text, low confidence, malformed/oversized TSV,
NUL or non-UTF-8 output, and deadline expiry are also returned as structured
errors. Each engine launch gets a separate process group/session. POSIX timeout
and output overflow terminate that group; Windows uses `taskkill /T` (and
`/F` after a grace period) on a best-effort basis. Stdout, stderr, TSV rows,
words, lines, text, matches, and the final NDJSON response all have hard caps.
