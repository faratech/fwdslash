cmake_minimum_required(VERSION 3.28)
project(ForwardSlashWindows VERSION 0.0.1 LANGUAGES CXX)

if(NOT WIN32)
  message(FATAL_ERROR "Forward Slash Windows only builds on Windows")
endif()

option(FSW_BUILD_TESTS "Build resolver tests" ON)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

add_library(fswcore STATIC
  src/core/path_resolver.cpp
  src/core/wsl_registry.cpp)
target_include_directories(fswcore PUBLIC src/core/include include)
target_compile_definitions(fswcore PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
target_compile_options(fswcore PRIVATE /W4 /WX /permissive- /utf-8)
target_link_libraries(fswcore PUBLIC advapi32)

add_executable(fswctl src/controller/main.cpp)
target_link_libraries(fswctl PRIVATE fswcore shell32 user32 advapi32 FltLib)
target_compile_definitions(fswctl PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
target_compile_options(fswctl PRIVATE /W4 /WX /permissive- /utf-8)

add_executable(fswbroker WIN32 src/broker/main.cpp)
target_link_libraries(fswbroker PRIVATE fswcore shell32 user32 gdi32 advapi32 ole32 oleaut32 uiautomationcore FltLib uuid)
target_compile_definitions(fswbroker PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
target_compile_options(fswbroker PRIVATE /W4 /WX /permissive- /utf-8)

if(FSW_BUILD_TESTS)
  enable_testing()
  add_executable(fsw_address_bar_integration tests/address_bar_integration.cpp)
  target_link_libraries(fsw_address_bar_integration PRIVATE shell32 ole32 oleaut32 user32)
  target_compile_definitions(fsw_address_bar_integration PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
  target_compile_options(fsw_address_bar_integration PRIVATE /W4 /WX /permissive- /utf-8)
  add_executable(fswcore_tests tests/core_tests.cpp)
  target_link_libraries(fswcore_tests PRIVATE fswcore)
  target_compile_definitions(fswcore_tests PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
  target_compile_options(fswcore_tests PRIVATE /W4 /WX /permissive- /utf-8)
  add_test(NAME fswcore_tests COMMAND fswcore_tests)
  add_executable(fsw_filesystem_integration tests/filesystem_integration.cpp)
  target_compile_definitions(fsw_filesystem_integration PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
  target_compile_options(fsw_filesystem_integration PRIVATE /W4 /WX /permissive- /utf-8)
endif()

install(TARGETS fswctl fswbroker RUNTIME DESTINATION .)
