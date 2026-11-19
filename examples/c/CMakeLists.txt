# Minimal CMake build of the examples. Uses the installed library through pkg-config if there is one
# (`cargo cinstall`), otherwise the library built in this repository by `cargo build --release -p tickertape-ffi`.
#
#   cmake -S examples/c -B build/examples && cmake --build build/examples
cmake_minimum_required(VERSION 3.16)
project(tickertape_examples C CXX)

find_package(PkgConfig QUIET)
if(PKG_CONFIG_FOUND)
  pkg_check_modules(TICKERTAPE IMPORTED_TARGET tickertape)
endif()

if(TARGET PkgConfig::TICKERTAPE)
  set(TICKERTAPE_LIB PkgConfig::TICKERTAPE)
else()
  set(TICKERTAPE_REPO "${CMAKE_CURRENT_SOURCE_DIR}/../..")
  add_library(tickertape SHARED IMPORTED)
  set_target_properties(tickertape PROPERTIES
    IMPORTED_LOCATION "${TICKERTAPE_REPO}/target/release/libtickertape${CMAKE_SHARED_LIBRARY_SUFFIX}"
    IMPORTED_NO_SONAME TRUE
    INTERFACE_INCLUDE_DIRECTORIES "${TICKERTAPE_REPO}/crates/tickertape-ffi/include")
  set(TICKERTAPE_LIB tickertape)
endif()

add_executable(writer writer.c)
add_executable(reader reader.c)
add_executable(example example.cpp)
set_target_properties(writer reader PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON)
set_target_properties(example PROPERTIES CXX_STANDARD 11 CXX_STANDARD_REQUIRED ON)
foreach(target writer reader example)
  target_link_libraries(${target} PRIVATE ${TICKERTAPE_LIB})
endforeach()
