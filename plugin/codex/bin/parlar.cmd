@echo off
rem Runs the installed parlar on Windows, where a harness cannot start the sh launcher next to
rem this file. Before parlar is installed the plugin stays out of the way: hooks exit quietly.
setlocal
set "P=%PARLAR_BIN%"
if not defined P if exist "%USERPROFILE%\.cargo\bin\parlar.exe" set "P=%USERPROFILE%\.cargo\bin\parlar.exe"
rem an npm install: run the native binary inside the package, not the Node launcher
if not defined P for %%I in (parlar.cmd) do if not "%%~$PATH:I"=="" if exist "%%~dp$PATH:Inode_modules\@agent-sh\parlar\npm\bin\native\parlar.exe" set "P=%%~dp$PATH:Inode_modules\@agent-sh\parlar\npm\bin\native\parlar.exe"
rem the PATH lookup skips this file, which may itself be on PATH
if not defined P for %%I in (parlar.exe) do if not "%%~$PATH:I"=="" if /i not "%%~$PATH:I"=="%~f0" set "P=%%~$PATH:I"
if not defined P goto missing
rem outside a ( ) block, so ERRORLEVEL is read after parlar exits
"%P%" %*
exit /b %ERRORLEVEL%
:missing
if "%~1"=="hook" exit /b 0
echo parlar is not installed; run /parlar:setup 1>&2
exit /b 1
