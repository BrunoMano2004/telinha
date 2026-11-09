@echo off
setlocal
if "%~1"=="" exit /b 1
set "TELINHA_UI_OUTPUT=%~f1"
set "TELINHA_UI_SOURCE=%~dp0telinha.cpp"
for /f "usebackq delims=" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "TELINHA_VS=%%i"
if not defined TELINHA_VS (
  echo Instale o Visual Studio Build Tools com Desktop development with C++.
  exit /b 1
)
call "%TELINHA_VS%\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 exit /b 1
cl /nologo /std:c++17 /EHsc /O2 /MT /utf-8 "%TELINHA_UI_SOURCE%" /Fe:"%TELINHA_UI_OUTPUT%" /Fo:"%TELINHA_UI_OUTPUT%.obj" /link /SUBSYSTEM:WINDOWS user32.lib shell32.lib gdi32.lib comctl32.lib
exit /b %errorlevel%
