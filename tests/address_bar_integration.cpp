#include <windows.h>
#include <ole2.h>
#include <oleauto.h>
#include <oaidl.h>
#include <ocidl.h>
#include <exdisp.h>
#include <shellapi.h>

#include <chrono>
#include <iostream>
#include <set>
#include <string>
#include <thread>
#include <vector>

namespace {

struct ExplorerWindow {
  HWND window = nullptr;
  std::wstring location_url;
};

std::vector<ExplorerWindow> ListExplorerWindows(IShellWindows* shell_windows) {
  std::vector<ExplorerWindow> result;
  long count = 0;
  if (FAILED(shell_windows->get_Count(&count))) {
    return result;
  }
  for (long index = 0; index < count; ++index) {
    VARIANT item{};
    VariantInit(&item);
    item.vt = VT_I4;
    item.lVal = index;
    IDispatch* dispatch = nullptr;
    if (FAILED(shell_windows->Item(item, &dispatch)) || dispatch == nullptr) {
      continue;
    }
    IWebBrowserApp* browser = nullptr;
    if (SUCCEEDED(dispatch->QueryInterface(IID_PPV_ARGS(&browser))) &&
        browser != nullptr) {
      SHANDLE_PTR handle = 0;
      BSTR location = nullptr;
      if (SUCCEEDED(browser->get_HWND(&handle)) &&
          SUCCEEDED(browser->get_LocationURL(&location))) {
        result.push_back(
            {reinterpret_cast<HWND>(handle),
             location == nullptr ? std::wstring() : std::wstring(location)});
      }
      if (location != nullptr) {
        SysFreeString(location);
      }
      browser->Release();
    }
    dispatch->Release();
  }
  return result;
}

bool SendKey(const WORD virtual_key, const DWORD flags = 0) {
  INPUT input{};
  input.type = INPUT_KEYBOARD;
  input.ki.wVk = virtual_key;
  input.ki.dwFlags = flags;
  return SendInput(1, &input, sizeof(input)) == 1;
}

bool ActivateWindow(const HWND window) {
  ShowWindow(window, SW_RESTORE);
  const DWORD current_thread = GetCurrentThreadId();
  const DWORD target_thread = GetWindowThreadProcessId(window, nullptr);
  const HWND foreground = GetForegroundWindow();
  const DWORD foreground_thread =
      foreground == nullptr ? 0 : GetWindowThreadProcessId(foreground, nullptr);
  const bool attached_target =
      target_thread != current_thread &&
      AttachThreadInput(current_thread, target_thread, TRUE) != FALSE;
  const bool attached_foreground =
      foreground_thread != 0 && foreground_thread != current_thread &&
      foreground_thread != target_thread &&
      AttachThreadInput(current_thread, foreground_thread, TRUE) != FALSE;
  BringWindowToTop(window);
  SetForegroundWindow(window);
  SetActiveWindow(window);
  if (attached_foreground) {
    AttachThreadInput(current_thread, foreground_thread, FALSE);
  }
  if (attached_target) {
    AttachThreadInput(current_thread, target_thread, FALSE);
  }
  return GetForegroundWindow() == window;
}

bool FocusAddressBarAndType(const HWND window, const std::wstring& text) {
  if (!ActivateWindow(window)) {
    std::wcerr << L"Unable to foreground the test Explorer window.\n";
    return false;
  }
  std::this_thread::sleep_for(std::chrono::milliseconds(250));
  if (!SendKey(VK_CONTROL) || !SendKey('L') ||
      !SendKey('L', KEYEVENTF_KEYUP) ||
      !SendKey(VK_CONTROL, KEYEVENTF_KEYUP)) {
    return false;
  }
  std::this_thread::sleep_for(std::chrono::milliseconds(250));
  for (const wchar_t character : text) {
    INPUT inputs[2]{};
    inputs[0].type = INPUT_KEYBOARD;
    inputs[0].ki.wScan = character;
    inputs[0].ki.dwFlags = KEYEVENTF_UNICODE;
    inputs[1] = inputs[0];
    inputs[1].ki.dwFlags |= KEYEVENTF_KEYUP;
    if (SendInput(2, inputs, sizeof(INPUT)) != 2) {
      return false;
    }
  }
  std::this_thread::sleep_for(std::chrono::milliseconds(300));
  return SendKey(VK_RETURN) && SendKey(VK_RETURN, KEYEVENTF_KEYUP);
}

bool IsExpectedLocation(const std::wstring& url,
                        const std::wstring& distribution,
                        const std::wstring& linux_path,
                        const bool wsl_root) {
  std::wstring expected = wsl_root ? L"file://wsl.localhost/"
                                   : L"file://wsl.localhost/" + distribution;
  for (const wchar_t character : linux_path) {
    expected.push_back(character == L'\\' ? L'/' : character);
  }
  return url.size() >= expected.size() &&
         CompareStringOrdinal(url.data(), static_cast<int>(expected.size()),
                              expected.data(), static_cast<int>(expected.size()),
                              TRUE) == CSTR_EQUAL;
}

}  // namespace

int wmain(const int argc, wchar_t** argv) {
  if (argc != 2 && argc != 3) {
    std::wcerr <<
        L"Usage: fsw_address_bar_integration.exe <distribution> [/path]\n";
    return 2;
  }
  const HRESULT initialized =
      CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
  if (FAILED(initialized)) {
    return 3;
  }
  IShellWindows* shell_windows = nullptr;
  if (FAILED(CoCreateInstance(CLSID_ShellWindows, nullptr,
                              CLSCTX_LOCAL_SERVER, IID_PPV_ARGS(&shell_windows))) ||
      shell_windows == nullptr) {
    CoUninitialize();
    return 4;
  }

  const auto before = ListExplorerWindows(shell_windows);
  std::set<HWND> original_windows;
  for (const auto& window : before) {
    original_windows.insert(window.window);
  }
  SHELLEXECUTEINFOW execute{sizeof(execute)};
  execute.lpVerb = L"open";
  execute.lpFile = L"explorer.exe";
  execute.lpParameters = L"/n,C:\\";
  execute.nShow = SW_SHOWNORMAL;
  if (!ShellExecuteExW(&execute)) {
    shell_windows->Release();
    CoUninitialize();
    return 5;
  }

  HWND test_window = nullptr;
  const auto launch_deadline =
      std::chrono::steady_clock::now() + std::chrono::seconds(10);
  while (std::chrono::steady_clock::now() < launch_deadline &&
         test_window == nullptr) {
    for (const auto& window : ListExplorerWindows(shell_windows)) {
      if (!original_windows.contains(window.window)) {
        test_window = window.window;
        break;
      }
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
  }
  const bool default_root = argc == 3 && std::wstring_view(argv[2]) == L"--root";
  const std::wstring linux_path =
      argc == 3 && !default_root ? argv[2] : L"";
  const std::wstring typed_path =
      default_root ? L"/" : L"/" + std::wstring(argv[1]) + linux_path;
  if (test_window == nullptr ||
      !FocusAddressBarAndType(test_window, typed_path)) {
    if (test_window == nullptr) {
      std::wcerr << L"Explorer did not create a distinct test window.\n";
    }
    shell_windows->Release();
    CoUninitialize();
    return 6;
  }

  bool passed = false;
  const auto navigation_deadline =
      std::chrono::steady_clock::now() + std::chrono::seconds(15);
  while (std::chrono::steady_clock::now() < navigation_deadline && !passed) {
    for (const auto& window : ListExplorerWindows(shell_windows)) {
      if (window.window == test_window &&
          IsExpectedLocation(window.location_url, argv[1], linux_path,
                             default_root)) {
        std::wcout << L"Explorer address bar opened " << window.location_url
                   << L"\n";
        passed = true;
        break;
      }
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(200));
  }

  for (const auto& window : ListExplorerWindows(shell_windows)) {
    if (!original_windows.contains(window.window)) {
      PostMessageW(window.window, WM_CLOSE, 0, 0);
    }
  }
  shell_windows->Release();
  CoUninitialize();
  if (!passed) {
    std::wcerr << L"Explorer did not navigate to the WSL location.\n";
    return 7;
  }
  return 0;
}
