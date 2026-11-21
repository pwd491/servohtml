Wrapper around [Servo](https://github.com/servo/servo) for HTML rendering in [CS16Client](https://github.com/FWGS/CS16Client). The C++ client loads a compiled library built from the Rust crate in `rust/`.

## Build

```bash
cmake -S . -B build -DBUILD_CSWEB=ON
cmake --build build --config Release
```

The resulting `libcsweb.so` is copied into `build/` (next to the client).

## Debug build (Rust debuginfo)

```bash
cmake -S . -B build -DBUILD_CSWEB=ON -DBUILD_CSWEB_DEBUG=ON
cmake --build build --config Debug --target servo
```

> After toggling the flag you **must reconfigure** (`cmake -S . -B build -D...`). A plain `cmake --build` is not enough — Corrosion caches the profile.

## Requirements

- CMake ≥ 3.10
- Rust toolchain (`rustup`, stable)
- Corrosion is fetched automatically via `FetchContent`

## Layout

```
include/        C headers for the client
rust/           Rust crate csweb (cdylib)
patches/        Auto-patching for specific-platform
CMakeLists.txt
```