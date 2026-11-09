// Interface Win32 sem runtime adicional: janela, bandeja e processo servidor.
#ifndef UNICODE
#define UNICODE
#endif
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <shellapi.h>
#include <commctrl.h>
#include <string>
#include <vector>
#include <mutex>
#include <thread>
#include <sstream>
#include <fstream>

enum { ID_START = 101, ID_STOP, ID_COPY, ID_MONITOR, ID_AUDIO, ID_PUBLIC,
       ID_EXCLUDE, ID_OPEN, ID_EXIT, ID_LOG, WM_TRAY = WM_APP + 1, WM_LINE };
static HWND window, startButton, stopButton, copyButton, monitorBox, audioBox,
    publicBox, excludeBox, linkBox, statusLabel, logBox;
static HANDLE child, inputPipe, job, wakeEvent;
static std::thread reader;
static std::wstring folder, link;
static std::vector<HMONITOR> monitors;
static NOTIFYICONDATAW tray = {};
static bool quitting = false;
static HFONT font;
static std::mutex linesMutex;
static std::vector<std::wstring> lines;

static std::wstring Utf8(const std::string& text) {
    int size = MultiByteToWideChar(CP_UTF8, 0, text.data(), (int)text.size(), NULL, 0);
    std::wstring result(size, 0);
    if (size) MultiByteToWideChar(CP_UTF8, 0, text.data(), (int)text.size(), &result[0], size);
    return result;
}
static std::wstring Text(HWND control) {
    int size = GetWindowTextLengthW(control);
    std::wstring value(size + 1, 0);
    GetWindowTextW(control, &value[0], size + 1);
    value.resize(size);
    return value;
}
// Regras de quoting do CommandLineToArgvW (sem shell).
static std::wstring Quote(const std::wstring& value) {
    std::wstring result = L"\"";
    size_t slashes = 0;
    for (wchar_t ch : value) {
        if (ch == L'\\') { ++slashes; continue; }
        if (ch == L'\"') result.append(slashes * 2 + 1, L'\\');
        else result.append(slashes, L'\\');
        slashes = 0;
        result += ch;
    }
    result.append(slashes * 2, L'\\');
    return result + L"\"";
}
static void Status(const wchar_t* text) {
    SetWindowTextW(statusLabel, text);
    wcsncpy_s(tray.szTip, text, _TRUNCATE);
    Shell_NotifyIconW(NIM_MODIFY, &tray);
}
static void CopyLink() {
    if (link.empty() || !OpenClipboard(window)) return;
    HGLOBAL memory = GlobalAlloc(GMEM_MOVEABLE, (link.size() + 1) * sizeof(wchar_t));
    if (memory) {
        void* data = GlobalLock(memory);
        if (data) {
            memcpy(data, link.c_str(), (link.size() + 1) * sizeof(wchar_t));
            GlobalUnlock(memory);
            EmptyClipboard();
            if (!SetClipboardData(CF_UNICODETEXT, memory)) GlobalFree(memory);
        } else GlobalFree(memory);
    }
    CloseClipboard();
}
static void Show() { ShowWindow(window, SW_SHOWNORMAL); SetForegroundWindow(window); }
static void Append(const std::wstring& line) {
    // Limitado à memória desta sessão; o link privado não vai para logs em disco.
    if (GetWindowTextLengthW(logBox) > 50000) SetWindowTextW(logBox, L"");
    SendMessageW(logBox, EM_SETSEL, (WPARAM)-1, (LPARAM)-1);
    std::wstring text = line + L"\r\n";
    SendMessageW(logBox, EM_REPLACESEL, FALSE, (LPARAM)text.c_str());
}
static void Controls(bool running) {
    EnableWindow(startButton, !running); EnableWindow(stopButton, running);
    EnableWindow(monitorBox, !running); EnableWindow(audioBox, !running);
    EnableWindow(publicBox, !running); EnableWindow(excludeBox, !running);
}
static void CloseChild() {
    if (inputPipe) { CloseHandle(inputPipe); inputPipe = NULL; }
    // Mate descendentes antes de aguardar EOF do pipe herdado.
    if (job) { CloseHandle(job); job = NULL; }
    if (reader.joinable()) reader.join();
    if (child) { CloseHandle(child); child = NULL; }
    { std::lock_guard<std::mutex> lock(linesMutex); lines.clear(); }
    link.clear(); SetWindowTextW(linkBox, L""); EnableWindow(copyButton, FALSE);
    Controls(false); Status(L"Telinha - parado");
}
static void Stop() {
    if (!child) return;
    Status(L"Telinha - encerrando...");
    DWORD written;
    if (inputPipe) WriteFile(inputPipe, "q\n", 2, &written, NULL);
    if (WaitForSingleObject(child, 4000) == WAIT_TIMEOUT && job) TerminateJobObject(job, 0);
    WaitForSingleObject(child, 2000);
    CloseChild();
}
static void Start() {
    if (child) return;
    int selected = (int)SendMessageW(monitorBox, CB_GETCURSEL, 0, 0);
    if (selected < 0 || selected >= (int)monitors.size()) return;
    std::wstring args = Quote(folder + L"\\telinha.exe") + L" --monitor-handle " +
        std::to_wstring((unsigned long long)(UINT_PTR)monitors[selected]);
    if (SendMessageW(audioBox, BM_GETCHECK, 0, 0) == BST_CHECKED) args += L" --audio";
    bool isPublic = SendMessageW(publicBox, BM_GETCHECK, 0, 0) == BST_CHECKED;
    if (isPublic) args += L" --tunnel";
    else args += L" --require-tailscale";
    std::wstring excluded = Text(excludeBox);
    if (!excluded.empty()) args += L" --audio-exclude " + Quote(excluded);
    HANDLE outputRead = NULL, outputWrite = NULL, inputRead = NULL;
    SECURITY_ATTRIBUTES security = { sizeof(security), NULL, TRUE };
    bool pipes = CreatePipe(&outputRead, &outputWrite, &security, 0) &&
        CreatePipe(&inputRead, &inputPipe, &security, 0);
    if (outputRead) SetHandleInformation(outputRead, HANDLE_FLAG_INHERIT, 0);
    if (inputPipe) SetHandleInformation(inputPipe, HANDLE_FLAG_INHERIT, 0);
    job = CreateJobObjectW(NULL, NULL);
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits = {};
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    bool jobReady = job && SetInformationJobObject(job, JobObjectExtendedLimitInformation, &limits, sizeof(limits));
    STARTUPINFOW startup = {}; startup.cb = sizeof(startup);
    startup.dwFlags = STARTF_USESTDHANDLES;
    startup.hStdInput = inputRead; startup.hStdOutput = outputWrite; startup.hStdError = outputWrite;
    PROCESS_INFORMATION process = {};
    bool created = pipes && jobReady && CreateProcessW(NULL, &args[0], NULL, NULL, TRUE,
        CREATE_NO_WINDOW | CREATE_SUSPENDED, NULL, folder.c_str(), &startup, &process);
    // Suspenso até ser colocado no Job: nenhum processo auxiliar escapa.
    bool assigned = created && AssignProcessToJobObject(job, process.hProcess);
    if (outputWrite) CloseHandle(outputWrite);
    if (inputRead) CloseHandle(inputRead);
    if (!assigned) {
        if (created) { TerminateProcess(process.hProcess, 1); CloseHandle(process.hThread); CloseHandle(process.hProcess); }
        if (outputRead) CloseHandle(outputRead);
        CloseChild();
        MessageBoxW(window, L"Nao foi possivel iniciar o Telinha. Reinstale o aplicativo e tente novamente.", L"Telinha", MB_ICONERROR);
        return;
    }
    child = process.hProcess;
    Controls(true); SetWindowTextW(logBox, L""); Status(L"Telinha - preparando transmissao...");
    reader = std::thread([outputRead]() {
        char buffer[4096]; DWORD count; std::string pending;
        while (ReadFile(outputRead, buffer, sizeof(buffer), &count, NULL) && count) {
            pending.append(buffer, count);
            size_t end;
            while ((end = pending.find('\n')) != std::string::npos) {
                std::string line = pending.substr(0, end);
                if (!line.empty() && line.back() == '\r') line.pop_back();
                { std::lock_guard<std::mutex> lock(linesMutex); lines.push_back(Utf8(line)); }
                PostMessageW(window, WM_LINE, 0, 0);
                pending.erase(0, end + 1);
            }
            if (pending.size() > 65536) pending.clear();
        }
        CloseHandle(outputRead);
    });
    ResumeThread(process.hThread); CloseHandle(process.hThread);
}
static HWND Control(const wchar_t* type, const wchar_t* text, DWORD style, int x, int y, int w, int h, int id = 0) {
    HWND item = CreateWindowExW(0, type, text, WS_CHILD | WS_VISIBLE | style, x, y, w, h,
        window, (HMENU)(INT_PTR)id, GetModuleHandleW(NULL), NULL);
    SendMessageW(item, WM_SETFONT, (WPARAM)font, TRUE);
    return item;
}
static BOOL CALLBACK AddMonitor(HMONITOR handle, HDC, LPRECT, LPARAM) {
    MONITORINFOEXW info = {}; info.cbSize = sizeof(info);
    if (!GetMonitorInfoW(handle, &info)) return TRUE;
    std::wstring label = info.szDevice;
    label += L" (" + std::to_wstring(info.rcMonitor.right - info.rcMonitor.left) + L" x " +
        std::to_wstring(info.rcMonitor.bottom - info.rcMonitor.top) + L")";
    if (info.dwFlags & MONITORINFOF_PRIMARY) label += L" - principal";
    SendMessageW(monitorBox, CB_ADDSTRING, 0, (LPARAM)label.c_str());
    monitors.push_back(handle);
    if (info.dwFlags & MONITORINFOF_PRIMARY) SendMessageW(monitorBox, CB_SETCURSEL, monitors.size() - 1, 0);
    return TRUE;
}
static LRESULT CALLBACK WindowProc(HWND hwnd, UINT message, WPARAM wp, LPARAM lp) {
    switch (message) {
    case WM_COMMAND:
        switch (LOWORD(wp)) {
        case ID_START: Start(); break;
        case ID_STOP: Stop(); break;
        case ID_COPY: CopyLink(); break;
        case ID_OPEN: Show(); break;
        case ID_EXIT: quitting = true; Stop(); DestroyWindow(window); break;
        }
        return 0;
    case WM_TRAY:
        if (lp == WM_LBUTTONUP) Show();
        if (lp == WM_RBUTTONUP) {
            HMENU menu = CreatePopupMenu();
            AppendMenuW(menu, MF_STRING, ID_OPEN, L"Abrir Telinha");
            AppendMenuW(menu, MF_STRING | (child ? MF_GRAYED : 0), ID_START, L"Iniciar compartilhamento");
            AppendMenuW(menu, MF_STRING | (child ? 0 : MF_GRAYED), ID_STOP, L"Parar compartilhamento");
            AppendMenuW(menu, MF_STRING | (link.empty() ? MF_GRAYED : 0), ID_COPY, L"Copiar link");
            AppendMenuW(menu, MF_SEPARATOR, 0, NULL); AppendMenuW(menu, MF_STRING, ID_EXIT, L"Sair");
            POINT point; GetCursorPos(&point); SetForegroundWindow(hwnd);
            TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, NULL); DestroyMenu(menu);
        }
        return 0;
    case WM_LINE: {
        std::vector<std::wstring> pending;
        { std::lock_guard<std::mutex> lock(linesMutex); pending.swap(lines); }
        for (const auto& line : pending) {
            Append(line);
            const std::wstring prefix = L"Assista em: ";
            if (child && line.compare(0, prefix.size(), prefix) == 0) {
                link = line.substr(prefix.size()); SetWindowTextW(linkBox, link.c_str());
                EnableWindow(copyButton, TRUE); CopyLink(); Status(L"Telinha - compartilhando (link copiado)");
            }
        }
        return 0;
    }
    case WM_TIMER:
        if (WaitForSingleObject(wakeEvent, 0) == WAIT_OBJECT_0) Show();
        if (child && WaitForSingleObject(child, 0) == WAIT_OBJECT_0) {
            DWORD code = 0; GetExitCodeProcess(child, &code); CloseChild();
            if (code) { Show(); MessageBoxW(window, L"A transmissao parou. Veja os detalhes na janela abaixo.", L"Telinha", MB_ICONERROR); }
        }
        return 0;
    case WM_CLOSE:
        if (!quitting) { ShowWindow(hwnd, SW_HIDE); return 0; }
        break;
    case WM_QUERYENDSESSION: Stop(); return TRUE;
    case WM_DESTROY: Shell_NotifyIconW(NIM_DELETE, &tray); PostQuitMessage(0); return 0;
    default:
        if (message == RegisterWindowMessageW(L"TaskbarCreated")) Shell_NotifyIconW(NIM_ADD, &tray);
    }
    return DefWindowProcW(hwnd, message, wp, lp);
}
int WINAPI wWinMain(HINSTANCE instance, HINSTANCE, PWSTR command, int) {
    SetProcessDPIAware();
    HANDLE singleton = CreateMutexW(NULL, FALSE, L"Local\\TelinhaDesktop");
    bool alreadyOpen = GetLastError() == ERROR_ALREADY_EXISTS;
    wakeEvent = CreateEventW(NULL, FALSE, FALSE, L"Local\\TelinhaShow");
    if (alreadyOpen) { SetEvent(wakeEvent); CloseHandle(wakeEvent); CloseHandle(singleton); return 0; }
    wchar_t path[32768]; GetModuleFileNameW(NULL, path, 32768);
    folder = path; folder.resize(folder.find_last_of(L"\\/"));
    // Ambiente privado do app: não altera o PATH nem o GStreamer do sistema.
    DWORD pathSize = GetEnvironmentVariableW(L"PATH", NULL, 0);
    std::wstring oldPath(pathSize, 0);
    if (pathSize) { GetEnvironmentVariableW(L"PATH", &oldPath[0], pathSize); oldPath.resize(pathSize - 1); }
    std::wstring appPath = folder + L";" + folder + L"\\gstreamer\\bin;" + oldPath;
    SetEnvironmentVariableW(L"PATH", appPath.c_str());
    SetEnvironmentVariableW(L"GST_PLUGIN_PATH_1_0", (folder + L"\\gstreamer\\lib\\gstreamer-1.0").c_str());
    SetEnvironmentVariableW(L"GST_PLUGIN_SYSTEM_PATH_1_0", (folder + L"\\gstreamer\\lib\\gstreamer-1.0").c_str());
    SetEnvironmentVariableW(L"GST_PLUGIN_SCANNER_1_0", (folder + L"\\gstreamer\\libexec\\gstreamer-1.0\\gst-plugin-scanner.exe").c_str());
    WNDCLASSW cls = {}; cls.lpfnWndProc = WindowProc; cls.hInstance = instance;
    cls.lpszClassName = L"TelinhaDesktop"; cls.hCursor = LoadCursor(NULL, IDC_ARROW);
    cls.hIcon = LoadIcon(NULL, IDI_APPLICATION); cls.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    RegisterClassW(&cls);
    window = CreateWindowExW(0, cls.lpszClassName, L"Telinha", WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        CW_USEDEFAULT, CW_USEDEFAULT, 590, 560, NULL, NULL, instance, NULL);
    font = CreateFontW(-16, 0, 0, 0, FW_NORMAL, FALSE, FALSE, FALSE, DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, DEFAULT_PITCH, L"Segoe UI");
    Control(L"STATIC", L"Compartilhe sua tela", 0, 20, 16, 540, 28);
    Control(L"STATIC", L"Monitor", 0, 20, 54, 100, 24);
    monitorBox = Control(L"COMBOBOX", L"", CBS_DROPDOWNLIST | WS_TABSTOP | WS_VSCROLL, 120, 50, 430, 180, ID_MONITOR);
    EnumDisplayMonitors(NULL, NULL, AddMonitor, 0);
    audioBox = Control(L"BUTTON", L"Som dos aplicativos (sem apps de chamada)", BS_AUTOCHECKBOX | WS_TABSTOP, 20, 90, 520, 26, ID_AUDIO);
    SendMessageW(audioBox, BM_SETCHECK, BST_CHECKED, 0);
    publicBox = Control(L"BUTTON", L"Link publico - quem tiver o link pode assistir", BS_AUTOCHECKBOX | WS_TABSTOP, 20, 122, 530, 26, ID_PUBLIC);
    SendMessageW(publicBox, BM_SETCHECK, BST_CHECKED, 0);
    Control(L"STATIC", L"Desmarque para usar o Tailscale (precisa estar instalado e conectado).", 0, 20, 154, 540, 22);
    Control(L"STATIC", L"Excluir outros apps do som (separados por virgula)", 0, 20, 184, 540, 24);
    excludeBox = Control(L"EDIT", L"", WS_BORDER | ES_AUTOHSCROLL | WS_TABSTOP, 20, 210, 530, 26, ID_EXCLUDE);
    startButton = Control(L"BUTTON", L"Iniciar", WS_TABSTOP | BS_DEFPUSHBUTTON, 20, 252, 120, 34, ID_START);
    stopButton = Control(L"BUTTON", L"Parar", WS_TABSTOP, 150, 252, 120, 34, ID_STOP);
    copyButton = Control(L"BUTTON", L"Copiar link", WS_TABSTOP, 400, 252, 150, 34, ID_COPY);
    EnableWindow(stopButton, FALSE); EnableWindow(copyButton, FALSE);
    linkBox = Control(L"EDIT", L"", ES_READONLY | ES_AUTOHSCROLL | WS_BORDER | WS_TABSTOP, 20, 302, 530, 28);
    statusLabel = Control(L"STATIC", L"Telinha - parado", 0, 20, 342, 530, 24);
    logBox = Control(L"EDIT", L"", ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY | WS_VSCROLL | WS_BORDER, 20, 374, 530, 102, ID_LOG);
    Control(L"STATIC", L"Fechar esta janela mantem o Telinha perto do relogio. Use Sair para encerrar.", 0, 20, 488, 540, 22);
    tray.cbSize = sizeof(tray); tray.hWnd = window; tray.uID = 1;
    tray.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP; tray.uCallbackMessage = WM_TRAY;
    tray.hIcon = LoadIcon(NULL, IDI_APPLICATION); wcscpy_s(tray.szTip, L"Telinha - parado");
    Shell_NotifyIconW(NIM_ADD, &tray);
    SetTimer(window, 1, 250, NULL);
    if (std::wstring(command).find(L"--tray") == std::wstring::npos) Show();
    MSG msg;
    while (GetMessageW(&msg, NULL, 0, 0) > 0) {
        if (!IsDialogMessageW(window, &msg)) { TranslateMessage(&msg); DispatchMessageW(&msg); }
    }
    Stop(); DeleteObject(font); CloseHandle(wakeEvent); CloseHandle(singleton);
    return 0;
}
