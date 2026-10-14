@echo off
taskkill /F /IM quiver-chat.exe >nul 2>&1
echo quiver-chat stopped.
ping -n 4 127.0.0.1 >nul
