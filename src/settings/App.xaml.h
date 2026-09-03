#pragma once

#include "App.xaml.g.h"

namespace winrt::fswsettings::implementation {

struct App : AppT<App> {
  App();
  void OnLaunched(Microsoft::UI::Xaml::LaunchActivatedEventArgs const&);

 private:
  std::shared_ptr<void> settings_window_;
};

}  // namespace winrt::fswsettings::implementation
