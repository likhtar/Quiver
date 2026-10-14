@echo off
cd /d "%~dp0"

if not exist "quiver-chat.exe" (
    echo [ERROR] quiver-chat.exe not found next to this .bat.
    echo Keep the whole folder together.
    pause
    exit /b 1
)

if not exist "credentials.ron" (
    echo [ERROR] credentials.ron not found.
    echo Copy credentials.ron.example to credentials.ron and fill in
    echo client_id / client_secret first, then run this again.
    pause
    exit /b 1
)

echo ============================================================
echo  Twitch authorization (Device Code Flow)
echo ============================================================
echo.
echo  1. A browser will open (or open https://www.twitch.tv/activate)
echo  2. Enter the code shown below
echo  3. Approve access
echo.
echo Waiting for approval - DO NOT close this window...
echo.

quiver-chat.exe --config chat.ron --auth

echo.
echo ------------------------------------------------------------
echo If it printed "Stored at: ...", your oauth.json is saved and
echo redeems / subs / raids / cheers will now show up in the chat.
echo ------------------------------------------------------------
pause
