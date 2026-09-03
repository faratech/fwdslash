#pragma once

namespace fsw {

// True when the running process has MSIX package identity.
//
// The packaged build declares the startup task and the fwdslash:// protocol in
// its manifest, so the controller must not also write the HKCU Run value and
// Software\Classes registration it uses when unpackaged. Those writes are not
// virtualized away by this package, so they would persist as orphaned entries
// after the package is removed.
[[nodiscard]] bool HasPackageIdentity() noexcept;

}  // namespace fsw
