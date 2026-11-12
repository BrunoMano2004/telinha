// Interface Win32 sem runtime adicional: janela, bandeja e processo servidor.
// Conversa com o telinha.exe pelo stdin (comandos) e pela saída (eventos "@...").
#ifndef UNICODE
#define UNICODE
#endif
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <shellapi.h>
#include <commctrl.h>
#include <algorithm>
#include <cwctype>
#include <map>
#include <string>
#include <vector>
#include <mutex>
#include <thread>

enum { ID_START = 101, ID_STOP, ID_COPY, ID_MONITOR, ID_QUALITY, ID_AUDIO, ID_PUBLIC,
       ID_OPEN, ID_EXIT, ID_LOG, ID_VIEWERS, ID_KICK, ID_BLOCK, ID_BLOCKED, ID_UNBLOCK, ID_APPS,
       WM_TRAY = WM_APP + 1, WM_LINE };
static HWND window, startButton, stopButton, copyButton, monitorBox, qualityBox, audioBox,
    publicBox, linkBox, statusLabel, logBox, viewersList, kickButton, blockButton,
    blockedList, unblockButton, appsList;
static HANDLE child, inputPipe, job, wakeEvent;
static std::thread reader;
static std::wstring folder, link, iniPath;
static std::vector<HMONITOR> monitors;
static NOTIFYICONDATAW tray = {};
static bool quitting = false;
static HFONT font;
static std::mutex linesMutex;
static std::vector<std::wstring> lines;

// Qualidade: o bitrate é o upload de cada espectador. Muda ao vivo.
struct Quality { const wchar_t* label; int kbps; int fps; };
static const Quality qualities[] = {
    { L"Leve - 3 Mbps, 30 fps (conexões lentas)", 3000, 30 },
    { L"Equilibrada - 6 Mbps, 60 fps", 6000, 60 },
    { L"Alta - 12 Mbps, 60 fps (texto nítido)", 12000, 60 },
    { L"Máxima - 20 Mbps, 60 fps (upload rápido)", 20000, 60 },
};

// Mesma lista do telinha (src/audio_rules.rs): fora por padrão, para não dar eco.
static const wchar_t* voiceApps[] = { L"discord", L"vesktop", L"webcord", L"legcord", L"armcord",
    L"equibop", L"webrtc", L"teamspeak", L"ts3client", L"mumble", L"zoom", L"slack", L"teams",
    L"skype", L"telegram", L"signal", L"whatsapp", L"element", L"jitsi" };

// Apps: escolhas salvas (nome em minúsculas -> entra?) e os que tocam som agora.
struct AppItem { std::wstring name; bool voice; };
static std::map<std::wstring, bool> appChoices;
static std::vector<AppItem> liveApps, appItems;
static bool updatingApps = false;

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
static bool IsVoice(const std::wstring& name) {
    std::wstring lower = Lower(name);
    for (auto app : voiceApps) if (lower.find(app) != std::wstring::npos) return true;
    return false;
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

// --- JSON mínimo: só o formato dos eventos, [{"chave": "texto" | número | bool}, ...]
typedef std::map<std::wstring, std::wstring> JsonObject;
static bool JsonString(const std::wstring& s, size_t& i, std::wstring& out) {
    if (i >= s.size() || s[i] != L'"') return false;
    for (++i; i < s.size(); ++i) {
        wchar_t ch = s[i];
        if (ch == L'"') { ++i; return true; }
        if (ch != L'\\') { out += ch; continue; }
        if (++i >= s.size()) return false;
        switch (s[i]) {
        case L'n': out += L'\n'; break;
        case L't': out += L'\t'; break;
        case L'r': out += L'\r'; break;
        case L'b': out += L'\b'; break;
        case L'f': out += L'\f'; break;
        case L'u':
            if (i + 4 >= s.size()) return false;
            out += (wchar_t)wcstoul(s.substr(i + 1, 4).c_str(), NULL, 16);
            i += 4;
            break;
        default: out += s[i];
        }
    }
    return false;
}
static std::vector<JsonObject> JsonArray(const std::wstring& s) {
    std::vector<JsonObject> result;
    size_t i = 0;
    auto space = [&]() { while (i < s.size() && iswspace(s[i])) ++i; };
    space();
    if (i >= s.size() || s[i] != L'[') return result;
    ++i;
    while (true) {
        space();
        if (i >= s.size() || s[i] == L']') break;
        if (s[i] == L',') { ++i; continue; }
        if (s[i] != L'{') break;
        ++i;
        JsonObject object;
        while (true) {
            space();
            if (i >= s.size()) return result;
            if (s[i] == L'}') { ++i; break; }
            if (s[i] == L',') { ++i; continue; }
            std::wstring key, value;
            if (!JsonString(s, i, key)) return result;
            space();
            if (i >= s.size() || s[i] != L':') return result;
            ++i; space();
            if (i < s.size() && s[i] == L'"') { if (!JsonString(s, i, value)) return result; }
            else while (i < s.size() && s[i] != L',' && s[i] != L'}' && !iswspace(s[i])) value += s[i++];
            object[key] = value;
        }
        result.push_back(object);
    }
    return result;
}

// --- Preferências em %APPDATA%\Telinha\telinha.ini
static int IniInt(const wchar_t* key, int fallback) {
    return GetPrivateProfileIntW(L"geral", key, fallback, iniPath.c_str());
}
static void IniSet(const wchar_t* section, const std::wstring& key, const std::wstring& value) {
    WritePrivateProfileStringW(section, key.c_str(), value.c_str(), iniPath.c_str());
}
static void LoadSettings() {
    wchar_t appData[MAX_PATH];
    DWORD size = GetEnvironmentVariableW(L"APPDATA", appData, MAX_PATH);
    std::wstring dir = (size && size < MAX_PATH) ? std::wstring(appData) + L"\\Telinha" : folder;
    CreateDirectoryW(dir.c_str(), NULL);
    iniPath = dir + L"\\telinha.ini";
    std::vector<wchar_t> buffer(32768);
    DWORD length = GetPrivateProfileSectionW(L"apps", buffer.data(), (DWORD)buffer.size(), iniPath.c_str());
    for (const wchar_t* entry = buffer.data(); entry < buffer.data() + length && *entry; entry += wcslen(entry) + 1) {
        std::wstring line = entry;
        size_t eq = line.find(L'=');
        if (eq != std::wstring::npos && eq > 0) appChoices[Lower(line.substr(0, eq))] = line.substr(eq + 1) == L"1";
    }
}

static std::wstring Text(HWND control) {
    int size = GetWindowTextLengthW(control);
    std::wstring value(size + 1, 0);
    GetWindowTextW(control, &value[0], size + 1);
    value.resize(size);
    return value;
}
static void Status(const std::wstring& text) {
    SetWindowTextW(statusLabel, text.c_str());
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
}
static void Show() { ShowWindow(window, SW_SHOWNORMAL); SetForegroundWindow(window); }
static void Append(const std::wstring& line) {
    // Limitado à memória desta sessão; o link privado não vai para logs em disco.
    if (GetWindowTextLengthW(logBox) > 50000) SetWindowTextW(logBox, L"");
    SendMessageW(logBox, EM_SETSEL, (WPARAM)-1, (LPARAM)-1);
    std::wstring text = line + L"\r\n";
    SendMessageW(logBox, EM_REPLACESEL, FALSE, (LPARAM)text.c_str());
}
// Comando para o telinha.exe (uma linha no stdin dele).
static void Send(const std::wstring& command) {
    if (!child || !inputPipe) return;
    std::string data = ToUtf8(command) + "\n";
    DWORD written;
    WriteFile(inputPipe, data.data(), (DWORD)data.size(), &written, NULL);
}

// --- Listas (ListView em modo relatório)
static void ListRow(HWND list, int row, const std::vector<std::wstring>& cells, LPARAM data) {
    LVITEMW item = {};
    item.mask = LVIF_TEXT | LVIF_PARAM;
    item.iItem = row;
    item.lParam = data;
    item.pszText = (LPWSTR)cells[0].c_str();
    int index = (int)SendMessageW(list, LVM_INSERTITEMW, 0, (LPARAM)&item);
    for (size_t col = 1; col < cells.size(); ++col) {
        LVITEMW sub = {};
        sub.iSubItem = (int)col;
        sub.pszText = (LPWSTR)cells[col].c_str();
        SendMessageW(list, LVM_SETITEMTEXTW, index, (LPARAM)&sub);
    }
}
static LPARAM ListSelected(HWND list) {
    int index = (int)SendMessageW(list, LVM_GETNEXTITEM, (WPARAM)-1, LVNI_SELECTED);
    if (index < 0) return -1;
    LVITEMW item = {};
    item.mask = LVIF_PARAM;
    item.iItem = index;
    SendMessageW(list, LVM_GETITEMW, 0, (LPARAM)&item);
    return item.lParam;
}
static void ActionButtons() {
    bool live = child != NULL;
    EnableWindow(kickButton, live && ListSelected(viewersList) >= 0);
    EnableWindow(blockButton, live && ListSelected(viewersList) >= 0);
    EnableWindow(unblockButton, live && ListSelected(blockedList) >= 0);
}
static void ShowViewers(const std::vector<JsonObject>& viewers) {
    SendMessageW(viewersList, LVM_DELETEALLITEMS, 0, 0);
    int row = 0;
    for (auto& v : viewers) {
        ListRow(viewersList, row++, { v.count(L"name") ? v.at(L"name") : L"?",
            v.count(L"device") ? v.at(L"device") : L"", v.count(L"ip") ? v.at(L"ip") : L"" },
            (LPARAM)_wtoi64(v.count(L"id") ? v.at(L"id").c_str() : L"-1"));
    }
    if (child && !link.empty()) {
        Status(viewers.empty() ? L"Telinha - compartilhando, ninguém assistindo"
            : L"Telinha - " + std::to_wstring(viewers.size()) + (viewers.size() == 1 ? L" pessoa assistindo" : L" pessoas assistindo"));
    }
    ActionButtons();
}
static void ShowBlocked(const std::vector<JsonObject>& blocked) {
    SendMessageW(blockedList, LVM_DELETEALLITEMS, 0, 0);
    int row = 0;
    for (auto& b : blocked) {
        ListRow(blockedList, row++, { b.count(L"name") ? b.at(L"name") : L"?",
            b.count(L"device") ? b.at(L"device") : L"", b.count(L"ip") ? b.at(L"ip") : L"" },
            (LPARAM)_wtoi64(b.count(L"key") ? b.at(L"key").c_str() : L"-1"));
    }
    ActionButtons();
}
// Apps tocando som agora + os que já têm escolha salva, com caixa de seleção.
static void ShowApps() {
    std::map<std::wstring, AppItem> merged;
    for (auto& app : liveApps) merged.emplace(Lower(app.name), app);
    for (auto& choice : appChoices) merged.emplace(choice.first, AppItem{ choice.first, IsVoice(choice.first) });
    appItems.clear();
    for (auto& entry : merged) appItems.push_back(entry.second);
    updatingApps = true;
    SendMessageW(appsList, LVM_DELETEALLITEMS, 0, 0);
    for (size_t i = 0; i < appItems.size(); ++i) {
        const AppItem& app = appItems[i];
        ListRow(appsList, (int)i, { app.name, app.voice ? L"chamada de voz" : L"" }, (LPARAM)i);
        auto choice = appChoices.find(Lower(app.name));
        bool on = choice != appChoices.end() ? choice->second : !app.voice;
        ListView_SetCheckState(appsList, (int)i, on);
    }
    updatingApps = false;
}
static void AppToggled(int index) {
    if (index < 0 || index >= (int)appItems.size()) return;
    const std::wstring& name = appItems[index].name;
    bool on = ListView_GetCheckState(appsList, index) != FALSE;
    appChoices[Lower(name)] = on;
    IniSet(L"apps", Lower(name), on ? L"1" : L"0");
    Send(std::wstring(L"app ") + (on ? L"on " : L"off ") + name);
}

static void Controls(bool running) {
    EnableWindow(startButton, !running); EnableWindow(stopButton, running);
    // Monitor e qualidade continuam editáveis: mudam ao vivo.
    EnableWindow(audioBox, !running); EnableWindow(publicBox, !running);
    ActionButtons();
}
static void CloseChild() {
    if (inputPipe) { CloseHandle(inputPipe); inputPipe = NULL; }
    // Mate descendentes antes de aguardar EOF do pipe herdado.
    if (job) { CloseHandle(job); job = NULL; }
    if (reader.joinable()) reader.join();
    if (child) { CloseHandle(child); child = NULL; }
    { std::lock_guard<std::mutex> lock(linesMutex); lines.clear(); }
    link.clear(); SetWindowTextW(linkBox, L""); EnableWindow(copyButton, FALSE);
    SendMessageW(viewersList, LVM_DELETEALLITEMS, 0, 0);
    SendMessageW(blockedList, LVM_DELETEALLITEMS, 0, 0);
    liveApps.clear(); ShowApps();
    Controls(false); Status(L"Telinha - parado");
}
static void Stop() {
    if (!child) return;
    Status(L"Telinha - encerrando...");
    Send(L"q");
    if (WaitForSingleObject(child, 4000) == WAIT_TIMEOUT && job) TerminateJobObject(job, 0);
    WaitForSingleObject(child, 2000);
    CloseChild();
}
static const Quality& SelectedQuality() {
    int index = (int)SendMessageW(qualityBox, CB_GETCURSEL, 0, 0);
    if (index < 0 || index >= (int)(sizeof(qualities) / sizeof(qualities[0]))) index = 2;
    return qualities[index];
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
static void Start() {
    if (child) return;
    int selected = (int)SendMessageW(monitorBox, CB_GETCURSEL, 0, 0);
    if (selected < 0 || selected >= (int)monitors.size()) return;
    const Quality& quality = SelectedQuality();
    std::wstring args = Quote(folder + L"\\telinha.exe") + L" --monitor-handle " +
        std::to_wstring((unsigned long long)(UINT_PTR)monitors[selected]) +
        L" --bitrate " + std::to_wstring(quality.kbps) + L" --fps " + std::to_wstring(quality.fps);
    bool audio = SendMessageW(audioBox, BM_GETCHECK, 0, 0) == BST_CHECKED;
    bool isPublic = SendMessageW(publicBox, BM_GETCHECK, 0, 0) == BST_CHECKED;
    IniSet(L"geral", L"audio", audio ? L"1" : L"0");
    IniSet(L"geral", L"publico", isPublic ? L"1" : L"0");
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
    Controls(true); SetWindowTextW(logBox, L""); Status(L"Telinha - preparando transmissão...");
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
// Uma linha da saída do telinha: evento "@..." ou texto para o log.
static void HandleLine(const std::wstring& line) {
    auto event = [&](const wchar_t* name) {
        std::wstring prefix = std::wstring(L"@") + name + L" ";
        return line.compare(0, prefix.size(), prefix) == 0 ? line.substr(prefix.size()) : std::wstring();
    };
    std::wstring data;
    if (!(data = event(L"viewers")).empty()) { ShowViewers(JsonArray(data)); return; }
    if (!(data = event(L"blocked")).empty()) { ShowBlocked(JsonArray(data)); return; }
    if (!(data = event(L"apps")).empty()) {
        liveApps.clear();
        for (auto& app : JsonArray(data)) {
            if (!app.count(L"name")) continue;
            liveApps.push_back({ app.at(L"name"), app.count(L"voice") && app.at(L"voice") == L"true" });
        }
        ShowApps();
        return;
    }
    if (!(data = event(L"screen")).empty()) {
        if (data == L"switched") Append(L"Monitor trocado.");
        return;
    }
    Append(line);
    const std::wstring prefix = L"Assista em: ";
    if (child && line.compare(0, prefix.size(), prefix) == 0) {
        link = line.substr(prefix.size()); SetWindowTextW(linkBox, link.c_str());
        EnableWindow(copyButton, TRUE); CopyLink(); Status(L"Telinha - compartilhando (link copiado)");
    }
}

static HWND Control(const wchar_t* type, const wchar_t* text, DWORD style, int x, int y, int w, int h, int id = 0) {
    HWND item = CreateWindowExW(0, type, text, WS_CHILD | WS_VISIBLE | style, x, y, w, h,
        window, (HMENU)(INT_PTR)id, GetModuleHandleW(NULL), NULL);
    SendMessageW(item, WM_SETFONT, (WPARAM)font, TRUE);
    return item;
}
static HWND List(int x, int y, int w, int h, int id, const std::vector<std::pair<const wchar_t*, int>>& columns, bool checks) {
    HWND list = Control(WC_LISTVIEWW, L"", WS_BORDER | WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOSORTHEADER, x, y, w, h, id);
    SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, LVS_EX_FULLROWSELECT | (checks ? LVS_EX_CHECKBOXES : 0));
    for (size_t i = 0; i < columns.size(); ++i) {
        LVCOLUMNW column = {};
        column.mask = LVCF_TEXT | LVCF_WIDTH;
        column.pszText = (LPWSTR)columns[i].first;
        column.cx = columns[i].second;
        SendMessageW(list, LVM_INSERTCOLUMNW, i, (LPARAM)&column);
    }
    return list;
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
        case ID_KICK: { LPARAM id = ListSelected(viewersList); if (id >= 0) Send(L"kick " + std::to_wstring(id)); break; }
        case ID_BLOCK: { LPARAM id = ListSelected(viewersList); if (id >= 0) Send(L"block " + std::to_wstring(id)); break; }
        case ID_UNBLOCK: { LPARAM key = ListSelected(blockedList); if (key >= 0) Send(L"unblock " + std::to_wstring(key)); break; }
        case ID_MONITOR:
            // Com a transmissão rodando, troca o monitor sem derrubar ninguém.
            if (HIWORD(wp) == CBN_SELCHANGE && child) {
                int selected = (int)SendMessageW(monitorBox, CB_GETCURSEL, 0, 0);
                if (selected >= 0 && selected < (int)monitors.size())
                    Send(L"screen " + std::to_wstring((unsigned long long)(UINT_PTR)monitors[selected]));
            }
            break;
        case ID_QUALITY:
            if (HIWORD(wp) == CBN_SELCHANGE) {
                IniSet(L"geral", L"qualidade", std::to_wstring(SendMessageW(qualityBox, CB_GETCURSEL, 0, 0)));
                const Quality& quality = SelectedQuality();
                Send(L"quality " + std::to_wstring(quality.kbps) + L" " + std::to_wstring(quality.fps));
            }
            break;
        }
        return 0;
    case WM_NOTIFY: {
        NMHDR* header = (NMHDR*)lp;
        if (header->code == LVN_ITEMCHANGED) {
            NMLISTVIEW* change = (NMLISTVIEW*)lp;
            // Caixa de seleção marcada/desmarcada pela pessoa (não pela montagem da lista).
            if (header->idFrom == ID_APPS && !updatingApps && (change->uChanged & LVIF_STATE) &&
                (change->uOldState & LVIS_STATEIMAGEMASK) &&
                ((change->uOldState ^ change->uNewState) & LVIS_STATEIMAGEMASK))
                AppToggled((int)change->lParam);
            ActionButtons();
        }
        return 0;
    }
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
        for (const auto& line : pending) HandleLine(line);
        return 0;
    }
    case WM_TIMER:
        if (WaitForSingleObject(wakeEvent, 0) == WAIT_OBJECT_0) Show();
        if (child && WaitForSingleObject(child, 0) == WAIT_OBJECT_0) {
            DWORD code = 0; GetExitCodeProcess(child, &code); CloseChild();
            if (code) { Show(); MessageBoxW(window, L"A transmissão parou. Veja os detalhes na janela.", L"Telinha", MB_ICONERROR); }
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
    INITCOMMONCONTROLSEX controls = { sizeof(controls), ICC_LISTVIEW_CLASSES };
    InitCommonControlsEx(&controls);
    HANDLE singleton = CreateMutexW(NULL, FALSE, L"Local\\TelinhaDesktop");
    bool alreadyOpen = GetLastError() == ERROR_ALREADY_EXISTS;
    wakeEvent = CreateEventW(NULL, FALSE, FALSE, L"Local\\TelinhaShow");
    if (alreadyOpen) { SetEvent(wakeEvent); CloseHandle(wakeEvent); CloseHandle(singleton); return 0; }
    wchar_t path[32768]; GetModuleFileNameW(NULL, path, 32768);
    folder = path; folder.resize(folder.find_last_of(L"\\/"));
    LoadSettings();
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
    cls.hIcon = bigIcon; cls.hIconSm = smallIcon; cls.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    RegisterClassExW(&cls);
    window = CreateWindowExW(0, cls.lpszClassName, L"Telinha", WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        CW_USEDEFAULT, CW_USEDEFAULT, 990, 640, NULL, NULL, instance, NULL);
    font = CreateFontW(-16, 0, 0, 0, FW_NORMAL, FALSE, FALSE, FALSE, DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, DEFAULT_PITCH, L"Segoe UI");

    // Coluna da esquerda: transmissão.
    Control(L"STATIC", L"Compartilhe sua tela", 0, 20, 16, 450, 28);
    Control(L"STATIC", L"Monitor", 0, 20, 56, 100, 24);
    monitorBox = Control(L"COMBOBOX", L"", CBS_DROPDOWNLIST | WS_TABSTOP | WS_VSCROLL, 120, 52, 350, 200, ID_MONITOR);
    EnumDisplayMonitors(NULL, NULL, AddMonitor, 0);
    Control(L"STATIC", L"Qualidade", 0, 20, 92, 100, 24);
    qualityBox = Control(L"COMBOBOX", L"", CBS_DROPDOWNLIST | WS_TABSTOP | WS_VSCROLL, 120, 88, 350, 200, ID_QUALITY);
    for (auto& quality : qualities) SendMessageW(qualityBox, CB_ADDSTRING, 0, (LPARAM)quality.label);
    int quality = IniInt(L"qualidade", 2);
    SendMessageW(qualityBox, CB_SETCURSEL, (quality >= 0 && quality < 4) ? quality : 2, 0);
    audioBox = Control(L"BUTTON", L"Som dos aplicativos (nunca o microfone)", BS_AUTOCHECKBOX | WS_TABSTOP, 20, 128, 450, 26, ID_AUDIO);
    SendMessageW(audioBox, BM_SETCHECK, IniInt(L"audio", 1) ? BST_CHECKED : BST_UNCHECKED, 0);
    publicBox = Control(L"BUTTON", L"Link público - quem tiver o link pode assistir", BS_AUTOCHECKBOX | WS_TABSTOP, 20, 158, 450, 26, ID_PUBLIC);
    SendMessageW(publicBox, BM_SETCHECK, IniInt(L"publico", 1) ? BST_CHECKED : BST_UNCHECKED, 0);
    Control(L"STATIC", L"Desmarque para usar o Tailscale (precisa estar instalado e conectado).", 0, 20, 188, 450, 40);
    startButton = Control(L"BUTTON", L"Iniciar", WS_TABSTOP | BS_DEFPUSHBUTTON, 20, 236, 110, 34, ID_START);
    stopButton = Control(L"BUTTON", L"Parar", WS_TABSTOP, 140, 236, 110, 34, ID_STOP);
    copyButton = Control(L"BUTTON", L"Copiar link", WS_TABSTOP, 320, 236, 150, 34, ID_COPY);
    EnableWindow(stopButton, FALSE); EnableWindow(copyButton, FALSE);
    linkBox = Control(L"EDIT", L"", ES_READONLY | ES_AUTOHSCROLL | WS_BORDER | WS_TABSTOP, 20, 282, 450, 28);
    statusLabel = Control(L"STATIC", L"Telinha - parado", 0, 20, 320, 450, 24);
    logBox = Control(L"EDIT", L"", ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY | WS_VSCROLL | WS_BORDER, 20, 352, 450, 190, ID_LOG);
    Control(L"STATIC", L"Fechar esta janela mantém o Telinha perto do relógio. Use Sair para encerrar.", 0, 20, 552, 450, 40);

    // Coluna da direita: quem assiste e o som.
    Control(L"STATIC", L"Espectadores", 0, 500, 16, 450, 24);
    viewersList = List(500, 42, 450, 140, ID_VIEWERS, { { L"Nome", 150 }, { L"Aparelho", 160 }, { L"IP", 130 } }, false);
    kickButton = Control(L"BUTTON", L"Expulsar", WS_TABSTOP, 500, 188, 110, 30, ID_KICK);
    blockButton = Control(L"BUTTON", L"Bloquear", WS_TABSTOP, 620, 188, 110, 30, ID_BLOCK);
    Control(L"STATIC", L"Bloqueados (navegador e IP)", 0, 500, 232, 450, 24);
    blockedList = List(500, 258, 450, 80, ID_BLOCKED, { { L"Nome", 150 }, { L"Aparelho", 160 }, { L"IP", 130 } }, false);
    unblockButton = Control(L"BUTTON", L"Desbloquear", WS_TABSTOP, 500, 344, 130, 30, ID_UNBLOCK);
    Control(L"STATIC", L"Apps no som (marque o que entra; vale na hora)", 0, 500, 388, 450, 24);
    appsList = List(500, 414, 450, 128, ID_APPS, { { L"Aplicativo", 280 }, { L"", 160 } }, true);
    Control(L"STATIC", L"Apps de chamada começam desmarcados para a chamada não dar eco.", 0, 500, 552, 450, 40);
    ShowApps();
    Controls(false);

    tray.cbSize = sizeof(tray); tray.hWnd = window; tray.uID = 1;
    tray.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP; tray.uCallbackMessage = WM_TRAY;
    tray.hIcon = smallIcon; wcscpy_s(tray.szTip, L"Telinha - parado");
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
