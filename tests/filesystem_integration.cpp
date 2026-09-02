#include <windows.h>

#include <iostream>
#include <string>

namespace {

bool SameFileIdentity(const std::wstring& left, const std::wstring& right) {
  const HANDLE left_handle = CreateFileW(
      left.c_str(), FILE_READ_ATTRIBUTES,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
      OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
  if (left_handle == INVALID_HANDLE_VALUE) {
    std::wcerr << L"CreateFileW failed for slash alias with "
               << GetLastError() << L".\n";
    return false;
  }
  const HANDLE right_handle = CreateFileW(
      right.c_str(), FILE_READ_ATTRIBUTES,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
      OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
  if (right_handle == INVALID_HANDLE_VALUE) {
    std::wcerr << L"CreateFileW failed for UNC control with "
               << GetLastError() << L".\n";
    CloseHandle(left_handle);
    return false;
  }
  FILE_STANDARD_INFO left_info{};
  FILE_STANDARD_INFO right_info{};
  const bool queried =
      GetFileInformationByHandleEx(left_handle, FileStandardInfo, &left_info,
                                   sizeof(left_info)) != FALSE &&
      GetFileInformationByHandleEx(right_handle, FileStandardInfo, &right_info,
                                   sizeof(right_info)) != FALSE;
  CloseHandle(right_handle);
  CloseHandle(left_handle);
  return queried && left_info.Directory == right_info.Directory &&
         left_info.EndOfFile.QuadPart == right_info.EndOfFile.QuadPart;
}

}  // namespace

int wmain(const int argc, wchar_t** argv) {
  if (argc != 3) {
    std::wcerr << L"Usage: fsw_filesystem_integration.exe Distro /path\n";
    return 2;
  }
  std::wstring suffix = argv[2];
  if (suffix.empty() || suffix.front() != L'/') {
    std::wcerr << L"The test path must begin with /.\n";
    return 2;
  }
  std::wstring alias = L"/" + std::wstring(argv[1]) + suffix;
  std::wstring control = L"\\\\wsl.localhost\\" + std::wstring(argv[1]);
  for (const wchar_t character : suffix) {
    control.push_back(character == L'/' ? L'\\' : character);
  }
  if (!SameFileIdentity(alias, control)) {
    return 1;
  }
  std::wcout << L"Filesystem alias and UNC control opened equivalent targets.\n";
  return 0;
}
