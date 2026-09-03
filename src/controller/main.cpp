#include "fsw/package_identity.hpp"
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
constexpr wchar_t kProtocolKey[] = L"Software\\Classes\\fwdslash";
constexpr wchar_t kCmdAdapterKey[] =
    L"Software\\ForwardSlashWindows\\CmdAdapter";
constexpr wchar_t kPowerShellAdapterRoot[] =
    L"Software\\ForwardSlashWindows\\PowerShellAdapter\\";

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

bool RegistryStringEquals(HKEY root, const std::wstring& path,
                          const wchar_t* name, const std::wstring& expected) {
  HKEY key = nullptr;
  if (RegOpenKeyExW(root, path.c_str(), 0, KEY_QUERY_VALUE, &key) !=
      ERROR_SUCCESS) {
    return false;
  }
  DWORD type = 0;
  DWORD bytes = 0;
  LSTATUS status = RegQueryValueExW(key, name, nullptr, &type, nullptr, &bytes);
  std::wstring value;
  if (status == ERROR_SUCCESS &&
      (type == REG_SZ || type == REG_EXPAND_SZ) && bytes >= sizeof(wchar_t)) {
    value.resize(bytes / sizeof(wchar_t));
    status = RegQueryValueExW(key, name, nullptr, &type,
                              reinterpret_cast<BYTE*>(value.data()), &bytes);
    while (!value.empty() && value.back() == L'\0') {
      value.pop_back();
    }
  }
  RegCloseKey(key);
  return status == ERROR_SUCCESS && value == expected;
}

bool RegistryStateInstalled(const std::wstring& path) {
  return RegistryStringEquals(HKEY_CURRENT_USER, path, L"State", L"installed");
}

bool IsWindowsIntegrationInstalled() {
  // A packaged build declares the startup task and the protocol handler in its
  // manifest, so they are present for as long as the package is installed.
  if (fsw::HasPackageIdentity()) {
    return true;
  }
  HKEY key = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, kRunKey, 0, KEY_QUERY_VALUE, &key) !=
      ERROR_SUCCESS) {
    return false;
  }
  const LSTATUS status = RegQueryValueExW(key, kRunValue, nullptr, nullptr,
                                          nullptr, nullptr);
  RegCloseKey(key);
  return status == ERROR_SUCCESS;
}

bool IsDisabled() {
  HKEY key = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, FSW_SETTINGS_KEY, 0, KEY_QUERY_VALUE,
                    &key) != ERROR_SUCCESS) {
    return false;
  }
  DWORD value = 0;
  DWORD type = 0;
  DWORD bytes = sizeof(value);
  const LSTATUS status = RegQueryValueExW(
      key, FSW_DISABLED_VALUE, nullptr, &type,
      reinterpret_cast<BYTE*>(&value), &bytes);
  RegCloseKey(key);
  return status == ERROR_SUCCESS && type == REG_DWORD && value != 0;
}

int PersistDisabled(const bool disabled) {
  HKEY key = nullptr;
  const LSTATUS opened = RegCreateKeyExW(HKEY_CURRENT_USER, FSW_SETTINGS_KEY,
                                          0, nullptr, 0, KEY_SET_VALUE,
                                          nullptr, &key, nullptr);
  if (opened != ERROR_SUCCESS) {
    std::wcerr << L"Unable to open the settings key. Error " << opened
               << L".\n";
    return 1;
  }
  const DWORD value = disabled ? 1U : 0U;
  const LSTATUS status = RegSetValueExW(
      key, FSW_DISABLED_VALUE, 0, REG_DWORD,
      reinterpret_cast<const BYTE*>(&value), sizeof(value));
  RegCloseKey(key);
  if (status != ERROR_SUCCESS) {
    std::wcerr << L"Unable to persist the disabled state. Error " << status
               << L".\n";
    return 1;
  }
  return 0;
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
    const FSW_BROKER_STATE state = BrokerState();
    if (state == FswBrokerActive) {
      std::wcout << L"Forward Slash Windows broker is already active.\n";
      return 0;
    }
    if (state == FswBrokerPaused) {
      DWORD_PTR result = 0;
      if (SendMessageTimeoutW(FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr),
                              FSW_WM_SET_PAUSED, 0, 0,
                              SMTO_ABORTIFHUNG | SMTO_BLOCK, 2000,
                              &result) != 0 &&
          result != 0) {
        if (BrokerState() == FswBrokerActive) {
          std::wcout << L"Forward Slash Windows broker resumed and is active.\n";
          return 0;
        }
        std::wcerr << L"Broker resumed but its keyboard hook is unavailable.\n";
        return 1;
      }
      std::wcerr << L"The broker did not accept the state change.\n";
      return 1;
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
  // The manifest's windows.startupTask owns logon start for a packaged build.
  // Writing the Run value here would survive uninstall as an orphaned entry
  // pointing into a WindowsApps directory that no longer exists.
  if (fsw::HasPackageIdentity()) {
    if (!enabled) {
      std::wcerr << L"Startup for the packaged app is controlled by Windows. "
                    L"Turn it off under Settings > Apps > Startup.\n";
    }
    return 0;
  }
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

int SetSettingsProtocol(const bool enabled) {
  // windows.protocol in the manifest owns fwdslash:// for a packaged build.
  // Registering it again under HKCU\Software\Classes would shadow the package
  // registration and outlive the package.
  if (fsw::HasPackageIdentity()) {
    return 0;
  }
  const std::filesystem::path settings =
      ExecutableDirectory() / L"fswsettings.exe";
  const std::wstring command = Quote(settings) + L" \"%1\"";
  const std::wstring command_key = std::wstring(kProtocolKey) +
                                   L"\\shell\\open\\command";
  if (enabled) {
    if (!std::filesystem::exists(settings)) {
      std::wcerr << L"The WinUI settings application was not found: "
                 << settings.wstring() << L"\n";
      return 1;
    }
    HKEY existing = nullptr;
    if (RegOpenKeyExW(HKEY_CURRENT_USER, command_key.c_str(), 0,
                      KEY_QUERY_VALUE, &existing) == ERROR_SUCCESS) {
      RegCloseKey(existing);
      if (!RegistryStringEquals(HKEY_CURRENT_USER, command_key, nullptr,
                                command)) {
        std::wcerr << L"The fwdslash URI scheme is already owned by another "
                      L"application. No protocol registration was changed.\n";
        return 1;
      }
      return 0;
    }
    HKEY root = nullptr;
    LSTATUS status = RegCreateKeyExW(HKEY_CURRENT_USER, kProtocolKey, 0,
                                      nullptr, 0, KEY_SET_VALUE, nullptr,
                                      &root, nullptr);
    if (status != ERROR_SUCCESS) {
      std::wcerr << L"Unable to create the fwdslash URI registration. Error "
                 << status << L".\n";
      return 1;
    }
    const std::wstring description = L"URL:Forward Slash Windows";
    status = RegSetValueExW(
        root, nullptr, 0, REG_SZ,
        reinterpret_cast<const BYTE*>(description.c_str()),
        static_cast<DWORD>((description.size() + 1) * sizeof(wchar_t)));
    if (status == ERROR_SUCCESS) {
      const wchar_t empty[] = L"";
      status = RegSetValueExW(root, L"URL Protocol", 0, REG_SZ,
                              reinterpret_cast<const BYTE*>(empty),
                              sizeof(empty));
    }
    RegCloseKey(root);
    HKEY command_handle = nullptr;
    if (status == ERROR_SUCCESS) {
      status = RegCreateKeyExW(HKEY_CURRENT_USER, command_key.c_str(), 0,
                               nullptr, 0, KEY_SET_VALUE, nullptr,
                               &command_handle, nullptr);
    }
    if (status == ERROR_SUCCESS) {
      status = RegSetValueExW(
          command_handle, nullptr, 0, REG_SZ,
          reinterpret_cast<const BYTE*>(command.c_str()),
          static_cast<DWORD>((command.size() + 1) * sizeof(wchar_t)));
    }
    if (command_handle != nullptr) {
      RegCloseKey(command_handle);
    }
    if (status != ERROR_SUCCESS) {
      RegDeleteTreeW(HKEY_CURRENT_USER, kProtocolKey);
      std::wcerr << L"Unable to complete the fwdslash URI registration. Error "
                 << status << L".\n";
      return 1;
    }
    return 0;
  }

  HKEY existing = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, command_key.c_str(), 0,
                    KEY_QUERY_VALUE, &existing) != ERROR_SUCCESS) {
    return 0;
  }
  RegCloseKey(existing);
  if (!RegistryStringEquals(HKEY_CURRENT_USER, command_key, nullptr,
                            command)) {
    std::wcerr << L"The fwdslash URI handler changed after registration. "
                  L"Refusing to remove another application's value.\n";
    return 1;
  }
  const LSTATUS status = RegDeleteTreeW(HKEY_CURRENT_USER, kProtocolKey);
  if (status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND) {
    std::wcerr << L"Unable to remove the fwdslash URI registration. Error "
               << status << L".\n";
    return 1;
  }
  return 0;
}

bool ExecutableAvailable(const wchar_t* name) {
  std::wstring path(32768, L'\0');
  const DWORD length = SearchPathW(nullptr, name, nullptr,
                                   static_cast<DWORD>(path.size()), path.data(),
                                   nullptr);
  return length > 0 && length < path.size();
}

int RunPowerShellScript(const std::filesystem::path& script,
                        const std::vector<std::wstring>& arguments) {
  if (!std::filesystem::exists(script)) {
    std::wcerr << L"Integration script was not found: " << script.wstring()
               << L"\n";
    return 1;
  }
  std::wstring command = L"powershell.exe -NoLogo -NoProfile -NonInteractive "
                         L"-ExecutionPolicy Bypass -File " +
                         Quote(script);
  for (const std::wstring& argument : arguments) {
    command.push_back(L' ');
    command.append(Quote(argument));
  }
  STARTUPINFOW startup{sizeof(startup)};
  PROCESS_INFORMATION process{};
  if (!CreateProcessW(nullptr, command.data(), nullptr, nullptr, TRUE, 0,
                      nullptr, ExecutableDirectory().c_str(), &startup,
                      &process)) {
    std::wcerr << L"Unable to start the integration transaction. Win32 error "
               << GetLastError() << L".\n";
    return 1;
  }
  CloseHandle(process.hThread);
  WaitForSingleObject(process.hProcess, INFINITE);
  DWORD exit_code = 1;
  GetExitCodeProcess(process.hProcess, &exit_code);
  CloseHandle(process.hProcess);
  return static_cast<int>(exit_code);
}

int SetScriptIntegration(const std::wstring_view id, const bool enabled) {
  const std::filesystem::path directory = ExecutableDirectory();
  if (id == L"cmd") {
    if (RegistryStateInstalled(kCmdAdapterKey) == enabled) {
      return 0;
    }
    const auto script = directory /
        (enabled ? L"Install-CmdAdapter.ps1" : L"Uninstall-CmdAdapter.ps1");
    std::vector<std::wstring> arguments;
    if (enabled) {
      arguments = {L"-ControllerPath", (directory / L"fwdslash.exe").wstring()};
    }
    return RunPowerShellScript(script, arguments);
  }

  std::wstring edition;
  if (id == L"windows-powershell") {
    edition = L"WindowsPowerShell";
  } else if (id == L"powershell") {
    edition = L"PowerShell";
    if (enabled && !ExecutableAvailable(L"pwsh.exe")) {
      std::wcerr << L"PowerShell 7 is not installed.\n";
      return 1;
    }
  } else {
    return 2;
  }
  const std::wstring state_key = std::wstring(kPowerShellAdapterRoot) + edition;
  if (RegistryStateInstalled(state_key) == enabled) {
    return 0;
  }
  const auto script = directory /
      (enabled ? L"Install-PowerShellAdapter.ps1"
               : L"Uninstall-PowerShellAdapter.ps1");
  std::vector<std::wstring> arguments = {L"-Edition", edition};
  if (enabled) {
    arguments.emplace_back(L"-ControllerPath");
    arguments.emplace_back((directory / L"fwdslash.exe").wstring());
  }
  return RunPowerShellScript(script, arguments);
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

int ShowBareSlashState() {
  const auto distributions = fsw::ListRegisteredDistributions();
  const fsw::BareSlashMode mode = fsw::GetBareSlashMode();
  const std::wstring pinned = fsw::GetBareSlashOverride();
  const auto wsl_default = fsw::GetDefaultDistribution(distributions);
  const fsw::ResolveResult bare = fsw::ResolveUserSlashPath(L"/");
  std::wcout << L"bare slash mode: "
             << (mode == fsw::BareSlashMode::default_distribution
                     ? L"default distribution"
                     : L"distribution list")
             << L"\n";
  if (!pinned.empty()) {
    std::wcout << L"pinned distribution: /" << pinned << L"\n";
  }
  std::wcout << L"WSL default distribution: "
             << (wsl_default.has_value() ? L"/" + *wsl_default : L"none")
             << L"\n";
  if (bare.matched()) {
    std::wcout << L"/ resolves to: " << bare.unc_path << L"\n";
  } else {
    std::wcout << L"/ is blocked: "
               << fsw::ResolveErrorMessage(bare.error, distributions) << L"\n";
  }
  return 0;
}

int WriteBareSlashSettings(const bool default_mode,
                           const std::wstring& pinned_distribution) {
  HKEY key = nullptr;
  const LSTATUS opened = RegCreateKeyExW(HKEY_CURRENT_USER, FSW_SETTINGS_KEY, 0,
                                         nullptr, 0, KEY_SET_VALUE, nullptr,
                                         &key, nullptr);
  if (opened != ERROR_SUCCESS) {
    std::wcerr << L"Unable to open the settings key. Error " << opened
               << L".\n";
    return 1;
  }
  const DWORD value = default_mode ? 1U : 0U;
  LSTATUS status =
      RegSetValueExW(key, FSW_BARE_SLASH_MODE_VALUE, 0, REG_DWORD,
                     reinterpret_cast<const BYTE*>(&value), sizeof(value));
  if (status == ERROR_SUCCESS) {
    if (default_mode && !pinned_distribution.empty()) {
      status = RegSetValueExW(
          key, FSW_BARE_SLASH_DISTRIBUTION_VALUE, 0, REG_SZ,
          reinterpret_cast<const BYTE*>(pinned_distribution.c_str()),
          static_cast<DWORD>((pinned_distribution.size() + 1) *
                             sizeof(wchar_t)));
    } else {
      status = RegDeleteValueW(key, FSW_BARE_SLASH_DISTRIBUTION_VALUE);
      if (status == ERROR_FILE_NOT_FOUND) {
        status = ERROR_SUCCESS;
      }
    }
  }
  RegCloseKey(key);
  if (status != ERROR_SUCCESS) {
    std::wcerr << L"Unable to persist the bare slash mode. Error " << status
               << L".\n";
    return 1;
  }
  return 0;
}

int SetBareSlash(const bool default_mode,
                 const std::wstring& pinned_distribution) {
  if (default_mode && !pinned_distribution.empty() &&
      !fsw::IsRegisteredDistribution(pinned_distribution)) {
    std::wcerr << L"That WSL distribution is not registered.\n";
    return 1;
  }
  if (WriteBareSlashSettings(default_mode, pinned_distribution) != 0) {
    return 1;
  }
  return ShowBareSlashState();
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
    const fsw::ResolveResult bare = fsw::ResolveUserSlashPath(L"/");
    std::wcout << L"{\"broker\":\"" << broker_status
               << L"\",\"driverConnected\":"
               << (IsDriverAvailable() ? L"true" : L"false")
               << L",\"disabled\":" << (IsDisabled() ? L"true" : L"false")
               << L",\"bareSlashMode\":\""
               << (fsw::GetBareSlashMode() ==
                           fsw::BareSlashMode::default_distribution
                       ? L"default"
                       : L"list")
               << L"\",\"bareSlashTarget\":"
               << (bare.matched()
                       ? L"\"" + JsonEscape(bare.unc_path) + L"\""
                       : L"null")
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
  std::wcout << L"global state: " << (IsDisabled() ? L"disabled" : L"enabled")
             << L"\n";
  std::wcout << L"filesystem driver: "
             << (IsDriverAvailable() ? L"connected" : L"not connected")
             << L"\n";
  std::wcout << L"registered distributions: " << distributions.size() << L"\n";
  const fsw::ResolveResult bare = fsw::ResolveUserSlashPath(L"/");
  if (bare.matched()) {
    std::wcout << L"  / -> " << bare.unc_path
               << (bare.is_wsl_root() ? L" (distribution list)"
                                      : L" (default distribution)")
               << L"\n";
  } else {
    std::wcout << L"  / -> blocked. "
               << fsw::ResolveErrorMessage(bare.error, distributions) << L"\n";
  }
  for (const std::wstring& distribution : distributions) {
    std::wcout << L"  /" << distribution << L"/ -> \\\\wsl.localhost\\"
               << distribution << L"\\\n";
  }
  return 0;
}

int SetPaused(const bool paused) {
  if (PersistDisabled(paused) != 0) {
    return 1;
  }
  const HWND window = FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr);
  if (window == nullptr) {
    std::wcout << (paused ? L"Forward-slash resolution disabled.\n"
                         : L"Forward-slash resolution enabled.\n");
    return 0;
  }
  DWORD_PTR result = 0;
  if (!SendMessageTimeoutW(window, FSW_WM_SET_PAUSED, paused ? 1 : 0, 0,
                           SMTO_ABORTIFHUNG | SMTO_BLOCK, 2000, &result) ||
      result == 0) {
    std::wcerr << L"The broker did not accept the state change.\n";
    return 1;
  }
  std::wcout << (paused ? L"Forward-slash resolution disabled.\n"
                        : L"Forward-slash resolution enabled.\n");
  return 0;
}

int ShowSettings(const std::wstring_view section) {
  const std::filesystem::path settings =
      ExecutableDirectory() / L"fswsettings.exe";
  if (!std::filesystem::exists(settings)) {
    std::wcerr << L"The WinUI settings application was not found: "
               << settings.wstring() << L"\n";
    return 1;
  }
  const std::wstring argument = L"fwdslash://settings/" +
      std::wstring(section.empty() ? L"general" : section);
  SHELLEXECUTEINFOW execute{sizeof(execute)};
  execute.lpVerb = L"open";
  execute.lpFile = settings.c_str();
  execute.lpParameters = argument.c_str();
  execute.nShow = SW_SHOWNORMAL;
  return ShellExecuteExW(&execute) ? 0 : 1;
}

int IntegrationStatus(const bool json) {
  const bool windows = IsWindowsIntegrationInstalled();
  const bool cmd = RegistryStateInstalled(kCmdAdapterKey);
  const bool windows_powershell = RegistryStateInstalled(
      std::wstring(kPowerShellAdapterRoot) + L"WindowsPowerShell");
  const bool powershell = RegistryStateInstalled(
      std::wstring(kPowerShellAdapterRoot) + L"PowerShell");
  const bool powershell_available = ExecutableAvailable(L"pwsh.exe");
  if (json) {
    std::wcout << L"{\"disabled\":" << (IsDisabled() ? L"true" : L"false")
               << L",\"windows\":" << (windows ? L"true" : L"false")
               << L",\"cmd\":" << (cmd ? L"true" : L"false")
               << L",\"windowsPowerShell\":"
               << (windows_powershell ? L"true" : L"false")
               << L",\"powerShell7\":" << (powershell ? L"true" : L"false")
               << L",\"powerShell7Available\":"
               << (powershell_available ? L"true" : L"false") << L"}\n";
    return 0;
  }
  std::wcout << L"resolution: " << (IsDisabled() ? L"disabled" : L"enabled")
             << L"\nWindows surfaces: " << (windows ? L"installed" : L"not installed")
             << L"\nCommand Prompt: " << (cmd ? L"installed" : L"not installed")
             << L"\nWindows PowerShell: "
             << (windows_powershell ? L"installed" : L"not installed")
             << L"\nPowerShell 7: " << (powershell ? L"installed" : L"not installed")
             << (powershell_available ? L"" : L" (PowerShell 7 unavailable)")
             << L"\n";
  return 0;
}

int SetWindowsIntegration(const bool enabled) {
  if (enabled) {
    if (SetSettingsProtocol(true) != 0 || SetStartup(true) != 0) {
      return 1;
    }
    const int started = StartBroker();
    if (started != 0) {
      SetStartup(false);
    }
    return started;
  }
  const int stopped = StopBroker();
  const int unregistered = SetStartup(false);
  return stopped != 0 ? stopped : unregistered;
}

int Doctor(const std::wstring_view path) {
  const fsw::ResolveResult resolved = fsw::ResolveUserSlashPath(path);
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
  const fsw::ResolveResult resolved = fsw::ResolveUserSlashPath(path);
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
  const fsw::ResolveResult resolved = fsw::ResolveUserSlashPath(path);
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
  const fsw::ResolveResult resolved = fsw::ResolveUserSlashPath(path);
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
  if (IsDisabled()) {
    return 3;
  }
  const fsw::ResolveResult resolved = fsw::ResolveUserSlashPath(path);
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
      L"  fwdslash status [--json]\n"
      L"  fwdslash resolve /Distro/path\n"
      L"  fwdslash open /Distro/path\n"
      L"  fwdslash list /Distro/path\n"
      L"  fwdslash doctor /Distro/path | --all\n"
      L"  fwdslash settings [general|windows|cmd|windows-powershell|powershell]\n"
      L"  fwdslash integrations [--json]\n"
      L"  fwdslash integration <name> enable|disable\n"
      L"  fwdslash bare-slash\n"
      L"  fwdslash bare-slash list | default [Distro]\n"
      L"  fwdslash disable | enable\n"
      L"  fwdslash pause | resume       Aliases for disable and enable\n"
      L"  fwdslash driver status\n"
      L"  fwdslash start | stop\n"
      L"  fwdslash install       Register and start the per-user broker\n"
      L"  fwdslash uninstall     Stop and unregister the per-user broker\n\n"
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
  if (command == L"settings" && (argc == 2 || argc == 3)) {
    return ShowSettings(argc == 3 ? std::wstring_view(argv[2]) : L"general");
  }
  if (command == L"integrations" &&
      (argc == 2 || (argc == 3 && std::wstring_view(argv[2]) == L"--json"))) {
    return IntegrationStatus(argc == 3);
  }
  if (command == L"bare-slash") {
    if (argc == 2) {
      return ShowBareSlashState();
    }
    const std::wstring_view operation(argv[2]);
    if (argc == 3 && operation == L"list") {
      return SetBareSlash(false, L"");
    }
    if (argc == 3 && operation == L"default") {
      return SetBareSlash(true, L"");
    }
    if (argc == 4 && operation == L"default") {
      return SetBareSlash(true, argv[3]);
    }
    Usage();
    return 2;
  }
  if (command == L"integration" && argc == 4) {
    const std::wstring_view integration(argv[2]);
    const std::wstring_view operation(argv[3]);
    if (operation != L"enable" && operation != L"disable") {
      Usage();
      return 2;
    }
    const bool enabled = operation == L"enable";
    if (integration == L"windows") {
      return SetWindowsIntegration(enabled);
    }
    const int result = SetScriptIntegration(integration, enabled);
    if (result == 2) {
      Usage();
    }
    return result;
  }
  if ((command == L"pause" || command == L"disable") && argc == 2) {
    return SetPaused(true);
  }
  if ((command == L"resume" || command == L"enable") && argc == 2) {
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
    return SetWindowsIntegration(true);
  }
  if (command == L"uninstall") {
    const int windows = SetWindowsIntegration(false);
    const int protocol = SetSettingsProtocol(false);
    return windows != 0 ? windows : protocol;
  }
  Usage();
  return 2;
}
