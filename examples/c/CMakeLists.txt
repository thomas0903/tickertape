# Minimal CMake build of the examples, against the library built in this repository by
# `cargo build --release -p tickertape-ffi` (or an installed one), through the tickertape CMake package.
#
#   cmake -S examples/c -B build/examples && cmake --build build/examples
cmake_minimum_required(VERSION 3.16)
project(tickertape_examples C CXX)

find_package(tickertape CONFIG REQUIRED PATHS "${CMAKE_CURRENT_SOURCE_DIR}/../../crates/tickertape-ffi/cmake"
  NO_DEFAULT_PATH)

add_executable(writer writer.c)
add_executable(reader reader.c)
add_executable(example example.cpp)
set_target_properties(writer reader PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON)
target_link_libraries(writer PRIVATE tickertape::tickertape)
target_link_libraries(reader PRIVATE tickertape::tickertape)
target_link_libraries(example PRIVATE tickertape::cpp)  # C++17 and tickertape.hpp
