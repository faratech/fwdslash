#pragma once

#include <functional>
#include <string>
#include <string_view>
#include <vector>

namespace fsw {

enum class ResolveTarget {
  none,
  wsl_root,
  distribution,
};

enum class ResolveError {
  none,
  not_a_slash_path,
  double_leading_slash,
  missing_distribution,
  unregistered_distribution,
  backslash_not_allowed,
  embedded_nul,
  traversal_above_root,
};

struct ResolveResult {
  ResolveError error{ResolveError::none};
  ResolveTarget target{ResolveTarget::none};
  std::wstring distribution;
  std::wstring linux_path;
  std::wstring unc_path;
  bool had_trailing_separator{false};

  [[nodiscard]] bool matched() const noexcept {
    return error == ResolveError::none && target != ResolveTarget::none;
  }

  [[nodiscard]] bool is_wsl_root() const noexcept {
    return matched() && target == ResolveTarget::wsl_root;
  }
};

using DistributionPredicate =
    std::function<bool(std::wstring_view distribution)>;

[[nodiscard]] ResolveResult ResolveSlashPath(
    std::wstring_view input,
    const DistributionPredicate& is_registered);

[[nodiscard]] std::wstring_view ResolveErrorName(ResolveError error) noexcept;

[[nodiscard]] std::wstring ResolveErrorMessage(
    ResolveError error,
    const std::vector<std::wstring>& registered_distributions = {});

[[nodiscard]] bool EqualsOrdinalIgnoreCase(std::wstring_view left,
                                           std::wstring_view right) noexcept;

}  // namespace fsw
