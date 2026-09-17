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

echo Building vcam_source.dll ...
cl /nologo /EHsc /std:c++17 /W3 /O2 /LD ^
   /Fo:build\ /Fe:build\vcam_source.dll ^
   src\source.cpp src\shared.cpp ^
   /link /DEF:src\vcam_source.def
if errorlevel 1 exit /b 1

echo Building vcam_host.exe ...
cl /nologo /EHsc /std:c++17 /W3 /O2 ^
   /Fo:build\ /Fe:build\vcam_host.exe ^
   src\host.cpp src\shared.cpp
if errorlevel 1 exit /b 1

echo.
echo Built build\vcam_source.dll and build\vcam_host.exe
