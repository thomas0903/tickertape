# Checks that `add_subdirectory` users of the tickertape CMake package get the targets (CI builds this; see the
# `c abi` job in .github/workflows/ci.yml). It builds the C++ example with tickertape::cpp.
#
#   cmake -S crates/tickertape-ffi/tests/cmake -B build/cmake-subdir && cmake --build build/cmake-subdir
cmake_minimum_required(VERSION 3.16)
project(tickertape_add_subdirectory CXX)

add_subdirectory(../.. tickertape)
add_executable(example ../../../../examples/c/example.cpp)
target_link_libraries(example PRIVATE tickertape::cpp)
