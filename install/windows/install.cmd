@echo off
rem Double-click to install playr. Arguments go to install.ps1, e.g. install.cmd -Uninstall
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
if errorlevel 1 pause
