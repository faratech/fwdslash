#include <windows.h>
#include <fltuser.h>
#include <ole2.h>
#include <oleauto.h>
#include <oaidl.h>
#include <ocidl.h>
#include <exdisp.h>
#include <shellapi.h>
#include <uiautomation.h>

#include "fsw/path_resolver.hpp"
#include "fsw/wsl_registry.hpp"
#include "fsw_filter_protocol.h"
#include "fsw_user_protocol.h"

#include <algorithm>
#include <memory>
#include <new>
#include <string>
#include <string_view>
#include <vector>

namespace {

constexpr wchar_t kMutexName[] = L"Local\\ForwardSlashWindows.Broker";
constexpr wchar_t kSettingsWindowClass[] = L"ForwardSlashWindows.Settings";
constexpr UINT kTrayMessage = WM_APP + 1;
constexpr UINT kProcessEnter = WM_APP + 2;
constexpr UINT_PTR kTrayId = 1;
constexpr UINT_PTR kHealthTimer = 1;
constexpr ULONG_PTR kReplayMarker = 0x4653572F;
constexpr UINT kMenuSettings = 1001;
constexpr UINT kMenuPause = 1002;
constexpr UINT kMenuExit = 1003;
constexpr UINT kSettingsStatus = 2001;
constexpr UINT kSettingsRefresh = 2002;
constexpr UINT kSettingsOpenRoot = 2003;
constexpr UINT kSettingsPause = 2004;
constexpr UINT kSettingsClose = 2005;

enum class SurfaceKind { unknown, explorer, run, search, common_dialog };

struct EnterRequest {
  HWND foreground{};
};

HHOOK g_keyboard_hook = nullptr;
IUIAutomation* g_automation = nullptr;
HWND g_broker_window = nullptr;
HWND g_settings_window = nullptr;
HANDLE g_filter_port = INVALID_HANDLE_VALUE;
bool g_paused = false;
bool g_enter_down = false;
bool g_suppress_enter_up = false;
std::vector<std::wstring> g_published_distributions;

void Diagnostic(const std::wstring_view message) {
  std::wstring path(32768, L'\0');
  const DWORD length = GetEnvironmentVariableW(
      L"FSW_DIAGNOSTIC_LOG", path.data(), static_cast<DWORD>(path.size()));
  if (length == 0 || length >= path.size()) {
    return;
  }
  path.resize(length);
  std::wstring line(message);
  line.append(L"\r\n");
  const int required = WideCharToMultiByte(
      CP_UTF8, 0, line.data(), static_cast<int>(line.size()), nullptr, 0,
      nullptr, nullptr);
  if (required <= 0) {
    return;
  }
  std::string bytes(static_cast<size_t>(required), '\0');
  WideCharToMultiByte(CP_UTF8, 0, line.data(), static_cast<int>(line.size()),
                      bytes.data(), required, nullptr, nullptr);
  const HANDLE file = CreateFileW(path.c_str(), FILE_APPEND_DATA,
                                  FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr,
                                  OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
  if (file == INVALID_HANDLE_VALUE) {
    return;
  }
  DWORD written = 0;
  WriteFile(file, bytes.data(), static_cast<DWORD>(bytes.size()), &written,
            nullptr);
  CloseHandle(file);
}

std::wstring ProcessName(const DWORD process_id) {
  const HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE,
                                     process_id);
  if (process == nullptr) {
    return {};
  }
  std::wstring image(32768, L'\0');
  DWORD length = static_cast<DWORD>(image.size());
  const bool queried =
      QueryFullProcessImageNameW(process, 0, image.data(), &length) != FALSE;
  CloseHandle(process);
  if (!queried) {
    return {};
  }
  image.resize(length);
  const size_t separator = image.find_last_of(L"\\/");
  return separator == std::wstring::npos ? image : image.substr(separator + 1);
}

std::wstring WindowClass(const HWND window) {
  std::wstring value(256, L'\0');
  const int length = GetClassNameW(window, value.data(),
                                   static_cast<int>(value.size()));
  if (length <= 0) {
    return {};
  }
  value.resize(static_cast<size_t>(length));
  return value;
}

SurfaceKind ClassifySurface(const HWND foreground) {
  if (foreground == nullptr) {
    return SurfaceKind::unknown;
  }
  DWORD process_id = 0;
  GetWindowThreadProcessId(foreground, &process_id);
  const std::wstring process = ProcessName(process_id);
  const std::wstring window_class = WindowClass(foreground);
  if (fsw::EqualsOrdinalIgnoreCase(process, L"SearchHost.exe") ||
      fsw::EqualsOrdinalIgnoreCase(process, L"SearchApp.exe") ||
      fsw::EqualsOrdinalIgnoreCase(process, L"StartMenuExperienceHost.exe")) {
    return SurfaceKind::search;
  }
  if (fsw::EqualsOrdinalIgnoreCase(process, L"explorer.exe")) {
    if (fsw::EqualsOrdinalIgnoreCase(window_class, L"CabinetWClass") ||
        fsw::EqualsOrdinalIgnoreCase(window_class, L"ExploreWClass")) {
      return SurfaceKind::explorer;
    }
    if (fsw::EqualsOrdinalIgnoreCase(window_class, L"#32770")) {
      return SurfaceKind::run;
    }
  }
  if (fsw::EqualsOrdinalIgnoreCase(window_class, L"#32770")) {
    return SurfaceKind::common_dialog;
  }
  return SurfaceKind::unknown;
}

bool SendVirtualKey(const WORD key) {
  INPUT inputs[2]{};
  for (INPUT& input : inputs) {
    input.type = INPUT_KEYBOARD;
    input.ki.wVk = key;
    input.ki.dwExtraInfo = kReplayMarker;
  }
  inputs[1].ki.dwFlags = KEYEVENTF_KEYUP;
  return SendInput(2, inputs, sizeof(INPUT)) == 2;
}

void ReplayEnter() {
  if (!SendVirtualKey(VK_RETURN)) {
    Diagnostic(L"event=replay_enter_failed");
  }
}

bool ReadFocusedValue(IUIAutomationElement** focused, std::wstring& value) {
  *focused = nullptr;
  if (g_automation == nullptr ||
      FAILED(g_automation->GetFocusedElement(focused)) || *focused == nullptr) {
    return false;
  }
  VARIANT property{};
  VariantInit(&property);
  const HRESULT result = (*focused)->GetCurrentPropertyValue(
      UIA_ValueValuePropertyId, &property);
  if (SUCCEEDED(result) && property.vt == VT_BSTR &&
      property.bstrVal != nullptr) {
    value.assign(property.bstrVal, SysStringLen(property.bstrVal));
  }
  VariantClear(&property);
  if (!value.empty()) {
    return true;
  }
  IUIAutomationLegacyIAccessiblePattern* legacy = nullptr;
  if (SUCCEEDED((*focused)->GetCurrentPatternAs(
          UIA_LegacyIAccessiblePatternId, IID_PPV_ARGS(&legacy))) &&
      legacy != nullptr) {
    BSTR legacy_value = nullptr;
    if (SUCCEEDED(legacy->get_CurrentValue(&legacy_value)) &&
        legacy_value != nullptr) {
      value.assign(legacy_value, SysStringLen(legacy_value));
      SysFreeString(legacy_value);
    }
    legacy->Release();
  }
  return !value.empty();
}

bool SetFocusedValue(IUIAutomationElement* focused,
                     const std::wstring& value) {
  IUIAutomationValuePattern* pattern = nullptr;
  if (focused == nullptr ||
      FAILED(focused->GetCurrentPatternAs(UIA_ValuePatternId,
                                          IID_PPV_ARGS(&pattern))) ||
      pattern == nullptr) {
    return false;
  }
  BSTR replacement = SysAllocStringLen(value.data(),
                                       static_cast<UINT>(value.size()));
  if (replacement == nullptr) {
    pattern->Release();
    return false;
  }
  const HRESULT result = pattern->SetValue(replacement);
  SysFreeString(replacement);
  pattern->Release();
  return SUCCEEDED(result);
}

bool OpenResolvedPath(const std::wstring& path) {
  SHELLEXECUTEINFOW execute{sizeof(execute)};
  execute.lpVerb = L"open";
  execute.lpFile = path.c_str();
  execute.nShow = SW_SHOWNORMAL;
  if (!ShellExecuteExW(&execute)) {
    Diagnostic(L"event=shell_open_failed error=" +
               std::to_wstring(GetLastError()));
    return false;
  }
  return true;
}

bool NavigateExplorerWindow(const HWND foreground, const std::wstring& path) {
  IShellWindows* shell_windows = nullptr;
  if (FAILED(CoCreateInstance(CLSID_ShellWindows, nullptr,
                              CLSCTX_LOCAL_SERVER,
                              IID_PPV_ARGS(&shell_windows))) ||
      shell_windows == nullptr) {
    return false;
  }
  long count = 0;
  bool navigated = false;
  if (SUCCEEDED(shell_windows->get_Count(&count))) {
    for (long index = 0; index < count && !navigated; ++index) {
      VARIANT item{};
      VariantInit(&item);
      item.vt = VT_I4;
      item.lVal = index;
      IDispatch* dispatch = nullptr;
      if (FAILED(shell_windows->Item(item, &dispatch)) || dispatch == nullptr) {
        continue;
      }
      IWebBrowser2* browser = nullptr;
      if (SUCCEEDED(dispatch->QueryInterface(IID_PPV_ARGS(&browser))) &&
          browser != nullptr) {
        SHANDLE_PTR browser_handle = 0;
        if (SUCCEEDED(browser->get_HWND(&browser_handle)) &&
            reinterpret_cast<HWND>(browser_handle) == foreground) {
          VARIANT target{};
          VariantInit(&target);
          target.vt = VT_BSTR;
          target.bstrVal = SysAllocStringLen(
              path.data(), static_cast<UINT>(path.size()));
          if (target.bstrVal != nullptr) {
            VARIANT empty{};
            VariantInit(&empty);
            navigated = SUCCEEDED(
                browser->Navigate2(&target, &empty, &empty, &empty, &empty));
            VariantClear(&target);
          }
        }
        browser->Release();
      }
      dispatch->Release();
    }
  }
  shell_windows->Release();
  return navigated;
}

void ShowNotification(const std::wstring& message, const DWORD flags) {
  if (g_broker_window == nullptr) {
    return;
  }
  NOTIFYICONDATAW icon{sizeof(icon)};
  icon.hWnd = g_broker_window;
  icon.uID = kTrayId;
  icon.uFlags = NIF_INFO;
  icon.dwInfoFlags = flags;
  wcscpy_s(icon.szInfoTitle, L"Forward Slash Windows");
  wcsncpy_s(icon.szInfo, message.c_str(), _TRUNCATE);
  Shell_NotifyIconW(NIM_MODIFY, &icon);
}

void HandleEnterRequest(std::unique_ptr<EnterRequest> request) {
  if (request == nullptr || g_paused ||
      request->foreground != GetForegroundWindow()) {
    ReplayEnter();
    return;
  }
  const SurfaceKind surface = ClassifySurface(request->foreground);
  if (surface == SurfaceKind::unknown) {
    ReplayEnter();
    return;
  }
  IUIAutomationElement* raw_element = nullptr;
  std::wstring input;
  if (!ReadFocusedValue(&raw_element, input)) {
    if (raw_element != nullptr) {
      raw_element->Release();
    }
    ReplayEnter();
    return;
  }
  const std::unique_ptr<IUIAutomationElement, void (*)(IUIAutomationElement*)>
      element(raw_element, [](IUIAutomationElement* value) {
        if (value != nullptr) {
          value->Release();
        }
      });
  if (input.empty() || input.front() != L'/') {
    ReplayEnter();
    return;
  }
  const fsw::ResolveResult resolved = fsw::ResolveRegisteredSlashPath(input);
  if (!resolved.matched()) {
    Diagnostic(L"event=path_rejected reason=" +
               std::wstring(fsw::ResolveErrorName(resolved.error)));
    ShowNotification(
        fsw::ResolveErrorMessage(resolved.error,
                                 fsw::ListRegisteredDistributions()),
        NIIF_WARNING);
    return;
  }
  Diagnostic(resolved.is_wsl_root() ? L"event=route_wsl_root"
                                    : L"event=route_distribution");
  if (surface == SurfaceKind::search) {
    if (!OpenResolvedPath(resolved.unc_path)) {
      ShowNotification(L"Windows could not open the WSL location.",
                       NIIF_ERROR);
    }
    SendVirtualKey(VK_ESCAPE);
    return;
  }
  if (surface == SurfaceKind::explorer && resolved.is_wsl_root() &&
      NavigateExplorerWindow(request->foreground, resolved.unc_path)) {
    return;
  }
  if (SetFocusedValue(element.get(), resolved.unc_path)) {
    ReplayEnter();
    return;
  }
  if (!OpenResolvedPath(resolved.unc_path)) {
    ShowNotification(L"Windows could not open the WSL location.", NIIF_ERROR);
  }
}

LRESULT CALLBACK LowLevelKeyboardProcedure(const int code, const WPARAM wparam,
                                           const LPARAM lparam) {
  if (code != HC_ACTION || lparam == 0) {
    return CallNextHookEx(nullptr, code, wparam, lparam);
  }
  const auto* key = reinterpret_cast<const KBDLLHOOKSTRUCT*>(lparam);
  if (key->vkCode != VK_RETURN || key->dwExtraInfo == kReplayMarker) {
    return CallNextHookEx(nullptr, code, wparam, lparam);
  }
  const bool key_up = wparam == WM_KEYUP || wparam == WM_SYSKEYUP ||
                      (key->flags & LLKHF_UP) != 0;
  if (key_up) {
    g_enter_down = false;
    if (g_suppress_enter_up) {
      g_suppress_enter_up = false;
      return 1;
    }
    return CallNextHookEx(nullptr, code, wparam, lparam);
  }
  if (g_enter_down) {
    return g_suppress_enter_up ? 1
                               : CallNextHookEx(nullptr, code, wparam, lparam);
  }
  g_enter_down = true;
  const HWND foreground = GetForegroundWindow();
  if (g_paused || ClassifySurface(foreground) == SurfaceKind::unknown) {
    return CallNextHookEx(nullptr, code, wparam, lparam);
  }
  auto request = std::make_unique<EnterRequest>();
  request->foreground = foreground;
  if (!PostMessageW(g_broker_window, kProcessEnter, 0,
                    reinterpret_cast<LPARAM>(request.get()))) {
    return CallNextHookEx(nullptr, code, wparam, lparam);
  }
  request.release();
  g_suppress_enter_up = true;
  return 1;
}

bool InstallHook() {
  if (g_keyboard_hook != nullptr) {
    return true;
  }
  if (g_automation == nullptr &&
      FAILED(CoCreateInstance(CLSID_CUIAutomation, nullptr,
                              CLSCTX_INPROC_SERVER,
                              IID_PPV_ARGS(&g_automation)))) {
    g_automation = nullptr;
    return false;
  }
  g_keyboard_hook = SetWindowsHookExW(WH_KEYBOARD_LL,
                                      LowLevelKeyboardProcedure,
                                      GetModuleHandleW(nullptr), 0);
  return g_keyboard_hook != nullptr;
}

void RemoveHook() {
  if (g_keyboard_hook != nullptr) {
    UnhookWindowsHookEx(g_keyboard_hook);
    g_keyboard_hook = nullptr;
  }
  g_enter_down = false;
  g_suppress_enter_up = false;
  if (g_automation != nullptr) {
    g_automation->Release();
    g_automation = nullptr;
  }
}

bool DistributionListsEqual(const std::vector<std::wstring>& left,
                            const std::vector<std::wstring>& right) {
  if (left.size() != right.size()) {
    return false;
  }
  for (size_t index = 0; index < left.size(); ++index) {
    if (!fsw::EqualsOrdinalIgnoreCase(left[index], right[index])) {
      return false;
    }
  }
  return true;
}

void DisconnectFilter() {
  if (g_filter_port != INVALID_HANDLE_VALUE) {
    CloseHandle(g_filter_port);
    g_filter_port = INVALID_HANDLE_VALUE;
  }
  g_published_distributions.clear();
}

void PublishFilterMappings(const bool force) {
  std::vector<std::wstring> distributions =
      fsw::ListRegisteredDistributions();
  std::sort(distributions.begin(), distributions.end(),
            [](const std::wstring& left, const std::wstring& right) {
              return CompareStringOrdinal(left.c_str(), -1, right.c_str(), -1,
                                          TRUE) == CSTR_LESS_THAN;
            });
  if (!force && DistributionListsEqual(distributions,
                                       g_published_distributions)) {
    return;
  }
  if (g_filter_port == INVALID_HANDLE_VALUE) {
    const HRESULT connected = FilterConnectCommunicationPort(
        FSW_FILTER_PORT_NAME, 0, nullptr, 0, nullptr, &g_filter_port);
    if (FAILED(connected)) {
      g_filter_port = INVALID_HANDLE_VALUE;
      return;
    }
  }
  FSW_MAPPING_MESSAGE message{};
  message.Version = FSW_PROTOCOL_VERSION;
  message.Size = sizeof(message);
  message.Operation = FswOperationReplaceMappings;
  message.Generation = GetTickCount64();
  message.DistributionCount = static_cast<ULONG>((std::min)(
      distributions.size(), static_cast<size_t>(FSW_MAX_DISTRIBUTIONS)));
  for (ULONG index = 0; index < message.DistributionCount; ++index) {
    wcsncpy_s(message.Distributions[index], distributions[index].c_str(),
              _TRUNCATE);
  }
  DWORD returned = 0;
  const HRESULT sent = FilterSendMessage(g_filter_port, &message,
                                         sizeof(message), nullptr, 0,
                                         &returned);
  if (FAILED(sent)) {
    DisconnectFilter();
    return;
  }
  g_published_distributions = std::move(distributions);
}

void SetTrayIcon(const HWND window, const bool add) {
  NOTIFYICONDATAW icon{sizeof(icon)};
  icon.hWnd = window;
  icon.uID = kTrayId;
  icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
  icon.uCallbackMessage = kTrayMessage;
  icon.hIcon = LoadIconW(nullptr, IDI_APPLICATION);
  wcscpy_s(icon.szTip, L"Forward Slash Windows");
  Shell_NotifyIconW(add ? NIM_ADD : NIM_DELETE, &icon);
  if (add) {
    icon.uVersion = NOTIFYICON_VERSION_4;
    Shell_NotifyIconW(NIM_SETVERSION, &icon);
  }
}

std::wstring SettingsStatusText() {
  const auto distributions = fsw::ListRegisteredDistributions();
  std::wstring text = L"Shell broker: ";
  text.append(g_paused ? L"paused"
                       : g_keyboard_hook != nullptr ? L"active"
                                                    : L"hook unavailable");
  text.append(L"\r\nFilesystem driver: ");
  text.append(g_filter_port != INVALID_HANDLE_VALUE
                  ? L"connected"
                  : L"not installed/connected");
  text.append(L"\r\n\r\n/  ->  \\\\wsl.localhost  (distribution list)\r\n");
  for (const std::wstring& distribution : distributions) {
    text.append(L"/");
    text.append(distribution);
    text.append(L"  ->  \\\\wsl.localhost\\");
    text.append(distribution);
    text.append(L"\r\n");
  }
  if (distributions.empty()) {
    text.append(L"No registered WSL distributions were found.\r\n");
  }
  text.append(
      L"\r\nInvalid slash paths are blocked instead of sent to web search.");
  return text;
}

void RefreshSettings() {
  if (g_settings_window == nullptr) {
    return;
  }
  SetWindowTextW(GetDlgItem(g_settings_window, kSettingsStatus),
                 SettingsStatusText().c_str());
  SetWindowTextW(GetDlgItem(g_settings_window, kSettingsPause),
                 g_paused ? L"Resume" : L"Pause");
}

void SetPaused(const bool paused) {
  g_paused = paused;
  if (paused) {
    RemoveHook();
  } else {
    InstallHook();
  }
  RefreshSettings();
}

LRESULT CALLBACK SettingsWindowProcedure(const HWND window, const UINT message,
                                         const WPARAM wparam,
                                         const LPARAM lparam) {
  switch (message) {
    case WM_CREATE: {
      const HFONT font = static_cast<HFONT>(GetStockObject(DEFAULT_GUI_FONT));
      const HWND status = CreateWindowExW(
          WS_EX_CLIENTEDGE, L"STATIC", L"", WS_CHILD | WS_VISIBLE | SS_LEFT,
          16, 16, 580, 260, window,
          reinterpret_cast<HMENU>(static_cast<INT_PTR>(kSettingsStatus)),
          nullptr, nullptr);
      SendMessageW(status, WM_SETFONT, reinterpret_cast<WPARAM>(font), TRUE);
      const struct Button {
        UINT id;
        const wchar_t* text;
        int x;
        int width;
      } buttons[] = {{kSettingsRefresh, L"Refresh", 16, 90},
                     {kSettingsOpenRoot, L"Open WSL root", 114, 125},
                     {kSettingsPause, L"Pause", 247, 90},
                     {kSettingsClose, L"Close", 506, 90}};
      for (const Button& button : buttons) {
        const HWND control = CreateWindowExW(
            0, L"BUTTON", button.text,
            WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON, button.x, 292,
            button.width, 30, window,
            reinterpret_cast<HMENU>(static_cast<INT_PTR>(button.id)), nullptr,
            nullptr);
        SendMessageW(control, WM_SETFONT, reinterpret_cast<WPARAM>(font), TRUE);
      }
      RefreshSettings();
      return 0;
    }
    case WM_COMMAND:
      switch (LOWORD(wparam)) {
        case kSettingsRefresh:
          PublishFilterMappings(true);
          RefreshSettings();
          return 0;
        case kSettingsOpenRoot:
          OpenResolvedPath(L"\\\\wsl.localhost");
          return 0;
        case kSettingsPause:
          SetPaused(!g_paused);
          return 0;
        case kSettingsClose:
          ShowWindow(window, SW_HIDE);
          return 0;
      }
      break;
    case WM_CLOSE:
      ShowWindow(window, SW_HIDE);
      return 0;
    case WM_DESTROY:
      g_settings_window = nullptr;
      return 0;
  }
  return DefWindowProcW(window, message, wparam, lparam);
}

void ShowSettings(const HWND owner) {
  if (g_settings_window == nullptr) {
    g_settings_window = CreateWindowExW(
        WS_EX_APPWINDOW, kSettingsWindowClass, L"Forward Slash Windows",
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        CW_USEDEFAULT, CW_USEDEFAULT, 630, 380, owner, nullptr,
        GetModuleHandleW(nullptr), nullptr);
  }
  RefreshSettings();
  ShowWindow(g_settings_window, SW_SHOWNORMAL);
  SetForegroundWindow(g_settings_window);
}

void ShowTrayMenu(const HWND window) {
  POINT cursor{};
  GetCursorPos(&cursor);
  const HMENU menu = CreatePopupMenu();
  AppendMenuW(menu, MF_STRING, kMenuSettings, L"Settings...");
  AppendMenuW(menu, MF_STRING, kMenuPause, g_paused ? L"Resume" : L"Pause");
  AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
  AppendMenuW(menu, MF_STRING, kMenuExit, L"Exit");
  SetForegroundWindow(window);
  TrackPopupMenu(menu, TPM_RIGHTBUTTON, cursor.x, cursor.y, 0, window, nullptr);
  DestroyMenu(menu);
}

LRESULT CALLBACK WindowProcedure(const HWND window, const UINT message,
                                 const WPARAM wparam, const LPARAM lparam) {
  switch (message) {
    case FSW_WM_QUERY_STATE:
      return g_paused ? FswBrokerPaused
                      : (g_keyboard_hook == nullptr ? FswBrokerUnavailable
                                                    : FswBrokerActive);
    case FSW_WM_SET_PAUSED:
      SetPaused(wparam != 0);
      return TRUE;
    case FSW_WM_SHOW_SETTINGS:
      ShowSettings(window);
      return TRUE;
    case kProcessEnter:
      HandleEnterRequest(std::unique_ptr<EnterRequest>(
          reinterpret_cast<EnterRequest*>(lparam)));
      return 0;
    case WM_TIMER:
      if (wparam == kHealthTimer) {
        PublishFilterMappings(false);
        RefreshSettings();
      }
      return 0;
    case WM_COMMAND:
      switch (LOWORD(wparam)) {
        case kMenuSettings:
          ShowSettings(window);
          return 0;
        case kMenuPause:
          SetPaused(!g_paused);
          return 0;
        case kMenuExit:
          DestroyWindow(window);
          return 0;
      }
      break;
    case kTrayMessage:
      if (LOWORD(lparam) == WM_RBUTTONUP || LOWORD(lparam) == WM_CONTEXTMENU) {
        ShowTrayMenu(window);
      } else if (LOWORD(lparam) == WM_LBUTTONDBLCLK) {
        ShowSettings(window);
      }
      return 0;
    case WM_CLOSE:
      DestroyWindow(window);
      return 0;
    case WM_DESTROY:
      KillTimer(window, kHealthTimer);
      SetTrayIcon(window, false);
      RemoveHook();
      DisconnectFilter();
      if (g_settings_window != nullptr) {
        DestroyWindow(g_settings_window);
      }
      g_broker_window = nullptr;
      PostQuitMessage(0);
      return 0;
  }
  return DefWindowProcW(window, message, wparam, lparam);
}

}  // namespace

int WINAPI wWinMain(const HINSTANCE instance, HINSTANCE, PWSTR, int) {
  const HANDLE mutex = CreateMutexW(nullptr, FALSE, kMutexName);
  if (mutex == nullptr || GetLastError() == ERROR_ALREADY_EXISTS) {
    if (mutex != nullptr) {
      CloseHandle(mutex);
    }
    return 0;
  }
  if (FAILED(CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED))) {
    CloseHandle(mutex);
    return 1;
  }
  WNDCLASSEXW broker_class{sizeof(broker_class)};
  broker_class.lpfnWndProc = WindowProcedure;
  broker_class.hInstance = instance;
  broker_class.hIcon = LoadIconW(nullptr, IDI_APPLICATION);
  broker_class.hCursor = LoadCursorW(nullptr, IDC_ARROW);
  broker_class.lpszClassName = FSW_BROKER_WINDOW_CLASS;
  WNDCLASSEXW settings_class{sizeof(settings_class)};
  settings_class.lpfnWndProc = SettingsWindowProcedure;
  settings_class.hInstance = instance;
  settings_class.hIcon = broker_class.hIcon;
  settings_class.hCursor = broker_class.hCursor;
  settings_class.hbrBackground = reinterpret_cast<HBRUSH>(COLOR_WINDOW + 1);
  settings_class.lpszClassName = kSettingsWindowClass;
  if (!RegisterClassExW(&broker_class) ||
      !RegisterClassExW(&settings_class)) {
    CoUninitialize();
    CloseHandle(mutex);
    return 1;
  }
  g_broker_window = CreateWindowExW(
      0, FSW_BROKER_WINDOW_CLASS, L"Forward Slash Windows", WS_OVERLAPPED,
      0, 0, 0, 0, HWND_MESSAGE, nullptr, instance, nullptr);
  if (g_broker_window == nullptr) {
    CoUninitialize();
    CloseHandle(mutex);
    return 1;
  }
  SetTrayIcon(g_broker_window, true);
  const bool hook_installed = InstallHook();
  PublishFilterMappings(true);
  SetTimer(g_broker_window, kHealthTimer, 5000, nullptr);
  if (!hook_installed) {
    ShowNotification(L"The shell keyboard hook could not be installed.",
                     NIIF_ERROR);
  }
  MSG message{};
  while (GetMessageW(&message, nullptr, 0, 0) > 0) {
    TranslateMessage(&message);
    DispatchMessageW(&message);
  }
  CoUninitialize();
  CloseHandle(mutex);
  return 0;
}
