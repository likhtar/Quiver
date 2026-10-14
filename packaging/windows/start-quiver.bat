@echo off
cd /d "%~dp0"

if not exist "quiver-chat.exe" (
    echo [ERROR] quiver-chat.exe not found next to this .bat.
    echo Keep the whole folder together.
    pause
    exit /b 1
)

tasklist /FI "IMAGENAME eq quiver-chat.exe" | find /I "quiver-chat.exe" >nul
if %errorlevel%==0 (
    echo quiver-chat is already running.
    echo Widget : http://127.0.0.1:4783
    ping -n 6 127.0.0.1 >nul
    exit /b 0
)

if not exist "credentials.ron" (
    echo [note] credentials.ron not found - running WITHOUT Twitch API credentials
    echo        ^(no channel badges/emotes^). Copy credentials.ron.example
    echo        to credentials.ron and fill it in to enable them.
)

echo Starting quiver-chat... (hidden window)
powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command "Start-Process -FilePath '%~dp0quiver-chat.exe' -WorkingDirectory '%~dp0' -ArgumentList '--config chat.ron' -WindowStyle Hidden"

echo.
echo Widget : http://127.0.0.1:4783
echo Health : http://127.0.0.1:4783/health
echo Stop   : double-click stop-quiver.bat
echo.
ping -n 6 127.0.0.1 >nul
exit /b 0
