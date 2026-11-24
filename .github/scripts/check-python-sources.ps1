$ErrorActionPreference = 'Stop'

if (Test-Path -LiteralPath 'pyproject.toml' -PathType Leaf) {
    throw 'The workspace must not contain a top-level Python project.'
}

# The Rust browser driver embeds this adapter to call the optional local vision model.
$pathspec = @(
    '*.py', '*.pyi'
    ':!:tests/windows/*.py', ':!:tests/linux/*.py'
    ':(top,exclude,literal)crates/aad-control/src/browser/canvas_vision.py'
)
$unexpected = git ls-files -- @pathspec
if ($LASTEXITCODE -ne 0) { throw 'Cannot enumerate tracked Python files.' }
if ($unexpected) {
    throw "Unexpected tracked Python files:`n$($unexpected -join "`n")"
}
