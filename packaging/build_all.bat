@echo off
setlocal
cd /d "%~dp0.."

echo ============================================================
echo   Host Monitor - desktop build (Rust backend embedded)
echo   No Python runtime / no PyInstaller sidecar.
echo ============================================================
echo.

set PATH=%USERPROFILE%\.cargo\bin;%PATH%
rem Serial build avoids a rare cargo/rustc jobserver deadlock on Windows.
set CARGO_BUILD_JOBS=1

echo [1/4] Build standalone xmm-server.exe (browser / service mode) ...
cargo build --release --manifest-path rs\xmm\Cargo.toml --bin xmm-server
if errorlevel 1 goto :err
echo.

echo [2/4] Stage web assets ...
if exist "shell\tauri\src-tauri\resources\web" rd /s /q "shell\tauri\src-tauri\resources\web"
xcopy /E /I /Y "web" "shell\tauri\src-tauri\resources\web" >nul
if errorlevel 1 goto :err
echo.

echo [3/4] Build Tauri NSIS installer ...
cd shell\tauri
call npx --yes @tauri-apps/cli@2 build
if errorlevel 1 goto :err
cd ..\..
echo.

echo [4/4] Copy deliverables into dist\ ...
if not exist "dist" mkdir "dist"
copy /Y "shell\tauri\src-tauri\target\release\bundle\nsis\*.exe" "dist\" >nul
copy /Y "rs\xmm\target\release\xmm-server.exe" "dist\xmm-server.exe" >nul
echo.

echo ============================================================
echo   Done.
echo   Installer : dist\*_x64-setup.exe
echo   Server bin: dist\xmm-server.exe  (browser / service mode)
echo.
echo   Optional Windows service: copy packaging\winsw\xiaozhushou-service.exe
echo   next to xmm-server.exe, then POST /api/service {"action":"install"}.
echo ============================================================
if not "%XMM_NO_PAUSE%"=="1" pause
exit /b 0

:err
echo.
echo *** BUILD FAILED ***
if not "%XMM_NO_PAUSE%"=="1" pause
exit /b 1
