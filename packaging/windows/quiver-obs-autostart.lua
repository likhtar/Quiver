--[[
  Quiver — автозапуск локального сервера чату разом з OBS
  ======================================================================
  Що робить: коли OBS стартує і завантажує цей скрипт, він запускає
  quiver-chat.exe, який лежить у ТІЙ САМІЙ папці, що й цей файл.
  Якщо сервер уже запущений — нічого не робить.

  Як підключити (один раз):
    1. У OBS:  Tools -> Scripts  (Інструменти -> Скрипти)
    2. Натисни "+" (Add), вибери цей файл quiver-obs-autostart.lua
    3. Готово. Тепер при кожному запуску OBS чат піднімається сам.

  ВАЖЛИВО: поклади цей .lua у ту саму папку, де лежить quiver-chat.exe
  (тобто в корінь портабельної папки Quiver_windows).
--]]

local obs = obslua

local EXE_NAME = "quiver-chat.exe"
local ARGS     = "--config chat.ron"

local started_by_us = false
local stop_on_exit  = false

-- Каталог цього скрипта (з кінцевим слешем).
local function script_dir(script_path)
  return (script_path or ""):match("^(.*[\\/])") or ""
end

local function is_running()
  local f = io.popen('tasklist /FI "IMAGENAME eq ' .. EXE_NAME .. '" /NH')
  if not f then return false end
  local out = f:read("*a") or ""
  f:close()
  return out:find(EXE_NAME, 1, true) ~= nil
end

local function start_server(dir)
  if is_running() then
    obs.script_log(obs.LOG_INFO, "quiver-chat вже запущено — пропускаю")
    return
  end
  if dir == "" then
    obs.script_log(obs.LOG_WARNING, "не вдалось визначити папку скрипта")
    return
  end
  -- start "" /b /d "папка" "exe" аргументи
  local exe = dir .. EXE_NAME
  local cmd = string.format('start "" /b /d "%s" "%s" %s', dir, exe, ARGS)
  obs.script_log(obs.LOG_INFO, "старт quiver-chat: " .. cmd)
  os.execute(cmd)
  started_by_us = true
  -- Дати серверові час піднятися (не блокує OBS).
  os.execute("ping -n 2 127.0.0.1 >nul")
end

local function stop_server()
  if not started_by_us then
    obs.script_log(obs.LOG_INFO, "quiver-chat запущено не цим скриптом — не зупиняю")
    return
  end
  os.execute('taskkill /F /IM ' .. EXE_NAME .. ' >nul 2>&1')
  started_by_us = false
  obs.script_log(obs.LOG_INFO, "quiver-chat зупинено")
end

function script_description()
  return [[Автозапуск локального сервера Quiver разом з OBS.

Поклади цей .lua поряд з quiver-chat.exe. Скрипт стартує сервер, коли
OBS відкривається. Адреса віджета: http://127.0.0.1:4783]]
end

function script_properties()
  local props = obs.obs_properties_create()
  obs.obs_properties_add_bool(props, "stop_on_exit",
    "Зупиняти сервер при закритті OBS (якщо його запустив цей скрипт)")
  return props
end

function script_defaults(settings)
  obs.obs_data_set_default_bool(settings, "stop_on_exit", false)
end

function script_update(settings)
  stop_on_exit = obs.obs_data_get_bool(settings, "stop_on_exit")
end

function script_load(script_path)
  start_server(script_dir(script_path))
end

function script_unload()
  if stop_on_exit then
    stop_server()
  end
end
