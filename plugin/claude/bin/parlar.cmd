@echo off
rem Runs the installed parlar on Windows, where a harness cannot start the sh launcher next to
rem this file. Before parlar is installed the plugin stays out of the way: hooks exit quietly.
setlocal
set "P=%PARLAR_BIN%"
if not defined P if exist "%USERPROFILE%\.cargo\bin\parlar.exe" set "P=%USERPROFILE%\.cargo\bin\parlar.exe"
rem the PATH lookup skips this file, which may itself be on PATH
if not defined P for %%I in (parlar.exe parlar.cmd) do if not defined P if not "%%~$PATH:I"=="" if /i not "%%~$PATH:I"=="%~f0" set "P=%%~$PATH:I"
if defined P (
  "%P%" %*
  exit /b %ERRORLEVEL%
)
if "%~1"=="hook" exit /b 0
echo parlar is not installed; run /parlar:setup 1>&2
exit /b 1
