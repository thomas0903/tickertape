# Builds the C and C++ examples against the library from `cargo build --release -p tickertape-ffi`, run from the
# repository root. `writer` links the static library, `reader` and `example` the shared one. `example` uses the C++
# binding, crates/tickertape-ffi/include/tickertape.hpp (C++17).
#
#   make -C examples/c              # or: make -C examples/c PROFILE=debug
#
# With an installed library (`cargo cinstall`, see docs/c-api.md), use pkg-config instead:
#   cc writer.c $(pkg-config --cflags --libs tickertape) -o writer
REPO ?= ../..
PROFILE ?= release
INCLUDE ?= $(REPO)/crates/tickertape-ffi/include
LIBDIR ?= $(REPO)/target/$(PROFILE)

# System libraries a static link needs (from `rustc --print native-static-libs`).
ifeq ($(shell uname),Darwin)
STATIC_LIBS = -liconv -lc -lm
else
STATIC_LIBS = -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
endif

CFLAGS ?= -std=c99 -O2 -Wall -Wextra -pedantic
CXXFLAGS ?= -std=c++17 -O2 -Wall -Wextra -pedantic
RPATH = -Wl,-rpath,$(abspath $(LIBDIR))

all: writer reader example

writer: writer.c
	$(CC) $(CFLAGS) -I$(INCLUDE) $< $(LIBDIR)/libtickertape.a $(STATIC_LIBS) -o $@

reader: reader.c
	$(CC) $(CFLAGS) -I$(INCLUDE) $< -L$(LIBDIR) -ltickertape $(RPATH) -o $@

example: example.cpp $(INCLUDE)/tickertape.hpp
	$(CXX) $(CXXFLAGS) -I$(INCLUDE) $< -L$(LIBDIR) -ltickertape $(RPATH) -o $@

clean:
	rm -f writer reader example

.PHONY: all clean
