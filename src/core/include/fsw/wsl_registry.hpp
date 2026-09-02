#pragma once

#include "fsw/path_resolver.hpp"

#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace fsw {

[[nodiscard]] std::vector<std::wstring> ListRegisteredDistributions();
[[nodiscard]] bool IsRegisteredDistribution(std::wstring_view distribution);
[[nodiscard]] std::optional<std::wstring> GetDefaultDistribution();
[[nodiscard]] ResolveResult ResolveRegisteredSlashPath(std::wstring_view input);

}  // namespace fsw
