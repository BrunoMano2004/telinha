// App do Windows: janela com a interface em HTML (windows/ui.html, embutida
// como recurso e mostrada num WebView2), ícone perto do relógio e o processo
// telinha.exe. A interface manda "comando\targ..." e recebe eventos JSON; o
// telinha.exe recebe comandos pelo stdin e responde com eventos "@..." na saída.
#ifndef UNICODE
#define UNICODE
#endif
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <shellapi.h>
#include <dwmapi.h>
#include <winhttp.h>
#include <bcrypt.h>
#include <wrl.h>
#include <WebView2.h>
#include <cwctype>
#include <functional>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

using Microsoft::WRL::Callback;
using Microsoft::WRL::ComPtr;

// Versão deste build (windows/build-ui.cmd passa a do Cargo.toml).
#ifndef TELINHA_VERSION
#define TELINHA_VERSION "0.0.0"
#endif
static const char* const repoReleases = "/repos/BrunoMano2004/telinha/releases/latest";

enum { ID_OPEN = 101, ID_START, ID_STOP, ID_COPY, ID_EXIT, WM_TRAY = WM_APP + 1, WM_LINE, WM_JSON, WM_INSTALL };
static HWND window;
static HANDLE child, inputPipe, job, wakeEvent;
static std::thread reader;
static std::wstring folder, link, iniPath;
static std::vector<HMONITOR> monitors;
static int primaryMonitor = 0;
static NOTIFYICONDATAW tray = {};
static bool quitting = false;
static std::mutex linesMutex;
static std::vector<std::wstring> lines;
static ComPtr<ICoreWebView2Controller> controller;
static ComPtr<ICoreWebView2> webview;
static HBRUSH background;
// Últimos eventos do telinha, reenviados se a interface recarregar.
static std::wstring lastViewers = L"[]", lastBlocked = L"[]", lastApps = L"[]";
// Escolhas de som por app (nome em minúsculas -> entra?), salvas no .ini.
static std::map<std::wstring, bool> appChoices;
// Última qualidade escolhida (para o "Iniciar" pela bandeja).
static int lastKbps = 12000, lastFps = 60;
// Mensagens para a interface vindas de outras threads (atualização).
static std::vector<std::wstring> pendingJson;
// Atualização disponível (preenchida pela verificação em segundo plano).
struct Release { std::wstring version, page, installer, checksum; };
static std::mutex updateMutex;
static Release available;
static bool downloading = false;
static std::wstring installerPath;

// --- Texto
static std::wstring Utf8(const std::string& text) {
    int size = MultiByteToWideChar(CP_UTF8, 0, text.data(), (int)text.size(), NULL, 0);
    std::wstring result(size, 0);
    if (size) MultiByteToWideChar(CP_UTF8, 0, text.data(), (int)text.size(), &result[0], size);
    return result;
}
static std::string ToUtf8(const std::wstring& text) {
    int size = WideCharToMultiByte(CP_UTF8, 0, text.data(), (int)text.size(), NULL, 0, NULL, NULL);
    std::string result(size, 0);
    if (size) WideCharToMultiByte(CP_UTF8, 0, text.data(), (int)text.size(), &result[0], size, NULL, NULL);
    return result;
}
static std::wstring Lower(std::wstring text) {
    for (auto& ch : text) ch = (wchar_t)towlower(ch);
    return text;
}
static std::vector<std::wstring> Split(const std::wstring& text, wchar_t separator) {
    std::vector<std::wstring> parts;
    size_t start = 0, end;
    while ((end = text.find(separator, start)) != std::wstring::npos) {
        parts.push_back(text.substr(start, end - start));
        start = end + 1;
    }
    parts.push_back(text.substr(start));
    return parts;
}
static std::wstring JsonText(const std::wstring& text) {
    std::wstring out = L"\"";
    for (wchar_t ch : text) {
        switch (ch) {
        case L'"': out += L"\\\""; break;
        case L'\\': out += L"\\\\"; break;
        case L'\n': out += L"\\n"; break;
        case L'\r': out += L"\\r"; break;
        case L'\t': out += L"\\t"; break;
        default:
            if (ch < 0x20) { wchar_t code[8]; swprintf_s(code, L"\\u%04x", (unsigned)ch); out += code; }
            else out += ch;
        }
    }
    return out + L"\"";
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

// --- Preferências em %APPDATA%\Telinha\telinha.ini
static int IniInt(const wchar_t* key, int fallback) {
    return GetPrivateProfileIntW(L"geral", key, fallback, iniPath.c_str());
}
static void IniSet(const wchar_t* section, const std::wstring& key, const std::wstring& value) {
    WritePrivateProfileStringW(section, key.c_str(), value.c_str(), iniPath.c_str());
}
static std::wstring KnownDir(const wchar_t* variable) {
    wchar_t value[MAX_PATH];
    DWORD size = GetEnvironmentVariableW(variable, value, MAX_PATH);
    std::wstring dir = (size && size < MAX_PATH) ? std::wstring(value) + L"\\Telinha" : folder;
    CreateDirectoryW(dir.c_str(), NULL);
    return dir;
}
static void LoadSettings() {
    iniPath = KnownDir(L"APPDATA") + L"\\telinha.ini";
    std::vector<wchar_t> buffer(32768);
    DWORD length = GetPrivateProfileSectionW(L"apps", buffer.data(), (DWORD)buffer.size(), iniPath.c_str());
    for (const wchar_t* entry = buffer.data(); entry < buffer.data() + length && *entry; entry += wcslen(entry) + 1) {
        std::wstring line = entry;
        size_t eq = line.find(L'=');
        if (eq != std::wstring::npos && eq > 0) appChoices[Lower(line.substr(0, eq))] = line.substr(eq + 1) == L"1";
    }
    static const int kbps[] = { 3000, 6000, 12000, 20000 }, fps[] = { 30, 60, 60, 60 };
    int quality = IniInt(L"qualidade", 2);
    if (quality >= 0 && quality < 4) { lastKbps = kbps[quality]; lastFps = fps[quality]; }
}

// --- Ponte com a interface
static void Post(const std::wstring& json) {
    if (webview) webview->PostWebMessageAsJson(json.c_str());
}
static void PostState() {
    Post(std::wstring(L"{\"type\":\"state\",\"running\":") + (child ? L"true" : L"false") +
        L",\"link\":" + JsonText(link) + L"}");
}
static void PostInit() {
    std::wstring list = L"[";
    for (size_t i = 0; i < monitors.size(); ++i) {
        MONITORINFO info = {}; info.cbSize = sizeof(info);
        GetMonitorInfoW(monitors[i], &info);
        std::wstring label = L"Monitor " + std::to_wstring(i + 1) + L" (" +
            std::to_wstring(info.rcMonitor.right - info.rcMonitor.left) + L" × " +
            std::to_wstring(info.rcMonitor.bottom - info.rcMonitor.top) + L")";
        if (i) list += L",";
        list += L"{\"label\":" + JsonText(label) + L",\"primary\":" + ((int)i == primaryMonitor ? L"true" : L"false") + L"}";
    }
    list += L"]";
    std::wstring apps = L"{";
    for (auto& choice : appChoices) {
        if (apps.size() > 1) apps += L",";
        apps += JsonText(choice.first) + L":" + (choice.second ? L"true" : L"false");
    }
    apps += L"}";
    int monitor = IniInt(L"monitor", primaryMonitor);
    if (monitor < 0 || monitor >= (int)monitors.size()) monitor = primaryMonitor;
    Post(L"{\"type\":\"init\",\"version\":" + JsonText(Utf8(TELINHA_VERSION)) + L",\"monitors\":" + list + L",\"monitor\":" + std::to_wstring(monitor) +
        L",\"quality\":" + std::to_wstring(IniInt(L"qualidade", 2)) +
        L",\"audio\":" + (IniInt(L"audio", 1) ? L"true" : L"false") +
        L",\"public\":" + (IniInt(L"publico", 1) ? L"true" : L"false") +
        L",\"apps\":" + apps + L"}");
    PostState();
    Post(L"{\"type\":\"viewers\",\"data\":" + lastViewers + L"}");
    Post(L"{\"type\":\"blocked\",\"data\":" + lastBlocked + L"}");
    Post(L"{\"type\":\"apps\",\"data\":" + lastApps + L"}");
    std::lock_guard<std::mutex> lock(updateMutex);
    if (!available.version.empty())
        Post(L"{\"type\":\"update\",\"version\":" + JsonText(available.version) + L",\"page\":" + JsonText(available.page) + L"}");
}

static void Status(const std::wstring& text) {
    wcsncpy_s(tray.szTip, text.c_str(), _TRUNCATE);
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
    Post(L"{\"type\":\"copied\"}");
}
static void Show() {
    ShowWindow(window, IsIconic(window) ? SW_RESTORE : SW_SHOW);
    SetForegroundWindow(window);
}
// Comando para o telinha.exe (uma linha no stdin dele).
static void Send(const std::wstring& command) {
    if (!child || !inputPipe) return;
    std::string data = ToUtf8(command) + "\n";
    DWORD written;
    WriteFile(inputPipe, data.data(), (DWORD)data.size(), &written, NULL);
}

// --- Processo do telinha
static void CloseChild() {
    if (inputPipe) { CloseHandle(inputPipe); inputPipe = NULL; }
    // Mate descendentes antes de aguardar EOF do pipe herdado.
    if (job) { CloseHandle(job); job = NULL; }
    if (reader.joinable()) reader.join();
    if (child) { CloseHandle(child); child = NULL; }
    { std::lock_guard<std::mutex> lock(linesMutex); lines.clear(); }
    link.clear();
    lastViewers = lastBlocked = lastApps = L"[]";
    PostState();
    Status(L"Telinha - parado");
}
static void Stop() {
    if (!child) return;
    Status(L"Telinha - encerrando...");
    Send(L"q");
    if (WaitForSingleObject(child, 4000) == WAIT_TIMEOUT && job) TerminateJobObject(job, 0);
    WaitForSingleObject(child, 2000);
    CloseChild();
}
static std::wstring ChoiceList(bool on) {
    std::wstring list;
    for (auto& choice : appChoices) {
        // Vírgula separaria a lista; aspas não sobrevivem ao quoting do argumento.
        if (choice.second != on || choice.first.find_first_of(L",\"") != std::wstring::npos) continue;
        if (!list.empty()) list += L",";
        list += choice.first;
    }
    return list;
}
static void Start(int monitor, int kbps, int fps, bool audio, bool isPublic) {
    if (child || monitors.empty()) return;
    if (monitor < 0 || monitor >= (int)monitors.size()) monitor = primaryMonitor;
    lastKbps = kbps; lastFps = fps;
    std::wstring args = Quote(folder + L"\\telinha.exe") + L" --monitor-handle " +
        std::to_wstring((unsigned long long)(UINT_PTR)monitors[monitor]) +
        L" --bitrate " + std::to_wstring(kbps) + L" --fps " + std::to_wstring(fps);
    if (audio) {
        args += L" --audio";
        std::wstring include = ChoiceList(true), exclude = ChoiceList(false);
        if (!include.empty()) args += L" --audio-include " + Quote(include);
        if (!exclude.empty()) args += L" --audio-exclude " + Quote(exclude);
    }
    if (isPublic) args += L" --tunnel";
    else args += L" --require-tailscale";
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
        MessageBoxW(window, L"Não foi possível iniciar o Telinha. Reinstale o aplicativo e tente novamente.", L"Telinha", MB_ICONERROR);
        return;
    }
    child = process.hProcess;
    PostState();
    Status(L"Telinha - preparando transmissão...");
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
// Uma linha da saída do telinha: evento "@..." vai para a interface; o resto, para o log.
static void HandleLine(const std::wstring& line) {
    auto event = [&](const wchar_t* name, std::wstring& data) {
        std::wstring prefix = std::wstring(L"@") + name + L" ";
        if (line.compare(0, prefix.size(), prefix) != 0) return false;
        data = line.substr(prefix.size());
        return true;
    };
    std::wstring data;
    if (event(L"viewers", data)) {
        lastViewers = data;
        Post(L"{\"type\":\"viewers\",\"data\":" + data + L"}");
        size_t count = 0;
        for (size_t at = data.find(L"\"id\":"); at != std::wstring::npos; at = data.find(L"\"id\":", at + 1)) ++count;
        if (!link.empty())
            Status(count ? L"Telinha - " + std::to_wstring(count) + (count == 1 ? L" pessoa assistindo" : L" pessoas assistindo")
                : L"Telinha - ao vivo, ninguém assistindo");
        return;
    }
    if (event(L"blocked", data)) { lastBlocked = data; Post(L"{\"type\":\"blocked\",\"data\":" + data + L"}"); return; }
    if (event(L"apps", data)) { lastApps = data; Post(L"{\"type\":\"apps\",\"data\":" + data + L"}"); return; }
    if (event(L"screen", data)) { Post(L"{\"type\":\"screen\",\"state\":" + JsonText(data) + L"}"); return; }
    Post(L"{\"type\":\"log\",\"text\":" + JsonText(line) + L"}");
    const std::wstring prefix = L"Assista em: ";
    if (child && line.compare(0, prefix.size(), prefix) == 0) {
        link = line.substr(prefix.size());
        PostState();
        CopyLink();
        Status(L"Telinha - ao vivo (link copiado)");
    }
}
// Mensagem da interface: "comando\targ\targ...".
static void HandleUi(const std::wstring& message) {
    std::vector<std::wstring> p = Split(message, L'\t');
    const std::wstring& cmd = p[0];
    auto arg = [&](size_t i) { return i < p.size() ? p[i] : std::wstring(); };
    if (cmd == L"ready") PostInit();
    else if (cmd == L"start") {
        IniSet(L"geral", L"monitor", arg(1));
        IniSet(L"geral", L"audio", arg(4));
        IniSet(L"geral", L"publico", arg(5));
        Start(_wtoi(arg(1).c_str()), _wtoi(arg(2).c_str()), _wtoi(arg(3).c_str()), arg(4) == L"1", arg(5) == L"1");
    }
    else if (cmd == L"stop") Stop();
    else if (cmd == L"copy") CopyLink();
    else if (cmd == L"open" && !link.empty()) ShellExecuteW(window, L"open", link.c_str(), NULL, NULL, SW_SHOWNORMAL);
    else if (cmd == L"send") Send(arg(1));
    else if (cmd == L"screen") {
        int monitor = _wtoi(arg(1).c_str());
        if (monitor >= 0 && monitor < (int)monitors.size())
            Send(L"screen " + std::to_wstring((unsigned long long)(UINT_PTR)monitors[monitor]));
    }
    else if (cmd == L"app" && !arg(1).empty()) {
        bool on = arg(2) == L"1";
        appChoices[Lower(arg(1))] = on;
        IniSet(L"apps", Lower(arg(1)), on ? L"1" : L"0");
        Send(std::wstring(L"app ") + (on ? L"on " : L"off ") + arg(1));
    }
    else if (cmd == L"update") DownloadUpdate();
    else if (cmd == L"release") {
        std::lock_guard<std::mutex> lock(updateMutex);
        if (!available.page.empty()) ShellExecuteW(window, L"open", available.page.c_str(), NULL, NULL, SW_SHOWNORMAL);
    }
    else if (cmd == L"save" && (arg(1) == L"monitor" || arg(1) == L"qualidade" || arg(1) == L"audio" || arg(1) == L"publico")) {
        IniSet(L"geral", arg(1), arg(2));
        if (arg(1) == L"qualidade") LoadSettings();
    }
}

// --- Atualização pelo GitHub Releases
// Só versões estáveis (releases/latest ignora pré-releases). O instalador é
// conferido pelo SHA-256 publicado junto e roda em modo silencioso.
static void PostFromThread(const std::wstring& json) {
    { std::lock_guard<std::mutex> lock(linesMutex); pendingJson.push_back(json); }
    PostMessageW(window, WM_JSON, 0, 0);
}
// GET https seguindo redirecionamentos; onData recebe os pedaços e o tamanho total (0 se desconhecido).
static bool HttpGet(const std::wstring& url, const wchar_t* accept,
    const std::function<bool(const char*, DWORD, ULONGLONG)>& onData) {
    wchar_t host[256] = {}, path[4096] = {}, extra[4096] = {};
    URL_COMPONENTS parts = {}; parts.dwStructSize = sizeof(parts);
    parts.lpszHostName = host; parts.dwHostNameLength = 256;
    parts.lpszUrlPath = path; parts.dwUrlPathLength = 4096;
    parts.lpszExtraInfo = extra; parts.dwExtraInfoLength = 4096;
    if (!WinHttpCrackUrl(url.c_str(), 0, 0, &parts) || parts.nScheme != INTERNET_SCHEME_HTTPS) return false;
    bool ok = false;
    HINTERNET session = WinHttpOpen(L"Telinha-Updater", WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
        WINHTTP_NO_PROXY_NAME, WINHTTP_NO_PROXY_BYPASS, 0);
    HINTERNET connection = session ? WinHttpConnect(session, host, parts.nPort, 0) : NULL;
    std::wstring target = std::wstring(path) + extra;
    HINTERNET request = connection ? WinHttpOpenRequest(connection, L"GET", target.c_str(), NULL,
        WINHTTP_NO_REFERER, WINHTTP_DEFAULT_ACCEPT_TYPES, WINHTTP_FLAG_SECURE) : NULL;
    std::wstring headers = std::wstring(L"Accept: ") + accept;
    if (request && WinHttpSendRequest(request, headers.c_str(), (DWORD)-1L, WINHTTP_NO_REQUEST_DATA, 0, 0, 0) &&
        WinHttpReceiveResponse(request, NULL)) {
        DWORD status = 0, size = sizeof(status);
        WinHttpQueryHeaders(request, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            WINHTTP_HEADER_NAME_BY_INDEX, &status, &size, WINHTTP_NO_HEADER_INDEX);
        wchar_t length[32] = {}; size = sizeof(length);
        ULONGLONG total = WinHttpQueryHeaders(request, WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_HEADER_NAME_BY_INDEX,
            length, &size, WINHTTP_NO_HEADER_INDEX) ? _wcstoui64(length, NULL, 10) : 0;
        if (status == 200) {
            ok = true;
            std::vector<char> buffer(65536);
            DWORD read = 0;
            while (WinHttpReadData(request, buffer.data(), (DWORD)buffer.size(), &read) && read) {
                if (!onData(buffer.data(), read, total)) { ok = false; break; }
            }
            if (read) ok = false;
        }
    }
    if (request) WinHttpCloseHandle(request);
    if (connection) WinHttpCloseHandle(connection);
    if (session) WinHttpCloseHandle(session);
    return ok;
}
static bool HttpText(const std::wstring& url, const wchar_t* accept, std::string& text) {
    text.clear();
    return HttpGet(url, accept, [&](const char* data, DWORD size, ULONGLONG) {
        text.append(data, size);
        return text.size() < 4 * 1024 * 1024;
    });
}
// Valores de uma chave de texto num JSON simples (sem escapes nos valores que usamos).
static std::vector<std::string> JsonValues(const std::string& json, const std::string& key) {
    std::vector<std::string> values;
    std::string needle = "\"" + key + "\":";
    for (size_t at = json.find(needle); at != std::string::npos; at = json.find(needle, at + 1)) {
        size_t start = json.find('"', at + needle.size());
        size_t end = start == std::string::npos ? start : json.find('"', start + 1);
        if (end != std::string::npos) values.push_back(json.substr(start + 1, end - start - 1));
    }
    return values;
}
// "v1.2.3" > "1.2.0"? Partes numéricas; um sufixo "-algo" é ignorado.
static bool Newer(const std::string& candidate, const std::string& current) {
    auto parse = [](std::string v) {
        if (!v.empty() && (v[0] == 'v' || v[0] == 'V')) v.erase(0, 1);
        v = v.substr(0, v.find('-'));
        std::vector<int> parts;
        for (auto& part : Split(Utf8(v), L'.')) parts.push_back(_wtoi(part.c_str()));
        while (parts.size() < 3) parts.push_back(0);
        return parts;
    };
    return parse(candidate) > parse(current);
}
static void CheckForUpdate() {
    std::string json;
    if (!HttpText(L"https://api.github.com" + Utf8(repoReleases), L"application/vnd.github+json", json)) return;
    auto tags = JsonValues(json, "tag_name");
    if (tags.empty() || !Newer(tags[0], TELINHA_VERSION)) return;
    Release release;
    release.version = Utf8(tags[0][0] == 'v' ? tags[0].substr(1) : tags[0]);
    auto pages = JsonValues(json, "html_url");
    if (!pages.empty()) release.page = Utf8(pages[0]);
    for (auto& url : JsonValues(json, "browser_download_url")) {
        auto ends = [&](const char* suffix) {
            std::string s = suffix;
            return url.size() >= s.size() && url.compare(url.size() - s.size(), s.size(), s) == 0;
        };
        if (ends("-windows-x64.exe")) release.installer = Utf8(url);
        if (ends("-windows-x64.exe.sha256")) release.checksum = Utf8(url);
    }
    if (release.installer.empty() || release.checksum.empty()) return;
    bool isNew;
    {
        std::lock_guard<std::mutex> lock(updateMutex);
        isNew = available.version != release.version;
        available = release;
    }
    PostFromThread(L"{\"type\":\"update\",\"version\":" + JsonText(release.version) + L",\"page\":" + JsonText(release.page) + L"}");
    if (isNew) {
        NOTIFYICONDATAW balloon = tray;
        balloon.uFlags = NIF_INFO;
        wcscpy_s(balloon.szInfoTitle, L"Nova versão do Telinha");
        std::wstring text = L"A versão " + release.version + L" está disponível. Abra o Telinha para atualizar.";
        wcsncpy_s(balloon.szInfo, text.c_str(), _TRUNCATE);
        balloon.dwInfoFlags = NIIF_INFO;
        Shell_NotifyIconW(NIM_MODIFY, &balloon);
    }
}
static void UpdateError(const std::wstring& text) {
    { std::lock_guard<std::mutex> lock(updateMutex); downloading = false; }
    PostFromThread(L"{\"type\":\"update-error\",\"text\":" + JsonText(text) + L"}");
}
// Baixa o instalador para %TEMP%, confere o SHA-256 e pede para a janela instalar.
static void DownloadUpdate() {
    Release release;
    {
        std::lock_guard<std::mutex> lock(updateMutex);
        if (downloading || available.installer.empty()) return;
        downloading = true;
        release = available;
    }
    std::thread([release]() {
        std::string checksum;
        if (!HttpText(release.checksum, L"application/octet-stream", checksum) || checksum.size() < 64)
            return UpdateError(L"Não foi possível baixar o checksum da nova versão.");
        std::string expected = checksum.substr(0, 64);
        for (auto& ch : expected) ch = (char)tolower((unsigned char)ch);

        wchar_t temp[MAX_PATH];
        GetTempPathW(MAX_PATH, temp);
        std::wstring path = std::wstring(temp) + L"Telinha-Setup-" + release.version + L".exe";
        HANDLE file = CreateFileW(path.c_str(), GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
        if (file == INVALID_HANDLE_VALUE) return UpdateError(L"Não foi possível salvar o instalador.");
        BCRYPT_ALG_HANDLE algorithm = NULL;
        BCRYPT_HASH_HANDLE hash = NULL;
        bool hashing = BCRYPT_SUCCESS(BCryptOpenAlgorithmProvider(&algorithm, BCRYPT_SHA256_ALGORITHM, NULL, 0)) &&
            BCRYPT_SUCCESS(BCryptCreateHash(algorithm, &hash, NULL, 0, NULL, 0, 0));
        ULONGLONG received = 0;
        int lastPercent = -1;
        bool ok = hashing && HttpGet(release.installer, L"application/octet-stream",
            [&](const char* data, DWORD size, ULONGLONG total) {
                DWORD written = 0;
                if (!WriteFile(file, data, size, &written, NULL) || written != size) return false;
                BCryptHashData(hash, (PUCHAR)data, size, 0);
                received += size;
                int percent = total ? (int)(received * 100 / total) : -1;
                if (percent != lastPercent) {
                    lastPercent = percent;
                    PostFromThread(L"{\"type\":\"update-progress\",\"percent\":" + std::to_wstring(percent) + L"}");
                }
                return true;
            });
        CloseHandle(file);
        unsigned char digest[32] = {};
        if (hashing) BCryptFinishHash(hash, digest, sizeof(digest), 0);
        if (hash) BCryptDestroyHash(hash);
        if (algorithm) BCryptCloseAlgorithmProvider(algorithm, 0);
        if (!ok) { DeleteFileW(path.c_str()); return UpdateError(L"O download da nova versão falhou. Tente de novo."); }
        char hex[65] = {};
        for (int i = 0; i < 32; ++i) sprintf_s(hex + i * 2, 3, "%02x", digest[i]);
        if (expected != hex) {
            DeleteFileW(path.c_str());
            return UpdateError(L"O instalador baixado não confere com o checksum publicado. Nada foi instalado.");
        }
        { std::lock_guard<std::mutex> lock(updateMutex); installerPath = path; }
        PostMessageW(window, WM_INSTALL, 0, 0);
    }).detach();
}
// Verifica 5 s depois de abrir e depois a cada 6 horas.
static void StartUpdateChecks() {
    std::thread([]() {
        Sleep(5000);
        for (;;) {
            CheckForUpdate();
            Sleep(6 * 60 * 60 * 1000);
        }
    }).detach();
}

// --- Janela e tema
static bool DarkMode() {
    DWORD light = 1, size = sizeof(light);
    RegGetValueW(HKEY_CURRENT_USER, L"Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
        L"AppsUseLightTheme", RRF_RT_REG_DWORD, NULL, &light, &size);
    return light == 0;
}
// Barra de título e fundo da mesma cor da interface (#1b1b1f / #f3f3f7).
static void ApplyTheme() {
    bool dark = DarkMode();
    BOOL useDark = dark;
    DwmSetWindowAttribute(window, 20 /* DWMWA_USE_IMMERSIVE_DARK_MODE */, &useDark, sizeof(useDark));
    COLORREF caption = dark ? RGB(0x1b, 0x1b, 0x1f) : RGB(0xf3, 0xf3, 0xf7);
    DwmSetWindowAttribute(window, 35 /* DWMWA_CAPTION_COLOR */, &caption, sizeof(caption));
    if (background) DeleteObject(background);
    background = CreateSolidBrush(caption);
    SetClassLongPtrW(window, GCLP_HBRBACKGROUND, (LONG_PTR)background);
    ComPtr<ICoreWebView2Controller2> controller2;
    if (controller && SUCCEEDED(controller.As(&controller2))) {
        COREWEBVIEW2_COLOR color = { 255, GetRValue(caption), GetGValue(caption), GetBValue(caption) };
        controller2->put_DefaultBackgroundColor(color);
    }
    InvalidateRect(window, NULL, TRUE);
}
static void Resize() {
    if (!controller) return;
    RECT bounds; GetClientRect(window, &bounds);
    controller->put_Bounds(bounds);
}
static std::wstring LoadUi() {
    HRSRC resource = FindResourceW(NULL, MAKEINTRESOURCEW(2), RT_RCDATA);
    HGLOBAL data = resource ? LoadResource(NULL, resource) : NULL;
    if (!data) return L"<p>Interface ausente. Reinstale o Telinha.</p>";
    return Utf8(std::string((const char*)LockResource(data), SizeofResource(NULL, resource)));
}
static void WebViewFailed(HRESULT result) {
    wchar_t text[400];
    swprintf_s(text, L"A interface do Telinha precisa do Microsoft Edge WebView2 Runtime (erro 0x%08lX).\n\n"
        L"Abrir a página de download agora?", (unsigned long)result);
    if (MessageBoxW(window, text, L"Telinha", MB_ICONWARNING | MB_YESNO) == IDYES)
        ShellExecuteW(window, L"open", L"https://go.microsoft.com/fwlink/p/?LinkId=2124703", NULL, NULL, SW_SHOWNORMAL);
}
static bool LocalUri(ICoreWebView2* sender, LPWSTR uri) {
    (void)sender;
    std::wstring value = uri ? uri : L"";
    return value == L"about:blank" || value.rfind(L"data:", 0) == 0;
}
static void CreateWebView() {
    std::wstring dataDir = KnownDir(L"LOCALAPPDATA") + L"\\WebView2";
    HRESULT started = CreateCoreWebView2EnvironmentWithOptions(NULL, dataDir.c_str(), NULL,
        Callback<ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler>(
            [](HRESULT result, ICoreWebView2Environment* environment) -> HRESULT {
                if (FAILED(result) || !environment) { WebViewFailed(result); return S_OK; }
                return environment->CreateCoreWebView2Controller(window,
                    Callback<ICoreWebView2CreateCoreWebView2ControllerCompletedHandler>(
                        [](HRESULT result, ICoreWebView2Controller* created) -> HRESULT {
                            if (FAILED(result) || !created) { WebViewFailed(result); return S_OK; }
                            controller = created;
                            controller->get_CoreWebView2(&webview);
                            ApplyTheme();
                            ComPtr<ICoreWebView2Settings> settings;
                            webview->get_Settings(&settings);
                            settings->put_AreDevToolsEnabled(FALSE);
                            settings->put_AreDefaultContextMenusEnabled(FALSE);
                            settings->put_IsStatusBarEnabled(FALSE);
                            settings->put_IsZoomControlEnabled(FALSE);
                            ComPtr<ICoreWebView2Settings3> settings3;
                            if (SUCCEEDED(settings.As(&settings3))) settings3->put_AreBrowserAcceleratorKeysEnabled(FALSE);
                            EventRegistrationToken token;
                            webview->add_WebMessageReceived(Callback<ICoreWebView2WebMessageReceivedEventHandler>(
                                [](ICoreWebView2*, ICoreWebView2WebMessageReceivedEventArgs* args) -> HRESULT {
                                    LPWSTR message = NULL;
                                    if (SUCCEEDED(args->TryGetWebMessageAsString(&message)) && message) {
                                        HandleUi(message);
                                        CoTaskMemFree(message);
                                    }
                                    return S_OK;
                                }).Get(), &token);
                            // A interface é local: qualquer outro endereço abre no navegador.
                            webview->add_NavigationStarting(Callback<ICoreWebView2NavigationStartingEventHandler>(
                                [](ICoreWebView2* sender, ICoreWebView2NavigationStartingEventArgs* args) -> HRESULT {
                                    LPWSTR uri = NULL;
                                    args->get_Uri(&uri);
                                    if (!LocalUri(sender, uri)) {
                                        args->put_Cancel(TRUE);
                                        if (uri) ShellExecuteW(window, L"open", uri, NULL, NULL, SW_SHOWNORMAL);
                                    }
                                    if (uri) CoTaskMemFree(uri);
                                    return S_OK;
                                }).Get(), &token);
                            webview->add_NewWindowRequested(Callback<ICoreWebView2NewWindowRequestedEventHandler>(
                                [](ICoreWebView2*, ICoreWebView2NewWindowRequestedEventArgs* args) -> HRESULT {
                                    LPWSTR uri = NULL;
                                    args->get_Uri(&uri);
                                    args->put_Handled(TRUE);
                                    if (uri) { ShellExecuteW(window, L"open", uri, NULL, NULL, SW_SHOWNORMAL); CoTaskMemFree(uri); }
                                    return S_OK;
                                }).Get(), &token);
                            Resize();
                            webview->NavigateToString(LoadUi().c_str());
                            return S_OK;
                        }).Get());
            }).Get());
    if (FAILED(started)) WebViewFailed(started);
}
static BOOL CALLBACK AddMonitor(HMONITOR handle, HDC, LPRECT, LPARAM) {
    MONITORINFO info = {}; info.cbSize = sizeof(info);
    if (!GetMonitorInfoW(handle, &info)) return TRUE;
    if (info.dwFlags & MONITORINFOF_PRIMARY) primaryMonitor = (int)monitors.size();
    monitors.push_back(handle);
    return TRUE;
}
static int Scale(int value, HWND hwnd) { return MulDiv(value, (int)GetDpiForWindow(hwnd), 96); }

static LRESULT CALLBACK WindowProc(HWND hwnd, UINT message, WPARAM wp, LPARAM lp) {
    switch (message) {
    case WM_COMMAND:
        switch (LOWORD(wp)) {
        case ID_OPEN: Show(); break;
        case ID_START: {
            int monitor = IniInt(L"monitor", primaryMonitor);
            Start(monitor, lastKbps, lastFps, IniInt(L"audio", 1) != 0, IniInt(L"publico", 1) != 0);
            break;
        }
        case ID_STOP: Stop(); break;
        case ID_COPY: CopyLink(); break;
        case ID_EXIT: quitting = true; Stop(); DestroyWindow(window); break;
        }
        return 0;
    case WM_TRAY:
        if (lp == WM_LBUTTONUP) Show();
        if (lp == WM_RBUTTONUP) {
            HMENU menu = CreatePopupMenu();
            AppendMenuW(menu, MF_STRING, ID_OPEN, L"Abrir Telinha");
            AppendMenuW(menu, MF_STRING | (child ? MF_GRAYED : 0), ID_START, L"Iniciar transmissão");
            AppendMenuW(menu, MF_STRING | (child ? 0 : MF_GRAYED), ID_STOP, L"Parar transmissão");
            AppendMenuW(menu, MF_STRING | (link.empty() ? MF_GRAYED : 0), ID_COPY, L"Copiar link");
            AppendMenuW(menu, MF_SEPARATOR, 0, NULL); AppendMenuW(menu, MF_STRING, ID_EXIT, L"Sair");
            POINT point; GetCursorPos(&point); SetForegroundWindow(hwnd);
            TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, NULL); DestroyMenu(menu);
        }
        return 0;
    case WM_LINE: {
        std::vector<std::wstring> pending;
        { std::lock_guard<std::mutex> lock(linesMutex); pending.swap(lines); }
        for (const auto& line : pending) HandleLine(line);
        return 0;
    }
    case WM_JSON: {
        std::vector<std::wstring> pending;
        { std::lock_guard<std::mutex> lock(linesMutex); pending.swap(pendingJson); }
        for (const auto& json : pending) Post(json);
        return 0;
    }
    case WM_INSTALL: {
        // O instalador fecha esta janela e a reabre no fim (installer.iss).
        std::wstring path;
        { std::lock_guard<std::mutex> lock(updateMutex); path = installerPath; }
        quitting = true;
        Stop();
        if ((INT_PTR)ShellExecuteW(NULL, L"open", path.c_str(), L"/SILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS",
                NULL, SW_SHOWNORMAL) <= 32) {
            quitting = false;
            UpdateError(L"Não foi possível abrir o instalador.");
            return 0;
        }
        DestroyWindow(window);
        return 0;
    }
    case WM_TIMER:
        if (WaitForSingleObject(wakeEvent, 0) == WAIT_OBJECT_0) Show();
        if (child && WaitForSingleObject(child, 0) == WAIT_OBJECT_0) {
            DWORD code = 0; GetExitCodeProcess(child, &code); CloseChild();
            if (code) {
                Show();
                MessageBoxW(window, L"A transmissão parou. Veja os detalhes técnicos no fim da janela.", L"Telinha", MB_ICONERROR);
            }
        }
        return 0;
    case WM_SIZE:
        Resize();
        if (controller) controller->put_IsVisible(wp != SIZE_MINIMIZED);
        return 0;
    case WM_DPICHANGED: {
        RECT* suggested = (RECT*)lp;
        SetWindowPos(hwnd, NULL, suggested->left, suggested->top, suggested->right - suggested->left,
            suggested->bottom - suggested->top, SWP_NOZORDER | SWP_NOACTIVATE);
        return 0;
    }
    case WM_GETMINMAXINFO: {
        MINMAXINFO* info = (MINMAXINFO*)lp;
        info->ptMinTrackSize.x = Scale(560, hwnd);
        info->ptMinTrackSize.y = Scale(520, hwnd);
        return 0;
    }
    case WM_SETTINGCHANGE:
        if (lp && wcscmp((const wchar_t*)lp, L"ImmersiveColorSet") == 0) ApplyTheme();
        return 0;
    case WM_CLOSE:
        if (!quitting) { ShowWindow(hwnd, SW_HIDE); return 0; }
        break;
    case WM_QUERYENDSESSION: Stop(); return TRUE;
    case WM_DESTROY:
        Shell_NotifyIconW(NIM_DELETE, &tray);
        controller = nullptr; webview = nullptr;
        PostQuitMessage(0);
        return 0;
    default:
        if (message == RegisterWindowMessageW(L"TaskbarCreated")) Shell_NotifyIconW(NIM_ADD, &tray);
    }
    return DefWindowProcW(hwnd, message, wp, lp);
}

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE, PWSTR command, int) {
    // DPI por monitor vem do manifesto (windows/telinha.manifest).
    HANDLE singleton = CreateMutexW(NULL, FALSE, L"Local\\TelinhaDesktop");
    bool alreadyOpen = GetLastError() == ERROR_ALREADY_EXISTS;
    wakeEvent = CreateEventW(NULL, FALSE, FALSE, L"Local\\TelinhaShow");
    if (alreadyOpen) { SetEvent(wakeEvent); CloseHandle(wakeEvent); CloseHandle(singleton); return 0; }
    CoInitializeEx(NULL, COINIT_APARTMENTTHREADED);
    wchar_t path[32768]; GetModuleFileNameW(NULL, path, 32768);
    folder = path; folder.resize(folder.find_last_of(L"\\/"));
    LoadSettings();
    EnumDisplayMonitors(NULL, NULL, AddMonitor, 0);
    // Ambiente privado do app: não altera o PATH nem o GStreamer do sistema.
    DWORD pathSize = GetEnvironmentVariableW(L"PATH", NULL, 0);
    std::wstring oldPath(pathSize, 0);
    if (pathSize) { GetEnvironmentVariableW(L"PATH", &oldPath[0], pathSize); oldPath.resize(pathSize - 1); }
    std::wstring appPath = folder + L";" + folder + L"\\gstreamer\\bin;" + oldPath;
    SetEnvironmentVariableW(L"PATH", appPath.c_str());
    SetEnvironmentVariableW(L"GST_PLUGIN_PATH_1_0", (folder + L"\\gstreamer\\lib\\gstreamer-1.0").c_str());
    SetEnvironmentVariableW(L"GST_PLUGIN_SYSTEM_PATH_1_0", (folder + L"\\gstreamer\\lib\\gstreamer-1.0").c_str());
    SetEnvironmentVariableW(L"GST_PLUGIN_SCANNER_1_0", (folder + L"\\gstreamer\\libexec\\gstreamer-1.0\\gst-plugin-scanner.exe").c_str());

    // Ícone do recurso 1 (windows/telinha.rc), na janela e perto do relógio.
    HICON bigIcon = LoadIconW(instance, MAKEINTRESOURCEW(1));
    HICON smallIcon = (HICON)LoadImageW(instance, MAKEINTRESOURCEW(1), IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR);
    if (!bigIcon) bigIcon = LoadIcon(NULL, IDI_APPLICATION);
    if (!smallIcon) smallIcon = bigIcon;
    WNDCLASSEXW cls = {}; cls.cbSize = sizeof(cls); cls.lpfnWndProc = WindowProc; cls.hInstance = instance;
    cls.lpszClassName = L"TelinhaDesktop"; cls.hCursor = LoadCursor(NULL, IDC_ARROW);
    cls.hIcon = bigIcon; cls.hIconSm = smallIcon;
    RegisterClassExW(&cls);
    window = CreateWindowExW(0, cls.lpszClassName, L"Telinha", WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT, CW_USEDEFAULT, 1000, 760, NULL, NULL, instance, NULL);
    // Tamanho inicial em pixels lógicos, convertido para a escala do monitor.
    SetWindowPos(window, NULL, 0, 0, Scale(1000, window), Scale(760, window), SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    ApplyTheme();
    CreateWebView();

    tray.cbSize = sizeof(tray); tray.hWnd = window; tray.uID = 1;
    tray.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP; tray.uCallbackMessage = WM_TRAY;
    tray.hIcon = smallIcon; wcscpy_s(tray.szTip, L"Telinha - parado");
    Shell_NotifyIconW(NIM_ADD, &tray);
    SetTimer(window, 1, 250, NULL);
    StartUpdateChecks();
    if (std::wstring(command).find(L"--tray") == std::wstring::npos) Show();
    MSG msg;
    while (GetMessageW(&msg, NULL, 0, 0) > 0) { TranslateMessage(&msg); DispatchMessageW(&msg); }
    Stop();
    if (background) DeleteObject(background);
    CloseHandle(wakeEvent); CloseHandle(singleton);
    CoUninitialize();
    return 0;
}
