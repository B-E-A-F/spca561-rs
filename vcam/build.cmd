@echo off
rem Build the virtual camera COM server and its host.
rem
rem Kept out of cargo entirely: this is a COM in-process server, which cargo
rem has no idea how to produce, and the Rust side does not link against it --
rem they meet through shared memory instead. Run this by hand when the C++
rem changes.
setlocal

set VCVARS="C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if not exist %VCVARS% (
    echo Could not find vcvars64.bat -- install the Visual Studio Build Tools
    echo with the "Desktop development with C++" workload.
    exit /b 1
)
call %VCVARS% >nul 2>nul

cd /d "%~dp0"
if not exist build mkdir build

rem Two things hold the build output between runs, and both need elevation to
rem clear, so an elevated build does it and an unelevated one explains itself.
rem
rem The Frame Server keeps vcam_source.dll loaded after the camera goes away.
rem And vcam_host can wedge while the frame server is mid-conversation with it,
rem at which point even Stop-Process is refused. Either produces LNK1104 on the
rem next build, on whichever file it is holding.
net session >nul 2>&1
if not errorlevel 1 (
    echo Clearing stale processes and the DLL lock ...
    taskkill /f /im vcam_host.exe >nul 2>&1
    taskkill /f /im spca561.exe >nul 2>&1
    net stop FrameServer >nul 2>&1
    net start FrameServer >nul 2>&1
) else (
    echo Not elevated -- if the link fails with LNK1104, that is why.
)

echo Building vcam_source.dll ...
cl /nologo /EHsc /std:c++17 /W3 /O2 /LD ^
   /Fo:build\ /Fe:build\vcam_source.dll ^
   src\source.cpp src\shared.cpp ^
   /link /DEF:src\vcam_source.def
if errorlevel 1 (
    echo.
    echo LNK1104 means something still holds the file. Re-run this build from an
    echo elevated prompt, which clears both causes automatically.
    echo.
    exit /b 1
)

echo Building vcam_host.exe ...
cl /nologo /EHsc /std:c++17 /W3 /O2 ^
   /Fo:build\ /Fe:build\vcam_host.exe ^
   src\host.cpp src\shared.cpp
if errorlevel 1 exit /b 1

echo.
echo Built build\vcam_source.dll and build\vcam_host.exe
