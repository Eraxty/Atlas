@echo off
title Atlas

cd /d "%~dp0"

if not exist "atlas-windows.exe" (
    echo atlas-windows.exe not found next to this file.
    echo Keep this .bat in the same folder as the exe.
    pause
    exit /b 1
)

"atlas-windows.exe"
pause