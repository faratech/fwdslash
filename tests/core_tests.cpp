#include "fsw/path_resolver.hpp"

#include <cstdlib>
#include <iostream>
#include <string_view>

namespace {

bool Registered(const std::wstring_view name) {
  return fsw::EqualsOrdinalIgnoreCase(name, L"Ubuntu") ||
         fsw::EqualsOrdinalIgnoreCase(name, L"Dev Distro") ||
         fsw::EqualsOrdinalIgnoreCase(name, L"\u65e5\u672c\u8a9e");
}

void Require(const bool condition, const char* message) {
  if (!condition) {
    std::cerr << "FAILED: " << message << '\n';
    std::exit(EXIT_FAILURE);
  }
}

void RequireError(const std::wstring_view input,
                  const fsw::ResolveError expected) {
  const fsw::ResolveResult result = fsw::ResolveSlashPath(input, Registered);
  Require(result.error == expected, "unexpected resolver error");
}

fsw::ResolveResult ResolveWithMode(
    const std::wstring_view input, const fsw::BareSlashMode mode,
    const std::wstring_view preferred, const std::wstring_view wsl_default) {
  return fsw::ResolveSlashPathWithBareSlashMode(input, Registered, mode,
                                                preferred, wsl_default);
}

}  // namespace

int wmain() {
  {
    const auto result = fsw::ResolveSlashPath(L"/", Registered);
    Require(result.matched(), "bare slash should resolve");
    Require(result.is_wsl_root(), "bare slash should target the WSL root");
    Require(result.distribution.empty(), "bare slash has no distribution");
    Require(result.unc_path == L"\\\\wsl.localhost",
            "bare slash should list distributions");
  }
  {
    const auto result = fsw::ResolveSlashPath(L"/Ubuntu/", Registered);
    Require(result.matched(), "root should resolve");
    Require(result.distribution == L"Ubuntu", "distribution should be kept");
    Require(result.linux_path == L"/", "root Linux path should be slash");
    Require(result.unc_path == L"\\\\wsl.localhost\\Ubuntu",
            "root UNC should not gain an empty component");
  }
  {
    const auto result =
        fsw::ResolveSlashPath(L"/ubuntu/home/me/../project/", Registered);
    Require(result.matched(), "case-insensitive distro should resolve");
    Require(result.linux_path == L"/home/project/", "dot segments normalize");
    Require(result.unc_path ==
                L"\\\\wsl.localhost\\ubuntu\\home\\project\\",
            "UNC translation should preserve trailing slash");
  }
  {
    const auto result = fsw::ResolveSlashPath(
        L"/Dev Distro/home/user/My Project", Registered);
    Require(result.matched(), "spaces should resolve");
  }
  {
    const auto result =
        fsw::ResolveSlashPath(L"/\u65e5\u672c\u8a9e/home/\u30c6\u30b9\u30c8", Registered);
    Require(result.matched(), "Unicode should resolve");
  }

  RequireError(L"C:/Ubuntu", fsw::ResolveError::not_a_slash_path);
  RequireError(L"//Ubuntu", fsw::ResolveError::double_leading_slash);
  RequireError(L"/Debian/home", fsw::ResolveError::unregistered_distribution);
  RequireError(L"/Ubuntu\\home", fsw::ResolveError::backslash_not_allowed);
  RequireError(L"/Ubuntu/../Debian", fsw::ResolveError::traversal_above_root);
  Require(!fsw::ResolveErrorMessage(
               fsw::ResolveError::unregistered_distribution,
               {L"Ubuntu", L"Dev Distro"})
               .empty(),
          "invalid paths should have a user-facing explanation");

  {
    const auto result = ResolveWithMode(
        L"/", fsw::BareSlashMode::distribution_list, L"", L"Ubuntu");
    Require(result.is_wsl_root(), "list mode keeps the bare slash root");
    Require(result.unc_path == L"\\\\wsl.localhost",
            "list mode should target the distribution list");
  }
  {
    const auto result =
        ResolveWithMode(L"/", fsw::BareSlashMode::default_distribution, L"",
                        L"Ubuntu");
    Require(result.matched(), "default mode should resolve the bare slash");
    Require(result.distribution == L"Ubuntu",
            "default mode should follow the WSL default distribution");
    Require(result.linux_path == L"/",
            "default mode should resolve to the distribution root");
    Require(result.unc_path == L"\\\\wsl.localhost\\Ubuntu",
            "default mode should rewrite the bare slash UNC");
  }
  {
    const auto result = ResolveWithMode(
        L"/", fsw::BareSlashMode::default_distribution, L"Dev Distro",
        L"Ubuntu");
    Require(result.distribution == L"Dev Distro",
            "a registered preference should win over the WSL default");
    Require(result.unc_path == L"\\\\wsl.localhost\\Dev Distro",
            "preference UNC should preserve the distribution name");
  }
  {
    const auto result = ResolveWithMode(
        L"/", fsw::BareSlashMode::default_distribution, L"dev distro",
        L"Ubuntu");
    Require(result.distribution == L"dev distro",
            "preference matching should be case-insensitive");
  }
  {
    const auto result = ResolveWithMode(
        L"/", fsw::BareSlashMode::default_distribution, L"Debian", L"Ubuntu");
    Require(result.distribution == L"Ubuntu",
            "an unregistered preference should fall back to the WSL default");
  }
  {
    const auto result = ResolveWithMode(
        L"/", fsw::BareSlashMode::default_distribution, L"Debian", L"");
    Require(result.error == fsw::ResolveError::no_default_distribution,
            "no usable default should block the bare slash");
    Require(!result.matched(), "a blocked bare slash should not match");
  }
  {
    const auto result = ResolveWithMode(
        L"/", fsw::BareSlashMode::default_distribution, L"", L"");
    Require(result.error == fsw::ResolveError::no_default_distribution,
            "an unknown WSL default should block the bare slash");
  }
  {
    const auto result = ResolveWithMode(
        L"/Ubuntu/home", fsw::BareSlashMode::default_distribution, L"Ubuntu",
        L"");
    Require(result.unc_path == L"\\\\wsl.localhost\\Ubuntu\\home",
            "default mode should not change explicit distribution paths");
  }

  std::wcout << L"All resolver tests passed.\n";
  return EXIT_SUCCESS;
}
