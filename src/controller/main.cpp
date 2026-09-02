#include "fsw/path_resolver.hpp"
#include "fsw/wsl_registry.hpp"
#include "fsw_user_protocol.h"

#include <windows.h>
#include <fltuser.h>
#include <shellapi.h>

#include "fsw_filter_protocol.h"

#include <filesystem>
#include <iostream>
#include <string>
#include <string_view>
#include <vector>

namespace {

constexpr wchar_t kRunKey[] =
    L"Software\\Microsoft\\Windows\\CurrentVersion\\Run";
constexpr wchar_t kRunValue[] = L"ForwardSlashWindows";

std::filesystem::path ExecutableDirectory() {
  std::wstring buffer(32768, L'\0');
  const DWORD length = GetModuleFileNameW(nullptr, buffer.data(),
                                          static_cast<DWORD>(buffer.size()));
  buffer.resize(length);
  return std::filesystem::path(buffer).parent_path();
}

std::wstring Quote(const std::filesystem::path& path) {
  return L"\"" + path.wstring() + L"\"";
}

bool IsBrokerRunning() {
  return FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr) != nullptr;
}

bool IsDriverAvailable() {
  HANDLE port = INVALID_HANDLE_VALUE;
  const HRESULT result = FilterConnectCommunicationPort(
      FSW_FILTER_PORT_NAME, 0, nullptr, 0, nullptr, &port);
  if (FAILED(result)) {
    return false;
  }
  CloseHandle(port);
  return true;
}

FSW_BROKER_STATE BrokerState() {
  const HWND window = FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr);
  if (window == nullptr) {
    return FswBrokerUnavailable;
  }
  DWORD_PTR result = 0;
  if (SendMessageTimeoutW(window, FSW_WM_QUERY_STATE, 0, 0,
                          SMTO_ABORTIFHUNG | SMTO_BLOCK, 1000, &result) == 0) {
    return FswBrokerUnavailable;
  }
  return static_cast<FSW_BROKER_STATE>(result);
}

int StartBroker() {
  if (IsBrokerRunning()) {
    if (BrokerState() == FswBrokerActive) {
      std::wcout << L"Forward Slash Windows broker is already active.\n";
      return 0;
    }
    std::wcerr << L"Broker is running but its keyboard hook is unavailable.\n";
    return 1;
  }
  const std::filesystem::path broker = ExecutableDirectory() / L"fswbroker.exe";
  std::wstring command = Quote(broker);
  STARTUPINFOW startup{sizeof(startup)};
  PROCESS_INFORMATION process{};
  if (!CreateProcessW(broker.c_str(), command.data(), nullptr, nullptr, FALSE,
                      CREATE_NEW_PROCESS_GROUP, nullptr, broker.parent_path().c_str(),
                      &startup, &process)) {
    std::wcerr << L"Unable to start broker. Win32 error " << GetLastError()
               << L".\n";
    return 1;
  }
  CloseHandle(process.hThread);
  const ULONGLONG deadline = GetTickCount64() + 5000;
  do {
    const FSW_BROKER_STATE state = BrokerState();
    if (state == FswBrokerActive) {
      CloseHandle(process.hProcess);
      std::wcout << L"Forward Slash Windows broker started and is active.\n";
      return 0;
    }
    if (WaitForSingleObject(process.hProcess, 0) == WAIT_OBJECT_0) {
      break;
    }
    Sleep(50);
  } while (GetTickCount64() < deadline);
  const HWND window = FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr);
  if (window != nullptr) {
    PostMessageW(window, WM_CLOSE, 0, 0);
  }
  WaitForSingleObject(process.hProcess, 2000);
  CloseHandle(process.hProcess);
  std::wcerr << L"Broker started but its keyboard hook is unavailable.\n";
  return 1;
}

int StopBroker() {
  const HWND window = FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr);
  if (window == nullptr) {
    std::wcout << L"Forward Slash Windows broker is not running.\n";
    return 0;
  }
  DWORD process_id = 0;
  GetWindowThreadProcessId(window, &process_id);
  HANDLE process =
      OpenProcess(SYNCHRONIZE, FALSE, process_id);
  PostMessageW(window, WM_CLOSE, 0, 0);
  if (process != nullptr) {
    const DWORD wait = WaitForSingleObject(process, 5000);
    CloseHandle(process);
    if (wait != WAIT_OBJECT_0) {
      std::wcerr << L"Broker did not stop within five seconds.\n";
      return 1;
    }
  } else {
    const ULONGLONG deadline = GetTickCount64() + 5000;
    while (FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr) != nullptr &&
           GetTickCount64() < deadline) {
      Sleep(50);
    }
    if (FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr) != nullptr) {
      std::wcerr << L"Broker did not stop within five seconds.\n";
      return 1;
    }
  }
  std::wcout << L"Forward Slash Windows broker stopped.\n";
  return 0;
}

int SetStartup(const bool enabled) {
  HKEY key = nullptr;
  const LSTATUS opened = RegCreateKeyExW(HKEY_CURRENT_USER, kRunKey, 0, nullptr,
                                          0, KEY_SET_VALUE, nullptr, &key,
                                          nullptr);
  if (opened != ERROR_SUCCESS) {
    std::wcerr << L"Unable to open the per-user startup key. Error " << opened
               << L".\n";
    return 1;
  }
  LSTATUS status = ERROR_SUCCESS;
  if (enabled) {
    const std::wstring value = Quote(ExecutableDirectory() / L"fswbroker.exe");
    status = RegSetValueExW(
        key, kRunValue, 0, REG_SZ, reinterpret_cast<const BYTE*>(value.c_str()),
        static_cast<DWORD>((value.size() + 1) * sizeof(wchar_t)));
  } else {
    status = RegDeleteValueW(key, kRunValue);
    if (status == ERROR_FILE_NOT_FOUND) {
      status = ERROR_SUCCESS;
    }
  }
  RegCloseKey(key);
  if (status != ERROR_SUCCESS) {
    std::wcerr << L"Unable to update startup registration. Error " << status
               << L".\n";
    return 1;
  }
  return 0;
}

std::wstring JsonEscape(const std::wstring_view input) {
  std::wstring output;
  for (const wchar_t character : input) {
    switch (character) {
      case L'\\':
        output.append(L"\\\\");
        break;
      case L'\"':
        output.append(L"\\\"");
        break;
      case L'\r':
        output.append(L"\\r");
        break;
      case L'\n':
        output.append(L"\\n");
        break;
      default:
        output.push_back(character);
        break;
    }
  }
  return output;
}

int Status(const bool json) {
  const auto distributions = fsw::ListRegisteredDistributions();
  const FSW_BROKER_STATE broker_state = BrokerState();
  const wchar_t* broker_status =
      !IsBrokerRunning() ? L"stopped"
      : broker_state == FswBrokerActive ? L"running (active)"
      : broker_state == FswBrokerPaused ? L"running (paused)"
                                       : L"running (hook unavailable)";
  if (json) {
    std::wcout << L"{\"broker\":\"" << broker_status
               << L"\",\"driverConnected\":"
               << (IsDriverAvailable() ? L"true" : L"false")
               << L",\"wslRoot\":\"\\\\\\\\wsl.localhost\","
                  L"\"distributions\":[";
    for (size_t index = 0; index < distributions.size(); ++index) {
      if (index != 0) {
        std::wcout << L',';
      }
      std::wcout << L"\"" << JsonEscape(distributions[index]) << L"\"";
    }
    std::wcout << L"]}\n";
    return 0;
  }
  std::wcout << L"broker: " << broker_status << L"\n";
  std::wcout << L"filesystem driver: "
             << (IsDriverAvailable() ? L"connected" : L"not connected")
             << L"\n";
  std::wcout << L"registered distributions: " << distributions.size() << L"\n";
  std::wcout << L"  / -> \\\\wsl.localhost\\ (distribution list)\n";
  for (const std::wstring& distribution : distributions) {
    std::wcout << L"  /" << distribution << L"/ -> \\\\wsl.localhost\\"
               << distribution << L"\\\n";
  }
  return 0;
}

int SetPaused(const bool paused) {
  const HWND window = FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr);
  if (window == nullptr) {
    std::wcerr << L"Forward Slash Windows broker is not running.\n";
    return 1;
  }
  DWORD_PTR result = 0;
  if (!SendMessageTimeoutW(window, FSW_WM_SET_PAUSED, paused ? 1 : 0, 0,
                           SMTO_ABORTIFHUNG | SMTO_BLOCK, 2000, &result) ||
      result == 0) {
    std::wcerr << L"The broker did not accept the state change.\n";
    return 1;
  }
  std::wcout << (paused ? L"Broker paused.\n" : L"Broker resumed.\n");
  return 0;
}

int ShowSettings() {
  const HWND window = FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr);
  if (window == nullptr) {
    std::wcerr << L"Forward Slash Windows broker is not running.\n";
    return 1;
  }
  PostMessageW(window, FSW_WM_SHOW_SETTINGS, 0, 0);
  return 0;
}

int Doctor(const std::wstring_view path) {
  const fsw::ResolveResult resolved = fsw::ResolveRegisteredSlashPath(path);
  if (!resolved.matched()) {
    std::wcerr << L"resolver: " << fsw::ResolveErrorName(resolved.error) << L"\n";
    return 1;
  }
  std::wcout << L"target kind: "
             << (resolved.is_wsl_root() ? L"WSL distribution list"
                                        : L"distribution path")
             << L"\n"
             << L"distribution: " << resolved.distribution << L"\n"
             << L"linux path: " << resolved.linux_path << L"\n"
             << L"windows target: " << resolved.unc_path << L"\n";
  if (resolved.is_wsl_root()) {
    const auto distributions = fsw::ListRegisteredDistributions();
    std::wcout << L"Shell namespace: "
               << (distributions.empty() ? L"no registered distributions"
                                         : L"available")
               << L"\n";
    return distributions.empty() ? 2 : 0;
  }
  const DWORD attributes = GetFileAttributesW(resolved.unc_path.c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) {
    std::wcout << L"target access: unavailable (Win32 error " << GetLastError()
               << L")\n";
    return 2;
  }
  std::wcout << L"target access: available\n";
  return 0;
}

int DoctorAll() {
  int outcome = Doctor(L"/");
  for (const std::wstring& distribution : fsw::ListRegisteredDistributions()) {
    const int result = Doctor(L"/" + distribution);
    if (result > outcome) {
      outcome = result;
    }
  }
  return outcome;
}

int Resolve(const std::wstring_view path) {
  const fsw::ResolveResult resolved = fsw::ResolveRegisteredSlashPath(path);
  if (!resolved.matched()) {
    std::wcerr << fsw::ResolveErrorMessage(
                       resolved.error, fsw::ListRegisteredDistributions())
               << L"\n";
    return 1;
  }
  std::wcout << resolved.unc_path << L"\n";
  return 0;
}

int Open(const std::wstring_view path) {
  const fsw::ResolveResult resolved = fsw::ResolveRegisteredSlashPath(path);
  if (!resolved.matched()) {
    std::wcerr << fsw::ResolveErrorMessage(
                       resolved.error, fsw::ListRegisteredDistributions())
               << L"\n";
    return 1;
  }
  SHELLEXECUTEINFOW execute{sizeof(execute)};
  execute.lpVerb = L"open";
  execute.lpFile = resolved.unc_path.c_str();
  execute.nShow = SW_SHOWNORMAL;
  if (!ShellExecuteExW(&execute)) {
    std::wcerr << L"Windows could not open the target. Error "
               << GetLastError() << L".\n";
    return 1;
  }
  return 0;
}

int List(const std::wstring_view path) {
  const fsw::ResolveResult resolved = fsw::ResolveRegisteredSlashPath(path);
  if (!resolved.matched()) {
    std::wcerr << fsw::ResolveErrorMessage(
                       resolved.error, fsw::ListRegisteredDistributions())
               << L"\n";
    return 1;
  }
  if (resolved.is_wsl_root()) {
    const auto distributions = fsw::ListRegisteredDistributions();
    for (const std::wstring& distribution : distributions) {
      std::wcout << L"[distro] /" << distribution << L"\n";
    }
    if (distributions.empty()) {
      std::wcout << L"No registered WSL distributions were found.\n";
    }
    return 0;
  }
  std::wstring pattern = resolved.unc_path;
  if (!pattern.empty() && pattern.back() != L'\\') {
    pattern.push_back(L'\\');
  }
  pattern.push_back(L'*');
  WIN32_FIND_DATAW entry{};
  const HANDLE search = FindFirstFileW(pattern.c_str(), &entry);
  if (search == INVALID_HANDLE_VALUE) {
    std::wcerr << L"Unable to enumerate " << resolved.unc_path
               << L". Error " << GetLastError() << L".\n";
    return 1;
  }
  do {
    const std::wstring_view name(entry.cFileName);
    if (name != L"." && name != L"..") {
      std::wcout << ((entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0
                         ? L"[dir]  "
                         : L"       ")
                 << name << L"\n";
    }
  } while (FindNextFileW(search, &entry));
  const DWORD final_error = GetLastError();
  FindClose(search);
  return final_error == ERROR_NO_MORE_FILES ? 0 : 1;
}

int CmdList(const std::wstring_view path) {
  const fsw::ResolveResult resolved = fsw::ResolveRegisteredSlashPath(path);
  if (!resolved.matched()) {
    // Exit 3 tells the batch adapter that this is a native DIR switch or path,
    // not one of our registered aliases. It deliberately emits no text.
    return 3;
  }
  return List(path);
}

void Usage() {
  std::wcout <<
      L"Forward Slash Windows controller\n\n"
      L"  fswctl status [--json]\n"
      L"  fswctl resolve /Distro/path\n"
      L"  fswctl open /Distro/path\n"
      L"  fswctl list /Distro/path\n"
      L"  fswctl doctor /Distro/path | --all\n"
      L"  fswctl settings\n"
      L"  fswctl pause | resume\n"
      L"  fswctl driver status\n"
      L"  fswctl start | stop\n"
      L"  fswctl install       Register and start the per-user broker\n"
      L"  fswctl uninstall     Stop and unregister the per-user broker\n\n"
      L"The optional filesystem driver is production-gated and is never "
      L"installed by these per-user commands.\n";
}

}  // namespace

int wmain(const int argc, wchar_t** argv) {
  if (argc < 2) {
    Usage();
    return 2;
  }
  const std::wstring_view command(argv[1]);
  if (command == L"status") {
    if (argc > 3 || (argc == 3 && std::wstring_view(argv[2]) != L"--json")) {
      Usage();
      return 2;
    }
    return Status(argc == 3);
  }
  if (command == L"resolve" && argc == 3) {
    return Resolve(argv[2]);
  }
  if (command == L"open" && argc == 3) {
    return Open(argv[2]);
  }
  if (command == L"list" && argc == 3) {
    return List(argv[2]);
  }
  if (command == L"cmd-list" && argc == 3) {
    return CmdList(argv[2]);
  }
  if (command == L"doctor") {
    if (argc != 3) {
      Usage();
      return 2;
    }
    return std::wstring_view(argv[2]) == L"--all" ? DoctorAll()
                                                   : Doctor(argv[2]);
  }
  if (command == L"settings" && argc == 2) {
    return ShowSettings();
  }
  if (command == L"pause" && argc == 2) {
    return SetPaused(true);
  }
  if (command == L"resume" && argc == 2) {
    return SetPaused(false);
  }
  if (command == L"driver" && argc == 3 &&
      std::wstring_view(argv[2]) == L"status") {
    std::wcout << (IsDriverAvailable() ? L"connected\n" : L"not connected\n");
    return IsDriverAvailable() ? 0 : 1;
  }
  if (command == L"start") {
    return StartBroker();
  }
  if (command == L"stop") {
    return StopBroker();
  }
  if (command == L"install") {
    const int registered = SetStartup(true);
    if (registered != 0) {
      return registered;
    }
    const int started = StartBroker();
    if (started != 0) {
      SetStartup(false);
    }
    return started;
  }
  if (command == L"uninstall") {
    const int stopped = StopBroker();
    const int unregistered = SetStartup(false);
    return stopped != 0 ? stopped : unregistered;
  }
  Usage();
  return 2;
}
