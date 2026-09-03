#include "fsw/wsl_registry.hpp"

#include "fsw/path_resolver.hpp"
#include "fsw_user_protocol.h"

#include <windows.h>

#include <algorithm>
#include <optional>

namespace fsw {
namespace {

constexpr wchar_t kLxssKey[] =
    L"Software\\Microsoft\\Windows\\CurrentVersion\\Lxss";

std::optional<std::wstring> ReadStringValue(const HKEY key,
                                            const wchar_t* value_name) {
  DWORD type = 0;
  DWORD byte_count = 0;
  if (RegQueryValueExW(key, value_name, nullptr, &type, nullptr, &byte_count) !=
          ERROR_SUCCESS ||
      (type != REG_SZ && type != REG_EXPAND_SZ) ||
      byte_count <= sizeof(wchar_t)) {
    return std::nullopt;
  }
  std::vector<wchar_t> value(byte_count / sizeof(wchar_t) + 1, L'\0');
  if (RegQueryValueExW(key, value_name, nullptr, &type,
                       reinterpret_cast<BYTE*>(value.data()), &byte_count) !=
      ERROR_SUCCESS) {
    return std::nullopt;
  }
  return std::wstring(value.data());
}

std::optional<DWORD> ReadDwordValue(const HKEY key, const wchar_t* value_name) {
  DWORD type = 0;
  DWORD value = 0;
  DWORD byte_count = sizeof(value);
  if (RegQueryValueExW(key, value_name, nullptr, &type,
                       reinterpret_cast<BYTE*>(&value), &byte_count) !=
          ERROR_SUCCESS ||
      type != REG_DWORD || byte_count != sizeof(value)) {
    return std::nullopt;
  }
  return value;
}

bool ContainsOrdinalIgnoreCase(const std::vector<std::wstring>& values,
                               const std::wstring_view candidate) {
  return std::any_of(values.begin(), values.end(),
                     [&candidate](const std::wstring& value) {
                       return EqualsOrdinalIgnoreCase(value, candidate);
                     });
}

}  // namespace

std::vector<std::wstring> ListRegisteredDistributions() {
  std::vector<std::wstring> distributions;
  HKEY root = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, kLxssKey, 0, KEY_READ, &root) !=
      ERROR_SUCCESS) {
    return distributions;
  }

  DWORD maximum_subkey_length = 0;
  if (RegQueryInfoKeyW(root, nullptr, nullptr, nullptr, nullptr,
                       &maximum_subkey_length, nullptr, nullptr, nullptr,
                       nullptr, nullptr, nullptr) != ERROR_SUCCESS) {
    RegCloseKey(root);
    return distributions;
  }

  DWORD index = 0;
  for (;;) {
    std::vector<wchar_t> subkey_name(maximum_subkey_length + 2, L'\0');
    DWORD subkey_length = static_cast<DWORD>(subkey_name.size());
    const LSTATUS enumeration = RegEnumKeyExW(
        root, index++, subkey_name.data(), &subkey_length, nullptr, nullptr,
        nullptr, nullptr);
    if (enumeration == ERROR_NO_MORE_ITEMS) {
      break;
    }
    if (enumeration != ERROR_SUCCESS) {
      continue;
    }

    HKEY subkey = nullptr;
    if (RegOpenKeyExW(root, subkey_name.data(), 0, KEY_QUERY_VALUE, &subkey) !=
        ERROR_SUCCESS) {
      continue;
    }
    const auto distribution = ReadStringValue(subkey, L"DistributionName");
    if (distribution.has_value()) {
      distributions.push_back(*distribution);
    }
    RegCloseKey(subkey);
  }
  RegCloseKey(root);
  return distributions;
}

bool IsRegisteredDistribution(const std::wstring_view distribution) {
  for (const std::wstring& candidate : ListRegisteredDistributions()) {
    if (EqualsOrdinalIgnoreCase(candidate, distribution)) {
      return true;
    }
  }
  return false;
}

std::optional<std::wstring> GetDefaultDistribution(
    const std::vector<std::wstring>& registered_distributions) {
  HKEY root = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, kLxssKey, 0, KEY_READ, &root) ==
      ERROR_SUCCESS) {
    const auto default_key = ReadStringValue(root, L"DefaultDistribution");
    if (default_key.has_value()) {
      HKEY distribution_key = nullptr;
      if (RegOpenKeyExW(root, default_key->c_str(), 0, KEY_READ,
                        &distribution_key) == ERROR_SUCCESS) {
        const auto distribution =
            ReadStringValue(distribution_key, L"DistributionName");
        RegCloseKey(distribution_key);
        if (distribution.has_value() && ContainsOrdinalIgnoreCase(
                                            registered_distributions,
                                            *distribution)) {
          RegCloseKey(root);
          return distribution;
        }
      }
    }
    RegCloseKey(root);
  }
  if (registered_distributions.size() == 1) {
    return registered_distributions.front();
  }
  return std::nullopt;
}

BareSlashMode GetBareSlashMode() {
  HKEY key = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, FSW_SETTINGS_KEY, 0, KEY_QUERY_VALUE,
                    &key) != ERROR_SUCCESS) {
    return BareSlashMode::distribution_list;
  }
  const auto value = ReadDwordValue(key, FSW_BARE_SLASH_MODE_VALUE);
  RegCloseKey(key);
  return value.has_value() && *value != 0
             ? BareSlashMode::default_distribution
             : BareSlashMode::distribution_list;
}

std::wstring GetBareSlashOverride() {
  HKEY key = nullptr;
  if (RegOpenKeyExW(HKEY_CURRENT_USER, FSW_SETTINGS_KEY, 0, KEY_QUERY_VALUE,
                    &key) != ERROR_SUCCESS) {
    return {};
  }
  const auto value = ReadStringValue(key, FSW_BARE_SLASH_DISTRIBUTION_VALUE);
  RegCloseKey(key);
  return value.has_value() ? *value : std::wstring{};
}

ResolveResult ResolveUserSlashPath(const std::wstring_view input) {
  const std::vector<std::wstring> registered = ListRegisteredDistributions();
  const DistributionPredicate predicate =
      [&registered](const std::wstring_view name) {
        return ContainsOrdinalIgnoreCase(registered, name);
      };
  return ResolveSlashPathWithBareSlashMode(
      input, predicate, GetBareSlashMode(), GetBareSlashOverride(),
      GetDefaultDistribution(registered).value_or(L""));
}

}  // namespace fsw
