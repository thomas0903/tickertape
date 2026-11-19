# CMake package for the tickertape C ABI and its C++ binding (ADR 0036). It defines two imported targets:
#
#   tickertape::tickertape   libtickertape (shared) and the include directory with tickertape.h
#   tickertape::cpp          the above plus the C++ binding tickertape.hpp, and the C++17 requirement
#
# Use it from a checkout of the repository, after `cargo build --release -p tickertape-ffi`, in either way:
#
#   find_package(tickertape CONFIG REQUIRED PATHS /path/to/tickertape/crates/tickertape-ffi/cmake)
#   add_subdirectory(/path/to/tickertape/crates/tickertape-ffi tickertape)   # the targets are then global
#
#   target_link_libraries(app PRIVATE tickertape::cpp)
#
# It looks for the library in the repository's target/release (TICKERTAPE_PROFILE picks another profile directory),
# then in the usual system places (an installed library, for example from `cargo cinstall`). Set
# TICKERTAPE_LIBRARY and TICKERTAPE_INCLUDE_DIR to choose the files yourself.
if(TARGET tickertape::tickertape)
  return()
endif()

get_filename_component(_tickertape_crate "${CMAKE_CURRENT_LIST_DIR}/.." ABSOLUTE)
get_filename_component(_tickertape_repo "${_tickertape_crate}/../.." ABSOLUTE)
if(NOT DEFINED TICKERTAPE_PROFILE)
  set(TICKERTAPE_PROFILE release)
endif()

find_path(TICKERTAPE_INCLUDE_DIR NAMES tickertape.hpp tickertape.h
  HINTS "${_tickertape_crate}/include"
  DOC "Directory with tickertape.h and tickertape.hpp")
find_library(TICKERTAPE_LIBRARY NAMES tickertape
  HINTS "${_tickertape_repo}/target/${TICKERTAPE_PROFILE}"
  DOC "libtickertape (shared)")

include(FindPackageHandleStandardArgs)
find_package_handle_standard_args(tickertape
  REQUIRED_VARS TICKERTAPE_LIBRARY TICKERTAPE_INCLUDE_DIR
  REASON_FAILURE_MESSAGE "build the library first: cargo build --release -p tickertape-ffi")
if(NOT tickertape_FOUND)
  return()
endif()

if(TICKERTAPE_IMPORTED_GLOBAL)
  set(_tickertape_scope GLOBAL)
else()
  set(_tickertape_scope "")
endif()

add_library(tickertape::tickertape SHARED IMPORTED ${_tickertape_scope})
set_target_properties(tickertape::tickertape PROPERTIES
  IMPORTED_LOCATION "${TICKERTAPE_LIBRARY}"
  INTERFACE_INCLUDE_DIRECTORIES "${TICKERTAPE_INCLUDE_DIR}")
# A plain `cargo build` gives the library no soname (Linux): say so, so that CMake links it with -ltickertape and an
# rpath instead of recording its path in the build tree.
string(FIND "${TICKERTAPE_LIBRARY}" "${_tickertape_repo}/target/" _tickertape_in_repo)
if(_tickertape_in_repo EQUAL 0)
  set_target_properties(tickertape::tickertape PROPERTIES IMPORTED_NO_SONAME TRUE)
endif()

add_library(tickertape::cpp INTERFACE IMPORTED ${_tickertape_scope})
set_target_properties(tickertape::cpp PROPERTIES
  INTERFACE_LINK_LIBRARIES tickertape::tickertape
  INTERFACE_COMPILE_FEATURES cxx_std_17)

unset(_tickertape_scope)
unset(_tickertape_in_repo)
unset(_tickertape_crate)
unset(_tickertape_repo)
