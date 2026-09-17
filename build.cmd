@echo off
chcp 65001 >nul
title deveco-local-model-dashboard build
cd /d "%~dp0"
echo [build] Starting build...
echo.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
set "EC=%ERRORLEVEL%"
echo.
echo [build] Finished, exit code %EC%
if not "%EC%"=="0" (
  echo [build] Build failed. See errors above.
)
pause
