@echo off
rem Test-only fake CLI. Set FAKE_CODEX_STATUS to chatgpt, api_key, signed_out, timeout.
if /I "%1"=="login" goto login
if /I "%1"=="logout" (
  echo Logged out
  exit /b 0
)
echo unsupported fake command
exit /b 2
:login
if /I "%FAKE_CODEX_STATUS%"=="api_key" (
  echo Authenticated with API key
  exit /b 0
)
if /I "%FAKE_CODEX_STATUS%"=="signed_out" (
  echo Not logged in
  exit /b 1
)
if /I "%FAKE_CODEX_STATUS%"=="timeout" timeout /t 30 /nobreak >nul
echo Logged in using ChatGPT
exit /b 0
