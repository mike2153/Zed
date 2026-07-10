@echo off
setlocal

cd /d "%~dp0"

set "TARGET=x86_64-pc-windows-msvc"
set "BUILD_DIR=target\%TARGET%\release"
set "DIST_DIR=dist\mikes-zed-windows"
set "EXIT_CODE=1"

call :time_to_cs "%TIME: =0%" START_CS

echo Installing Rust target if needed...
rustup target add %TARGET%
if errorlevel 1 goto failed

echo Building Zed...
cargo build --release --target %TARGET% --package zed --package cli
if errorlevel 1 goto failed

if not exist "%BUILD_DIR%\zed.exe" (
    echo Missing "%BUILD_DIR%\zed.exe"
    goto failed
)

if not exist "%BUILD_DIR%\cli.exe" (
    echo Missing "%BUILD_DIR%\cli.exe"
    goto failed
)

if not exist "%DIST_DIR%" mkdir "%DIST_DIR%"
if errorlevel 1 goto failed

copy /Y "%BUILD_DIR%\zed.exe" "%DIST_DIR%\zed.exe" >nul
if errorlevel 1 goto failed
copy /Y "%BUILD_DIR%\cli.exe" "%DIST_DIR%\cli.exe" >nul
if errorlevel 1 goto failed

if exist "%DIST_DIR%\conpty.dll" del /Q "%DIST_DIR%\conpty.dll"
if errorlevel 1 goto failed
if exist "%DIST_DIR%\OpenConsole.exe" del /Q "%DIST_DIR%\OpenConsole.exe"
if errorlevel 1 goto failed

if exist "%BUILD_DIR%\conpty.dll" (
    copy /Y "%BUILD_DIR%\conpty.dll" "%DIST_DIR%\conpty.dll" >nul
    if errorlevel 1 goto failed
) else (
    echo Warning: conpty.dll was not built. Zed can run, but terminal support may be reduced.
)

if exist "%BUILD_DIR%\OpenConsole.exe" (
    copy /Y "%BUILD_DIR%\OpenConsole.exe" "%DIST_DIR%\OpenConsole.exe" >nul
    if errorlevel 1 goto failed
) else (
    echo Warning: OpenConsole.exe was not built. Zed can run, but terminal support may be reduced.
)

echo.
echo Build complete.
echo Copy this folder to the other Windows machine:
echo %CD%\%DIST_DIR%
echo.
echo Run it with:
echo %DIST_DIR%\zed.exe
set "EXIT_CODE=0"
goto finish

:failed
echo.
echo Build failed.

:finish
echo.
call :time_to_cs "%TIME: =0%" END_CS
if %END_CS% LSS %START_CS% set /A END_CS+=8640000
set /A ELAPSED_CS=END_CS-START_CS
set /A ELAPSED_H=ELAPSED_CS/360000
set /A ELAPSED_M=(ELAPSED_CS/6000)%%60
set /A ELAPSED_S=(ELAPSED_CS/100)%%60
if %ELAPSED_H% GTR 0 (
    echo Build time: %ELAPSED_H%h %ELAPSED_M%m %ELAPSED_S%s
) else (
    echo Build time: %ELAPSED_M%m %ELAPSED_S%s
)
echo.
pause
exit /b %EXIT_CODE%

:time_to_cs
for /f "tokens=1-4 delims=:." %%A in ("%~1") do set /A "%~2=(((1%%A-100)*60+1%%B-100)*60+1%%C-100)*100+1%%D-100"
exit /b 0
