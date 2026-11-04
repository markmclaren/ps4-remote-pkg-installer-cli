@echo off
setlocal enabledelayedexpansion

set "HERE=%~dp0"
set "EXE=%HERE%rust-cli\target\release\ps4-remote-pkg-installer.exe"
if not exist "%EXE%" set "EXE=%HERE%rust-cli\target\release\ps4-remote-pkg-installer-cli.exe"
if not exist "%EXE%" set "EXE=%HERE%ps4-remote-pkg-installer.exe"
if not exist "%EXE%" set "EXE=%HERE%ps4-remote-pkg-installer-cli.exe"

if not exist "%EXE%" (
    echo Error: "%EXE%" was not found.
    pause
    exit /b 1
)

:: Determine PS4 Console IP
if not "%~1"=="" (
    set "CONSOLE_IP=%~1"
) else if not "%PS4_CONSOLE%"=="" (
    set "CONSOLE_IP=%PS4_CONSOLE%"
) else if exist "%HERE%ip.txt" (
    set /p "CONSOLE_IP="<"%HERE%ip.txt"
) else (
    set /p "CONSOLE_IP=Enter PS4 Console IP address: "
)

if "%CONSOLE_IP%"=="" (
    echo Error: PS4 console IP address is required.
    pause
    exit /b 1
)

echo Using PS4 Console IP: %CONSOLE_IP%
echo Opening web dashboard: http://%CONSOLE_IP%:12800 ...
start "" "http://%CONSOLE_IP%:12800"
echo.

:: Check status first
echo Checking Remote PKG Installer connection...
"%EXE%" --console "%CONSOLE_IP%" status
if errorlevel 1 (
    echo Warning: Could not connect to PS4 Remote PKG Installer at %CONSOLE_IP%.
    set /p "CONTINUE=Try installing anyway? (Y/N): "
    if /i not "!CONTINUE!"=="Y" exit /b 1
)

echo.
set "FOUND=0"
for %%F in ("%HERE%*.pkg") do (
    set "FOUND=1"
    echo ========================================================
    echo Installing: %%~nxF
    echo ========================================================
    "%EXE%" --console "%CONSOLE_IP%" install "%%~fF"
    if errorlevel 1 (
        echo Error installing: %%~nxF
        set /p "RETRY=Continue with remaining packages? (Y/N): "
        if /i not "!RETRY!"=="Y" exit /b 1
    ) else (
        echo Successfully finished: %%~nxF
    )
    echo.
    timeout /t 2 /nobreak >nul
)

if "!FOUND!"=="0" (
    echo No .pkg files found in %HERE%
)

echo All done.
pause
endlocal

