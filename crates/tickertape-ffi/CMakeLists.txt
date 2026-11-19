# For `add_subdirectory(path/to/tickertape/crates/tickertape-ffi tickertape)`: defines the global imported targets
# tickertape::tickertape (the C library and tickertape.h) and tickertape::cpp (plus the C++ binding, C++17). Build
# the library first with `cargo build --release -p tickertape-ffi`. See cmake/tickertapeConfig.cmake and
# docs/cpp-api.md.
cmake_minimum_required(VERSION 3.16)
set(TICKERTAPE_IMPORTED_GLOBAL TRUE)
include("${CMAKE_CURRENT_LIST_DIR}/cmake/tickertapeConfig.cmake")
if(NOT TARGET tickertape::tickertape)
  message(FATAL_ERROR "libtickertape not found: run `cargo build --release -p tickertape-ffi` first")
endif()
