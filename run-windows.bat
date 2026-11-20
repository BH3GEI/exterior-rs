@echo off
rem exterior on Windows: double-click to start. Opens the web interface; close this window to stop exterior.
rem PORT (default 3000) or EXTERIOR_ADDR (host:port) change the address; EXTERIOR_VAR the data folder.
cd /d "%~dp0"
if "%PORT%"=="" set PORT=3000
start "" /b powershell -NoProfile -Command "Start-Sleep 2; Start-Process http://127.0.0.1:%PORT%/"
exterior.exe
pause
