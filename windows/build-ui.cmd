@echo off
setlocal
rem Uso: build-ui.cmd <saida.exe> <versao> <pasta build\native do SDK do WebView2>
if "%~3"=="" exit /b 1
set "TELINHA_UI_OUTPUT=%~f1"
set "TELINHA_UI_VERSION=%~2"
set "TELINHA_WEBVIEW2=%~f3"
set "TELINHA_UI_SOURCE=%~dp0telinha.cpp"
for /f "usebackq delims=" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "TELINHA_VS=%%i"
if not defined TELINHA_VS (
  echo Instale o Visual Studio Build Tools com Desktop development with C++.
  exit /b 1
)
call "%TELINHA_VS%\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 exit /b 1
rem O .rc referencia ..\assets\telinha.ico relativo a esta pasta.
pushd "%~dp0"
rc /nologo /fo "%TELINHA_UI_OUTPUT%.res" telinha.rc
set "TELINHA_RC=%errorlevel%"
popd
if not "%TELINHA_RC%"=="0" exit /b %TELINHA_RC%
rem O manifesto vem do .rc (/MANIFEST:NO evita um segundo, gerado pelo linker).
cl /nologo /std:c++17 /EHsc /O2 /MT /utf-8 /DTELINHA_VERSION=\"%TELINHA_UI_VERSION%\" /I"%TELINHA_WEBVIEW2%\include" "%TELINHA_UI_SOURCE%" "%TELINHA_UI_OUTPUT%.res" /Fe:"%TELINHA_UI_OUTPUT%" /Fo:"%TELINHA_UI_OUTPUT%.obj" /link /SUBSYSTEM:WINDOWS /MANIFEST:NO "%TELINHA_WEBVIEW2%\x64\WebView2LoaderStatic.lib" user32.lib shell32.lib gdi32.lib ole32.lib oleaut32.lib advapi32.lib version.lib dwmapi.lib winhttp.lib bcrypt.lib
exit /b %errorlevel%
