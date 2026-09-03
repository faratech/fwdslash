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
  no_default_distribution,
};

enum class BareSlashMode {
  distribution_list,
  default_distribution,
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

// Resolves like ResolveSlashPath and then applies the user's bare-"/" mode.
// In default_distribution mode a bare "/", and any path whose leading segment
// is not a registered distribution such as "/tmp/log", resolve inside the
// preferred distribution when it is registered, otherwise inside the WSL
// default distribution when that one is registered, and otherwise fail with
// ResolveError::no_default_distribution. Explicit /Distro paths are unaffected,
// so a registered distribution always wins over a same-named directory.
[[nodiscard]] ResolveResult ResolveSlashPathWithBareSlashMode(
    std::wstring_view input,
    const DistributionPredicate& is_registered,
    BareSlashMode mode,
    std::wstring_view preferred_distribution,
    std::wstring_view wsl_default_distribution);

[[nodiscard]] std::wstring_view ResolveErrorName(ResolveError error) noexcept;

[[nodiscard]] std::wstring ResolveErrorMessage(
    ResolveError error,
    const std::vector<std::wstring>& registered_distributions = {});

[[nodiscard]] bool EqualsOrdinalIgnoreCase(std::wstring_view left,
                                           std::wstring_view right) noexcept;

}  // namespace fsw
