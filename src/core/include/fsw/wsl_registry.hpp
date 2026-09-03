#pragma once

#include "fsw/path_resolver.hpp"

#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace fsw {

[[nodiscard]] std::vector<std::wstring> ListRegisteredDistributions();
[[nodiscard]] bool IsRegisteredDistribution(std::wstring_view distribution);

// Resolves the distribution `wsl --set-default` points at. The caller passes
// its already enumerated registered distributions so hot paths enumerate the
// registry only once. Falls back to the single registered distribution when
// WSL has no recorded default.
[[nodiscard]] std::optional<std::wstring> GetDefaultDistribution(
    const std::vector<std::wstring>& registered_distributions);

[[nodiscard]] BareSlashMode GetBareSlashMode();

// The user's pinned bare-"/" distribution, or empty to follow the WSL default.
[[nodiscard]] std::wstring GetBareSlashOverride();

// Full registry-backed resolution used by the broker and controller: one
// enumeration, the per-user bare-"/" mode, and the effective default.
[[nodiscard]] ResolveResult ResolveUserSlashPath(std::wstring_view input);

}  // namespace fsw
