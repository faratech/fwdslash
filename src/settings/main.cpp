#include "pch.h"

#include "App.xaml.h"

#include "fsw_user_protocol.h"
#include "fsw/wsl_registry.hpp"

#include <algorithm>
#include <cwctype>
#include <memory>

using namespace winrt;
using namespace Microsoft::UI::Xaml;
using namespace Microsoft::UI::Xaml::Automation;
using namespace Microsoft::UI::Xaml::Controls;
using namespace Microsoft::UI::Xaml::Media;
using namespace Microsoft::UI::Xaml::Media::Imaging;

namespace {

constexpr wchar_t kRunKey[] =
    L"Software\\Microsoft\\Windows\\CurrentVersion\\Run";
constexpr wchar_t kRunValue[] = L"ForwardSlashWindows";
constexpr wchar_t kCmdAdapterKey[] =
    L"Software\\ForwardSlashWindows\\CmdAdapter";
constexpr wchar_t kPowerShellAdapterRoot[] =
    L"Software\\ForwardSlashWindows\\PowerShellAdapter\\";

void LogFatalError(const std::wstring_view message) noexcept {
  wchar_t local_app_data[32768]{};
  const DWORD length = GetEnvironmentVariableW(
      L"LOCALAPPDATA", local_app_data,
      static_cast<DWORD>(_countof(local_app_data)));
  if (length == 0 || length >= _countof(local_app_data)) return;
  const std::wstring directory =
      std::wstring(local_app_data, length) + L"\\ForwardSlashWindows";
  CreateDirectoryW(directory.c_str(), nullptr);
  const std::wstring path = directory + L"\\settings-crash.log";
  const HANDLE file = CreateFileW(path.c_str(), GENERIC_WRITE, FILE_SHARE_READ,
                                  nullptr, CREATE_ALWAYS,
                                  FILE_ATTRIBUTE_NORMAL, nullptr);
  if (file == INVALID_HANDLE_VALUE) return;
  const int bytes = WideCharToMultiByte(CP_UTF8, 0, message.data(),
                                        static_cast<int>(message.size()),
                                        nullptr, 0, nullptr, nullptr);
  if (bytes > 0) {
    std::string utf8(static_cast<size_t>(bytes), '\0');
    WideCharToMultiByte(CP_UTF8, 0, message.data(),
                        static_cast<int>(message.size()), utf8.data(), bytes,
                        nullptr, nullptr);
    DWORD written = 0;
    WriteFile(file, utf8.data(), static_cast<DWORD>(utf8.size()), &written,
              nullptr);
  }
  CloseHandle(file);
}

std::wstring ExecutableDirectory() {
  std::wstring path(32768, L'\0');
  const DWORD length = GetModuleFileNameW(nullptr, path.data(),
                                          static_cast<DWORD>(path.size()));
  if (length == 0 || length >= path.size()) {
    return {};
  }
  path.resize(length);
  const size_t separator = path.find_last_of(L"\\/");
  return separator == std::wstring::npos ? std::wstring{}
                                         : path.substr(0, separator);
}

bool RegistryValuePresent(const std::wstring& path, const wchar_t* name) {
  HKEY key = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, path.c_str(), 0, KEY_QUERY_VALUE,
                    &key) != ERROR_SUCCESS) {
    return false;
  }
  const LSTATUS status = RegQueryValueExW(key, name, nullptr, nullptr, nullptr,
                                          nullptr);
  RegCloseKey(key);
  return status == ERROR_SUCCESS;
}

bool RegistryStringEquals(const std::wstring& path, const wchar_t* name,
                          const std::wstring_view expected) {
  HKEY key = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, path.c_str(), 0, KEY_QUERY_VALUE,
                    &key) != ERROR_SUCCESS) {
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

bool AdapterInstalled(const std::wstring& path) {
  return RegistryStringEquals(path, L"State", L"installed");
}

bool Disabled() {
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

bool ExecutableAvailable(const wchar_t* name) {
  std::wstring path(32768, L'\0');
  const DWORD length = SearchPathW(nullptr, name, nullptr,
                                   static_cast<DWORD>(path.size()), path.data(),
                                   nullptr);
  return length > 0 && length < path.size();
}

std::wstring Quote(const std::wstring_view value) {
  return L"\"" + std::wstring(value) + L"\"";
}

bool RunController(const std::wstring_view arguments) {
  const std::wstring directory = ExecutableDirectory();
  const std::wstring controller = directory + L"\\fwdslash.exe";
  std::wstring command = Quote(controller) + L" " + std::wstring(arguments);
  STARTUPINFOW startup{sizeof(startup)};
  PROCESS_INFORMATION process{};
  if (!CreateProcessW(controller.c_str(), command.data(), nullptr, nullptr,
                      FALSE, CREATE_NO_WINDOW, nullptr, directory.c_str(),
                      &startup, &process)) {
    return false;
  }
  CloseHandle(process.hThread);
  const DWORD wait = WaitForSingleObject(process.hProcess, 30000);
  DWORD exit_code = 1;
  if (wait == WAIT_OBJECT_0) {
    GetExitCodeProcess(process.hProcess, &exit_code);
  }
  CloseHandle(process.hProcess);
  return wait == WAIT_OBJECT_0 && exit_code == 0;
}

std::wstring InitialSection() {
  int count = 0;
  LPWSTR* values = CommandLineToArgvW(GetCommandLineW(), &count);
  std::wstring section = L"general";
  if (values != nullptr) {
    for (int index = 1; index < count; ++index) {
      std::wstring argument(values[index]);
      std::transform(argument.begin(), argument.end(), argument.begin(),
                     [](const wchar_t value) {
                       return static_cast<wchar_t>(std::towlower(value));
                     });
      constexpr std::wstring_view prefix = L"fwdslash://settings/";
      if (argument.starts_with(prefix) && argument.size() > prefix.size()) {
        section = argument.substr(prefix.size());
      } else if (argument == L"--section" && index + 1 < count) {
        section = values[++index];
      }
    }
    LocalFree(values);
  }
  std::transform(section.begin(), section.end(), section.begin(),
                 [](const wchar_t value) {
                   return static_cast<wchar_t>(std::towlower(value));
                 });
  const size_t suffix = section.find_first_of(L"/?#");
  if (suffix != std::wstring::npos) {
    section.resize(suffix);
  }
  return section;
}

void ApplyCardBrush(const Border& card) {
  const auto resources = Application::Current().Resources();
  const auto background = resources.TryLookup(
      box_value(hstring{L"CardBackgroundFillColorDefaultBrush"}));
  const auto border = resources.TryLookup(
      box_value(hstring{L"CardStrokeColorDefaultBrush"}));
  if (const auto brush = background.try_as<Brush>()) {
    card.Background(brush);
  }
  if (const auto brush = border.try_as<Brush>()) {
    card.BorderBrush(brush);
  }
}

TextBlock Text(const std::wstring_view value, const double size = 14.0,
               const bool semibold = false) {
  TextBlock block;
  block.Text(value);
  block.FontSize(size);
  block.TextWrapping(TextWrapping::Wrap);
  if (semibold) {
    block.FontWeight(Windows::UI::Text::FontWeight{600});
  }
  return block;
}

StackPanel PageHeader(const std::wstring_view title,
                      const std::wstring_view subtitle) {
  StackPanel header;
  header.Spacing(4);
  header.Children().Append(Text(title, 28.0, true));
  auto description = Text(subtitle);
  description.Opacity(0.72);
  header.Children().Append(description);
  return header;
}

Border ToggleCard(const std::wstring_view title,
                  const std::wstring_view description,
                  const ToggleSwitch& toggle) {
  Border card;
  card.Padding(Thickness{18.0, 18.0, 18.0, 18.0});
  card.CornerRadius(CornerRadius{8.0, 8.0, 8.0, 8.0});
  card.BorderThickness(Thickness{1.0, 1.0, 1.0, 1.0});
  ApplyCardBrush(card);

  Grid layout;
  layout.ColumnSpacing(20.0);
  ColumnDefinition content_column;
  content_column.Width(GridLength{1.0, GridUnitType::Star});
  ColumnDefinition toggle_column;
  toggle_column.Width(GridLength{1.0, GridUnitType::Auto});
  layout.ColumnDefinitions().Append(content_column);
  layout.ColumnDefinitions().Append(toggle_column);

  StackPanel copy;
  copy.Spacing(3.0);
  copy.Children().Append(Text(title, 14.0, true));
  auto caption = Text(description);
  caption.Opacity(0.70);
  copy.Children().Append(caption);
  layout.Children().Append(copy);
  Grid::SetColumn(toggle, 1);
  toggle.VerticalAlignment(VerticalAlignment::Center);
  layout.Children().Append(toggle);
  card.Child(layout);
  return card;
}

NavigationViewItem NavigationItem(const std::wstring_view title,
                                  const std::wstring_view tag,
                                  const Symbol symbol) {
  NavigationViewItem item;
  item.Content(box_value(hstring{title}));
  item.Tag(box_value(hstring{tag}));
  item.Icon(SymbolIcon{symbol});
  return item;
}

class SettingsWindow {
 public:
  SettingsWindow() { Build(); }

  Window GetWindow() const { return window_; }

 private:
  Window window_{nullptr};
  NavigationView navigation_{nullptr};
  NavigationViewItem general_item_{nullptr};
  NavigationViewItem windows_item_{nullptr};
  NavigationViewItem terminals_item_{nullptr};
  NavigationViewItem about_item_{nullptr};
  InfoBar notice_{nullptr};
  ScrollViewer general_panel_{nullptr};
  ScrollViewer windows_panel_{nullptr};
  ScrollViewer terminals_panel_{nullptr};
  ScrollViewer about_panel_{nullptr};
  TextBlock status_text_{nullptr};
  TextBlock powershell_caption_{nullptr};
  ToggleSwitch global_toggle_{nullptr};
  RadioButton list_mode_radio_{nullptr};
  RadioButton default_mode_radio_{nullptr};
  ComboBox default_distribution_{nullptr};
  StackPanel default_distribution_row_{nullptr};
  ToggleSwitch windows_toggle_{nullptr};
  ToggleSwitch cmd_toggle_{nullptr};
  ToggleSwitch windows_powershell_toggle_{nullptr};
  ToggleSwitch powershell_toggle_{nullptr};
  bool loading_{true};

  static StackPanel PageStack() {
    StackPanel stack;
    stack.Spacing(16.0);
    stack.MaxWidth(720.0);
    stack.HorizontalAlignment(HorizontalAlignment::Left);
    return stack;
  }

  static ScrollViewer Scroller(const StackPanel& content) {
    ScrollViewer viewer;
    viewer.VerticalScrollBarVisibility(ScrollBarVisibility::Auto);
    viewer.Content(content);
    return viewer;
  }

  void Build() {
    window_ = Window{};
    window_.Title(L"Forward Slash Windows");
    if (const auto native = window_.try_as<::IWindowNative>()) {
      HWND handle = nullptr;
      if (SUCCEEDED(native->get_WindowHandle(&handle)) && handle != nullptr) {
        const HINSTANCE instance = GetModuleHandleW(nullptr);
        const auto large = reinterpret_cast<HICON>(LoadImageW(
            instance, MAKEINTRESOURCEW(IDI_FSW_APP), IMAGE_ICON,
            GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON),
            LR_DEFAULTCOLOR | LR_SHARED));
        const auto small_icon = reinterpret_cast<HICON>(LoadImageW(
            instance, MAKEINTRESOURCEW(IDI_FSW_APP), IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR | LR_SHARED));
        if (large != nullptr) SendMessageW(handle, WM_SETICON, ICON_BIG,
                                           reinterpret_cast<LPARAM>(large));
        if (small_icon != nullptr) {
          SendMessageW(handle, WM_SETICON, ICON_SMALL,
                       reinterpret_cast<LPARAM>(small_icon));
        }
      }
    }
    try {
      window_.SystemBackdrop(MicaBackdrop{});
    } catch (const hresult_error&) {
      // The supported fallback is the normal themed window background.
    }

    Grid window_root;
    RowDefinition title_row;
    title_row.Height(GridLength{1.0, GridUnitType::Auto});
    RowDefinition body_row;
    body_row.Height(GridLength{1.0, GridUnitType::Star});
    window_root.RowDefinitions().Append(title_row);
    window_root.RowDefinitions().Append(body_row);

    TitleBar title_bar;
    title_bar.Title(L"Forward Slash Windows");
    title_bar.IsPaneToggleButtonVisible(false);
    ImageIconSource title_icon;
    BitmapImage title_bitmap;
    title_bitmap.UriSource(
        Windows::Foundation::Uri{L"ms-appx:///Assets/fwdslash-titlebar.png"});
    title_icon.ImageSource(title_bitmap);
    title_bar.IconSource(title_icon);
    window_root.Children().Append(title_bar);

    navigation_ = NavigationView{};
    navigation_.PaneDisplayMode(NavigationViewPaneDisplayMode::LeftCompact);
    navigation_.IsBackButtonVisible(NavigationViewBackButtonVisible::Collapsed);
    navigation_.IsSettingsVisible(false);
    navigation_.IsPaneOpen(false);
    navigation_.OpenPaneLength(190.0);
    general_item_ = NavigationItem(L"General", L"general", Symbol::Home);
    windows_item_ = NavigationItem(L"Windows", L"windows", Symbol::Folder);
    terminals_item_ = NavigationItem(L"Terminals", L"terminals", Symbol::AllApps);
    about_item_ = NavigationItem(L"About", L"about", Symbol::Help);
    navigation_.MenuItems().Append(general_item_);
    navigation_.MenuItems().Append(windows_item_);
    navigation_.MenuItems().Append(terminals_item_);
    navigation_.MenuItems().Append(about_item_);

    Grid surface;
    surface.Padding(Thickness{32.0, 24.0, 32.0, 28.0});
    RowDefinition notice_row;
    notice_row.Height(GridLength{1.0, GridUnitType::Auto});
    RowDefinition content_row;
    content_row.Height(GridLength{1.0, GridUnitType::Star});
    surface.RowDefinitions().Append(notice_row);
    surface.RowDefinitions().Append(content_row);
    notice_ = InfoBar{};
    notice_.IsOpen(false);
    notice_.IsClosable(true);
    notice_.Margin(Thickness{0.0, 0.0, 0.0, 16.0});
    surface.Children().Append(notice_);

    BuildGeneral(surface);
    BuildWindows(surface);
    BuildTerminals(surface);
    BuildAbout(surface);
    navigation_.Content(surface);
    Grid::SetRow(navigation_, 1);
    window_root.Children().Append(navigation_);
    window_.Content(window_root);
    window_.ExtendsContentIntoTitleBar(true);
    window_.SetTitleBar(title_bar);

    navigation_.SelectionChanged(
        [this](NavigationView const&,
               NavigationViewSelectionChangedEventArgs const& args) {
          const auto item = args.SelectedItem().try_as<NavigationViewItem>();
          if (item != nullptr && item.Tag() != nullptr) {
            ShowSection(unbox_value<hstring>(item.Tag()).c_str());
          }
        });
    window_.Activated([this](winrt::Windows::Foundation::IInspectable const&,
                             WindowActivatedEventArgs const&) {
      RefreshState();
    });

    RefreshState();
    ShowSection(InitialSection());
    loading_ = false;
    window_.Activate();
  }

  void BuildGeneral(const Grid& surface) {
    StackPanel stack = PageStack();
    stack.Children().Append(PageHeader(
        L"Forward Slash Windows",
        L"Use Linux-style WSL paths in the Windows places you choose."));
    global_toggle_ = ToggleSwitch{};
    global_toggle_.OnContent(box_value(L"Enabled"));
    global_toggle_.OffContent(box_value(L"Disabled"));
    AutomationProperties::SetName(global_toggle_,
                                  L"Enable forward-slash resolution");
    global_toggle_.Toggled([this](winrt::Windows::Foundation::IInspectable const&,
                                  RoutedEventArgs const&) {
      if (loading_) return;
      const bool enabled = global_toggle_.IsOn();
      const bool succeeded = RunController(enabled ? L"enable" : L"disable");
      ShowResult(succeeded, enabled ? L"Resolution enabled"
                                    : L"Resolution disabled", false);
      RefreshState();
    });
    stack.Children().Append(ToggleCard(
        L"Forward-slash resolution",
        L"Disable temporarily without removing selected integrations.",
        global_toggle_));

    Border bare_card;
    bare_card.Padding(Thickness{18.0, 18.0, 18.0, 18.0});
    bare_card.CornerRadius(CornerRadius{8.0, 8.0, 8.0, 8.0});
    bare_card.BorderThickness(Thickness{1.0, 1.0, 1.0, 1.0});
    ApplyCardBrush(bare_card);
    StackPanel bare_content;
    bare_content.Spacing(8.0);
    bare_content.Children().Append(Text(L"Bare slash ( / ) behavior", 14.0, true));
    auto bare_caption = Text(L"Choose what typing only / means on enabled surfaces.");
    bare_caption.Opacity(0.70);
    bare_content.Children().Append(bare_caption);
    list_mode_radio_ = RadioButton{};
    list_mode_radio_.Content(box_value(L"Show all distributions"));
    list_mode_radio_.GroupName(L"BareSlashMode");
    AutomationProperties::SetName(list_mode_radio_, L"Show all distributions");
    list_mode_radio_.Checked(
        [this](winrt::Windows::Foundation::IInspectable const&,
               RoutedEventArgs const&) {
          if (loading_) return;
          const bool succeeded = RunController(L"bare-slash list");
          ShowResult(succeeded, L"Bare slash shows all distributions", false);
          RefreshState();
        });
    bare_content.Children().Append(list_mode_radio_);
    default_mode_radio_ = RadioButton{};
    default_mode_radio_.Content(box_value(L"Open my default distribution"));
    default_mode_radio_.GroupName(L"BareSlashMode");
    AutomationProperties::SetName(default_mode_radio_,
                                  L"Open my default distribution");
    default_mode_radio_.Checked(
        [this](winrt::Windows::Foundation::IInspectable const&,
               RoutedEventArgs const&) {
          if (loading_) return;
          const bool succeeded = RunController(L"bare-slash default");
          ShowResult(succeeded, L"Bare slash opens the default distribution",
                     false);
          RefreshState();
        });
    bare_content.Children().Append(default_mode_radio_);
    StackPanel picker_row;
    picker_row.Orientation(Orientation::Horizontal);
    picker_row.Spacing(8.0);
    auto picker_label = Text(L"Distribution:");
    picker_label.VerticalAlignment(VerticalAlignment::Center);
    default_distribution_ = ComboBox{};
    default_distribution_.MinWidth(240.0);
    AutomationProperties::SetName(default_distribution_,
                                  L"Default distribution for bare slash");
    default_distribution_.SelectionChanged(
        [this](winrt::Windows::Foundation::IInspectable const&,
               SelectionChangedEventArgs const&) {
          if (loading_) return;
          const int32_t index = default_distribution_.SelectedIndex();
          bool succeeded;
          if (index <= 0) {
            succeeded = RunController(L"bare-slash default");
          } else {
            const auto name = unbox_value<hstring>(
                default_distribution_.Items().GetAt(index));
            succeeded = RunController(std::wstring(L"bare-slash default \"") +
                                      std::wstring(name) + L"\"");
          }
          ShowResult(succeeded, L"Bare slash default updated", false);
          RefreshState();
        });
    picker_row.Children().Append(picker_label);
    picker_row.Children().Append(default_distribution_);
    default_distribution_row_ = picker_row;
    bare_content.Children().Append(picker_row);
    bare_card.Child(bare_content);
    stack.Children().Append(bare_card);

    StackPanel buttons;
    buttons.Orientation(Orientation::Horizontal);
    buttons.Spacing(12.0);
    Button open_root;
    open_root.Content(box_value(L"Open WSL root"));
    open_root.Click([this](winrt::Windows::Foundation::IInspectable const&,
                           RoutedEventArgs const&) {
      if (reinterpret_cast<INT_PTR>(ShellExecuteW(
              nullptr, L"open", L"\\\\wsl.localhost", nullptr, nullptr,
              SW_SHOWNORMAL)) <= 32) {
        ShowResult(false, L"Opening the WSL root", false);
      }
    });
    Button refresh;
    refresh.Content(box_value(L"Refresh status"));
    refresh.Click([this](winrt::Windows::Foundation::IInspectable const&,
                         RoutedEventArgs const&) {
      RefreshState();
      ShowResult(true, L"Status refreshed", false);
    });
    buttons.Children().Append(open_root);
    buttons.Children().Append(refresh);
    stack.Children().Append(buttons);
    status_text_ = Text(L"");
    status_text_.Opacity(0.78);
    stack.Children().Append(status_text_);
    general_panel_ = Scroller(stack);
    Grid::SetRow(general_panel_, 1);
    surface.Children().Append(general_panel_);
  }

  void BuildWindows(const Grid& surface) {
    StackPanel stack = PageStack();
    stack.Children().Append(PageHeader(
        L"Windows surfaces",
        L"Native navigation through the address bar and shell entry points."));
    windows_toggle_ = ToggleSwitch{};
    AutomationProperties::SetName(windows_toggle_,
                                  L"Install Windows surface integration");
    windows_toggle_.Toggled(
        [this](winrt::Windows::Foundation::IInspectable const&,
               RoutedEventArgs const&) {
          if (loading_) return;
          const bool enabled = windows_toggle_.IsOn();
          const bool succeeded = ApplyIntegration(L"windows", enabled);
          ShowResult(succeeded, enabled ? L"Windows surfaces installed"
                                        : L"Windows surfaces removed", false);
          RefreshState();
        });
    stack.Children().Append(ToggleCard(
        L"Explorer, Run, and Search",
        L"Installs the per-user broker and startup entry. Turning this off "
        L"stops the broker and removes its startup registration.",
        windows_toggle_));
    auto note = Text(L"Invalid slash paths are blocked instead of being sent "
                     L"to Edge or web search.");
    note.Opacity(0.72);
    stack.Children().Append(note);
    windows_panel_ = Scroller(stack);
    Grid::SetRow(windows_panel_, 1);
    surface.Children().Append(windows_panel_);
  }

  void BuildTerminals(const Grid& surface) {
    StackPanel stack = PageStack();
    stack.Spacing(12.0);
    stack.Children().Append(PageHeader(
        L"Terminal integrations",
        L"Each shell is independent and can be removed without changing the others."));

    cmd_toggle_ = ToggleSwitch{};
    AutomationProperties::SetName(cmd_toggle_,
                                  L"Install Command Prompt integration");
    cmd_toggle_.Toggled([this](winrt::Windows::Foundation::IInspectable const&,
                               RoutedEventArgs const&) {
      if (loading_) return;
      const bool enabled = cmd_toggle_.IsOn();
      const bool succeeded = ApplyIntegration(L"cmd", enabled);
      ShowResult(succeeded, enabled ? L"Command Prompt installed"
                                    : L"Command Prompt removed", true);
      RefreshState();
    });
    stack.Children().Append(ToggleCard(
        L"Command Prompt",
        L"Adds reversible dir and ls DOSKEY adapters for new cmd.exe sessions.",
        cmd_toggle_));

    windows_powershell_toggle_ = ToggleSwitch{};
    AutomationProperties::SetName(windows_powershell_toggle_,
                                  L"Install Windows PowerShell integration");
    windows_powershell_toggle_.Toggled(
        [this](winrt::Windows::Foundation::IInspectable const&,
               RoutedEventArgs const&) {
          if (loading_) return;
          const bool enabled = windows_powershell_toggle_.IsOn();
          const bool succeeded = ApplyIntegration(L"windows-powershell", enabled);
          ShowResult(succeeded, enabled ? L"Windows PowerShell installed"
                                        : L"Windows PowerShell removed", true);
          RefreshState();
        });
    stack.Children().Append(ToggleCard(
        L"Windows PowerShell 5.1",
        L"Adds a guarded profile import and preserves normal Get-ChildItem behavior.",
        windows_powershell_toggle_));

    powershell_toggle_ = ToggleSwitch{};
    AutomationProperties::SetName(powershell_toggle_,
                                  L"Install PowerShell 7 integration");
    powershell_toggle_.Toggled(
        [this](winrt::Windows::Foundation::IInspectable const&,
               RoutedEventArgs const&) {
          if (loading_) return;
          const bool enabled = powershell_toggle_.IsOn();
          const bool succeeded = ApplyIntegration(L"powershell", enabled);
          ShowResult(succeeded, enabled ? L"PowerShell 7 installed"
                                        : L"PowerShell 7 removed", true);
          RefreshState();
        });
    Border powershell_card = ToggleCard(
        L"PowerShell 7", L"Adds the same reversible adapter to the PowerShell 7 profile.",
        powershell_toggle_);
    const auto layout = powershell_card.Child().as<Grid>();
    const auto copy = layout.Children().GetAt(0).as<StackPanel>();
    powershell_caption_ = copy.Children().GetAt(1).as<TextBlock>();
    stack.Children().Append(powershell_card);

    auto note = Text(L"Profile and AutoRun changes apply to newly opened terminal "
                     L"sessions. Existing sessions retain what they already loaded.");
    note.Opacity(0.72);
    note.Margin(Thickness{0.0, 4.0, 0.0, 0.0});
    stack.Children().Append(note);
    terminals_panel_ = Scroller(stack);
    Grid::SetRow(terminals_panel_, 1);
    surface.Children().Append(terminals_panel_);
  }

  void BuildAbout(const Grid& surface) {
    StackPanel stack = PageStack();
    stack.Children().Append(PageHeader(L"About", L"Forward Slash Windows 0.0.1"));
    stack.Children().Append(Text(
        L"Maps /Distro/path to \\\\wsl.localhost\\Distro\\path, and / to "
        L"either the WSL distribution list or your default distribution, on "
        L"supported Windows surfaces."));
    auto driver = Text(L"The filesystem minifilter remains production-gated "
                       L"and is not installed by this app.");
    driver.Opacity(0.76);
    stack.Children().Append(driver);

    Border identity;
    identity.Padding(Thickness{18.0, 16.0, 18.0, 16.0});
    identity.CornerRadius(CornerRadius{8.0, 8.0, 8.0, 8.0});
    identity.BorderThickness(Thickness{1.0, 1.0, 1.0, 1.0});
    ApplyCardBrush(identity);
    StackPanel identity_content;
    identity_content.Spacing(4.0);
    identity_content.Children().Append(Text(L"Mike Fara", 16.0, true));
    identity_content.Children().Append(Text(L"Fara Technologies LLC"));
    auto location = Text(L"New York, United States");
    location.Opacity(0.72);
    identity_content.Children().Append(location);
    identity.Child(identity_content);
    stack.Children().Append(identity);

    StackPanel links;
    links.Orientation(Orientation::Horizontal);
    links.Spacing(8.0);
    HyperlinkButton github;
    github.Content(box_value(L"GitHub repository"));
    github.NavigateUri(Windows::Foundation::Uri{
        L"https://github.com/faratech/fwdslash"});
    HyperlinkButton license;
    license.Content(box_value(L"MIT License"));
    license.NavigateUri(Windows::Foundation::Uri{
        L"https://github.com/faratech/fwdslash/blob/main/LICENSE"});
    links.Children().Append(github);
    links.Children().Append(license);
    stack.Children().Append(links);
    auto open_source = Text(L"Open-source software licensed under the MIT License.");
    open_source.Opacity(0.72);
    stack.Children().Append(open_source);
    about_panel_ = Scroller(stack);
    Grid::SetRow(about_panel_, 1);
    surface.Children().Append(about_panel_);
  }

  bool ApplyIntegration(const std::wstring_view integration,
                        const bool enabled) {
    const std::wstring arguments = L"integration " +
        std::wstring(integration) + (enabled ? L" enable" : L" disable");
    return RunController(arguments);
  }

  void RefreshState() {
    loading_ = true;
    const bool disabled = Disabled();
    const bool windows = RegistryValuePresent(kRunKey, kRunValue);
    const bool cmd = AdapterInstalled(kCmdAdapterKey);
    const bool windows_powershell = AdapterInstalled(
        std::wstring(kPowerShellAdapterRoot) + L"WindowsPowerShell");
    const bool powershell = AdapterInstalled(
        std::wstring(kPowerShellAdapterRoot) + L"PowerShell");
    const bool powershell_available = ExecutableAvailable(L"pwsh.exe");

    global_toggle_.IsOn(!disabled);
    windows_toggle_.IsOn(windows);
    cmd_toggle_.IsOn(cmd);
    windows_powershell_toggle_.IsOn(windows_powershell);
    powershell_toggle_.IsOn(powershell);
    powershell_toggle_.IsEnabled(powershell_available || powershell);
    powershell_caption_.Text(
        powershell_available
            ? L"Adds the same reversible adapter to the PowerShell 7 profile."
            : L"PowerShell 7 is not installed on this computer.");

    const fsw::BareSlashMode bare_mode = fsw::GetBareSlashMode();
    const std::wstring pinned = fsw::GetBareSlashOverride();
    const auto bare_distributions = fsw::ListRegisteredDistributions();
    const auto wsl_default = fsw::GetDefaultDistribution(bare_distributions);
    using winrt::Windows::Foundation::IReference;
    const bool list_mode =
        bare_mode == fsw::BareSlashMode::distribution_list;
    list_mode_radio_.IsChecked(
        winrt::box_value(list_mode).as<IReference<bool>>());
    default_mode_radio_.IsChecked(
        winrt::box_value(!list_mode).as<IReference<bool>>());
    default_distribution_.Items().Clear();
    default_distribution_.Items().Append(box_value(wsl_default.has_value()
        ? winrt::hstring(L"Windows default (/" + *wsl_default + L")")
        : winrt::hstring(L"Windows default")));
    int32_t selected_distribution = 0;
    for (size_t index = 0; index < bare_distributions.size(); ++index) {
      default_distribution_.Items().Append(
          box_value(winrt::hstring(bare_distributions[index])));
      if (!pinned.empty() &&
          fsw::EqualsOrdinalIgnoreCase(bare_distributions[index], pinned)) {
        selected_distribution = static_cast<int32_t>(index + 1);
      }
    }
    default_distribution_.SelectedIndex(selected_distribution);
    default_distribution_row_.Visibility(
        bare_mode == fsw::BareSlashMode::default_distribution
            ? Visibility::Visible
            : Visibility::Collapsed);

    const HWND broker_window = FindWindowW(FSW_BROKER_WINDOW_CLASS, nullptr);
    const FSW_BROKER_STATE broker = [broker_window] {
      if (broker_window == nullptr) return FswBrokerUnavailable;
      DWORD_PTR result = 0;
      if (!SendMessageTimeoutW(broker_window, FSW_WM_QUERY_STATE, 0, 0,
                               SMTO_ABORTIFHUNG | SMTO_BLOCK, 750, &result)) {
        return FswBrokerUnavailable;
      }
      return static_cast<FSW_BROKER_STATE>(result);
    }();
    std::wstring status = L"Windows broker: ";
    status.append(!windows ? L"not installed"
                  : broker == FswBrokerActive ? L"active"
                  : broker == FswBrokerPaused ? L"disabled"
                  : broker_window != nullptr ? L"hook unavailable"
                                             : L"stopped");
    status.append(L"\nFilesystem driver: not installed (production-gated)");
    status_text_.Text(status);
    loading_ = false;
  }

  void ShowSection(const std::wstring_view section) {
    const bool terminals = section == L"terminals" || section == L"cmd" ||
                           section == L"windows-powershell" ||
                           section == L"powershell";
    general_panel_.Visibility(section == L"general" ? Visibility::Visible
                                                     : Visibility::Collapsed);
    windows_panel_.Visibility(section == L"windows" ? Visibility::Visible
                                                     : Visibility::Collapsed);
    terminals_panel_.Visibility(terminals ? Visibility::Visible
                                          : Visibility::Collapsed);
    about_panel_.Visibility(section == L"about" ? Visibility::Visible
                                                 : Visibility::Collapsed);
    if (section == L"windows") {
      navigation_.SelectedItem(windows_item_);
      windows_toggle_.Focus(FocusState::Programmatic);
    } else if (terminals) {
      navigation_.SelectedItem(terminals_item_);
      if (section == L"cmd") cmd_toggle_.Focus(FocusState::Programmatic);
      if (section == L"windows-powershell") {
        windows_powershell_toggle_.Focus(FocusState::Programmatic);
      }
      if (section == L"powershell") {
        powershell_toggle_.Focus(FocusState::Programmatic);
      }
    } else if (section == L"about") {
      navigation_.SelectedItem(about_item_);
    } else {
      navigation_.SelectedItem(general_item_);
    }
  }

  void ShowResult(const bool succeeded, const std::wstring_view action,
                  const bool terminal) {
    notice_.Severity(succeeded ? InfoBarSeverity::Success
                               : InfoBarSeverity::Error);
    notice_.Title(succeeded ? L"Updated" : L"Could not update integration");
    std::wstring message(action);
    if (succeeded && terminal) {
      message.append(L". Reopen affected terminals.");
    } else if (!succeeded) {
      message.append(L" failed. Existing settings were left in place.");
    }
    notice_.Message(message);
    notice_.IsOpen(true);
  }
};

}  // namespace

namespace winrt::fswsettings::implementation {

App::App() {
  UnhandledException([](winrt::Windows::Foundation::IInspectable const&,
                        UnhandledExceptionEventArgs const& args) {
    LogFatalError(args.Message().c_str());
  });
}

void App::OnLaunched(LaunchActivatedEventArgs const&) {
  try {
    settings_window_ = std::make_shared<SettingsWindow>();
  } catch (const hresult_error& error) {
    LogFatalError(error.message().c_str());
    throw;
  } catch (const std::exception& error) {
    const int chars = MultiByteToWideChar(CP_UTF8, 0, error.what(), -1,
                                          nullptr, 0);
    if (chars > 1) {
      std::wstring message(static_cast<size_t>(chars), L'\0');
      MultiByteToWideChar(CP_UTF8, 0, error.what(), -1, message.data(), chars);
      message.resize(static_cast<size_t>(chars - 1));
      LogFatalError(message);
    }
    throw;
  }
}

}  // namespace winrt::fswsettings::implementation
