#pragma once

#include <windows.h>

// Private, per-desktop controller protocol for the interactive user's broker.
inline constexpr wchar_t FSW_BROKER_WINDOW_CLASS[] =
    L"ForwardSlashWindows.Broker";
inline constexpr UINT FSW_WM_QUERY_STATE = WM_APP + 10;
inline constexpr UINT FSW_WM_SET_PAUSED = WM_APP + 11;
inline constexpr UINT FSW_WM_SHOW_SETTINGS = WM_APP + 12;

inline constexpr wchar_t FSW_SETTINGS_KEY[] =
    L"Software\\ForwardSlashWindows\\Settings";
inline constexpr wchar_t FSW_DISABLED_VALUE[] = L"Disabled";
inline constexpr wchar_t FSW_BARE_SLASH_MODE_VALUE[] = L"BareSlashMode";
inline constexpr wchar_t FSW_BARE_SLASH_DISTRIBUTION_VALUE[] =
    L"BareSlashDistribution";
// Custom bare-slash root: read only by the Rust resolver (fsw-core). A stale
// C++ build ignores it and keeps today's behavior for "/" — recorded in
// docs/divergences.md, resolver 6.
inline constexpr wchar_t FSW_BARE_SLASH_ROOT_VALUE[] = L"BareSlashRoot";

enum FSW_BROKER_STATE : LRESULT {
  FswBrokerUnavailable = 0,
  FswBrokerActive = 1,
  FswBrokerPaused = 2,
};
