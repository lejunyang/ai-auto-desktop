@echo off
setlocal
where py >nul 2>nul
if %errorlevel% equ 0 (
  py -3 "%~dp0ocr_tesseract_plugin.py" %*
) else (
  python "%~dp0ocr_tesseract_plugin.py" %*
)
