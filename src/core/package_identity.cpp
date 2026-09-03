#include "fsw/package_identity.hpp"

#include <windows.h>

#include <appmodel.h>

namespace fsw {

bool HasPackageIdentity() noexcept {
  // Queried once: identity cannot change over a process lifetime.
  static const bool packaged = [] {
    UINT32 length = 0;
    // Asking for the length always reports APPMODEL_ERROR_NO_PACKAGE when the
    // process is unpackaged, and ERROR_INSUFFICIENT_BUFFER when it is not.
    return ::GetCurrentPackageFullName(&length, nullptr) !=
           APPMODEL_ERROR_NO_PACKAGE;
  }();
  return packaged;
}

}  // namespace fsw
