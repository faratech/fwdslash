#include "fsw/path_resolver.hpp"

#include <windows.h>

#include <algorithm>
#include <limits>

namespace fsw {
namespace {

bool ContainsNul(std::wstring_view value) noexcept {
  for (const wchar_t character : value) {
    if (character == L'\0') {
      return true;
    }
  }
  return false;
}

}  // namespace

bool EqualsOrdinalIgnoreCase(const std::wstring_view left,
                             const std::wstring_view right) noexcept {
  if (left.size() != right.size()) {
    return false;
  }
  if (left.empty()) {
    return true;
  }
  if (left.size() > static_cast<size_t>((std::numeric_limits<int>::max)())) {
    return false;
  }
  return CompareStringOrdinal(left.data(), static_cast<int>(left.size()),
                              right.data(), static_cast<int>(right.size()),
                              TRUE) == CSTR_EQUAL;
}

ResolveResult ResolveSlashPath(
    const std::wstring_view input,
    const DistributionPredicate& is_registered) {
  ResolveResult result;
  if (input.empty() || input.front() != L'/') {
    result.error = ResolveError::not_a_slash_path;
    return result;
  }
  if (input.size() > 1 && input[1] == L'/') {
    result.error = ResolveError::double_leading_slash;
    return result;
  }
  if (ContainsNul(input)) {
    result.error = ResolveError::embedded_nul;
    return result;
  }
  if (input.find(L'\\') != std::wstring_view::npos) {
    result.error = ResolveError::backslash_not_allowed;
    return result;
  }

  if (input == L"/") {
    result.target = ResolveTarget::wsl_root;
    result.linux_path = L"/";
    result.unc_path = L"\\\\wsl.localhost";
    result.had_trailing_separator = true;
    return result;
  }

  result.had_trailing_separator = input.size() > 1 && input.back() == L'/';
  const size_t first_separator = input.find(L'/', 1);
  const size_t distribution_length =
      first_separator == std::wstring_view::npos ? input.size() - 1
                                                  : first_separator - 1;
  if (distribution_length == 0) {
    result.error = ResolveError::missing_distribution;
    return result;
  }

  result.distribution.assign(input.substr(1, distribution_length));
  if (!is_registered(result.distribution)) {
    result.error = ResolveError::unregistered_distribution;
    return result;
  }

  result.target = ResolveTarget::distribution;

  std::vector<std::wstring_view> components;
  size_t cursor = first_separator == std::wstring_view::npos
                      ? input.size()
                      : first_separator + 1;
  while (cursor < input.size()) {
    const size_t separator = input.find(L'/', cursor);
    const size_t end =
        separator == std::wstring_view::npos ? input.size() : separator;
    const std::wstring_view component = input.substr(cursor, end - cursor);
    if (component.empty() || component == L".") {
      // Consecutive and current-directory separators normalize away.
    } else if (component == L"..") {
      if (components.empty()) {
        result.error = ResolveError::traversal_above_root;
        result.distribution.clear();
        return result;
      }
      components.pop_back();
    } else {
      components.push_back(component);
    }
    if (separator == std::wstring_view::npos) {
      break;
    }
    cursor = separator + 1;
  }

  result.linux_path = L"/";
  result.unc_path = L"\\\\wsl.localhost\\";
  result.unc_path.append(result.distribution);
  for (const std::wstring_view component : components) {
    if (result.linux_path.size() > 1) {
      result.linux_path.push_back(L'/');
    }
    result.linux_path.append(component);
    result.unc_path.push_back(L'\\');
    result.unc_path.append(component);
  }
  if (result.had_trailing_separator && !components.empty()) {
    result.linux_path.push_back(L'/');
    result.unc_path.push_back(L'\\');
  }
  return result;
}

std::wstring ResolveErrorMessage(
    const ResolveError error,
    const std::vector<std::wstring>& registered_distributions) {
  std::wstring message;
  switch (error) {
    case ResolveError::none:
      return L"The path is valid.";
    case ResolveError::not_a_slash_path:
      return L"This is not a forward-slash WSL path.";
    case ResolveError::double_leading_slash:
      return L"Use one leading slash. Two leading slashes are not a WSL alias.";
    case ResolveError::missing_distribution:
      return L"Enter / to list WSL distributions, or /Distro/path to open one.";
    case ResolveError::unregistered_distribution:
      message = L"That WSL distribution is not registered.";
      break;
    case ResolveError::backslash_not_allowed:
      return L"Use forward slashes in aliases, for example /Ubuntu/home.";
    case ResolveError::embedded_nul:
      return L"The path contains an invalid character.";
    case ResolveError::traversal_above_root:
      return L"The path cannot traverse above the distribution root.";
  }
  if (!registered_distributions.empty()) {
    message.append(L" Try ");
    const size_t maximum = (std::min)(registered_distributions.size(),
                                      static_cast<size_t>(3));
    for (size_t index = 0; index < maximum; ++index) {
      if (index != 0) {
        message.append(L", ");
      }
      message.push_back(L'/');
      message.append(registered_distributions[index]);
    }
    message.push_back(L'.');
  }
  return message;
}

std::wstring_view ResolveErrorName(const ResolveError error) noexcept {
  switch (error) {
    case ResolveError::none:
      return L"none";
    case ResolveError::not_a_slash_path:
      return L"not_a_slash_path";
    case ResolveError::double_leading_slash:
      return L"double_leading_slash";
    case ResolveError::missing_distribution:
      return L"missing_distribution";
    case ResolveError::unregistered_distribution:
      return L"unregistered_distribution";
    case ResolveError::backslash_not_allowed:
      return L"backslash_not_allowed";
    case ResolveError::embedded_nul:
      return L"embedded_nul";
    case ResolveError::traversal_above_root:
      return L"traversal_above_root";
  }
  return L"unknown";
}

}  // namespace fsw
