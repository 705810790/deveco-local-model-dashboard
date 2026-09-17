@echo off
chcp 65001 >nul
title deveco-local-model-dashboard build
cd /d "%~dp0"
echo [build] 使用隔离 Rust 工具链开始编译...
echo.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
set "EC=%ERRORLEVEL%"
echo.
echo [build] 完成，退出码 %EC%
if not "%EC%"=="0" (
  echo [build] 编译失败，请查看上方错误信息。
)
pause
