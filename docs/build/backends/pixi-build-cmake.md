# pixi-build-cmake

The `pixi-build-cmake` backend is designed for building C and C++ projects using the [CMake](https://cmake.org/) build system. It provides seamless integration with Pixi's package management workflow while maintaining cross-platform compatibility.

!!! warning
    `pixi-build` is a preview flag, and will change until it is stabilized.
    This is why we require users to opt in to that feature by adding "pixi-build" to `workspace.preview`.

    ```toml
    [workspace]
    preview = ["pixi-build"]
    ```


## Overview

This backend automatically generates conda packages from CMake-based projects by:

- **Detecting and configuring compilers**: Automatically includes the appropriate C/C++ compilers for your target platform
- **Building with Ninja**: Uses the fast Ninja build system for optimal build performance
- **Cross-platform support**: Works consistently across Linux, macOS, and Windows
- **Standard CMake workflow**: Follows CMake best practices with sensible defaults

## Basic Usage

To use the CMake backend in your `pixi.toml`, add it to your package's build configuration:

```toml
[package]
name = "cmake_package"
version = "0.1.0"

[package.build]
backend = { name = "pixi-build-cmake", version = "*" }
channels = [
  "https://prefix.dev/conda-forge",
]
```

### Required Dependencies

The backend automatically includes the following build tools:

- `cmake` - The CMake build system
- `ninja` - Fast build system used by CMake
- Platform-specific C++ compilers (e.g., `gcc_linux-64`, `clang_osx-64`)

You can add these to your [`build-dependencies`](https://pixi.sh/latest/build/dependency_types/) if you need specific versions:

```toml
[package.build-dependencies]
ninja = "1.13"
```

## Configuration Options

You can customize the CMake backend behavior using the `[package.build.config]` section in your `pixi.toml`. The backend supports the following configuration options:

### `extra-args`

- **Type**: `Array<String>`
- **Default**: `[]`
- **Target Merge Behavior**: `Overwrite` - Platform-specific arguments completely replace base arguments

Additional command-line arguments to pass to the CMake configuration step. These arguments are inserted into the `cmake` command that configures your project.

```toml
[package.build.config]
extra-args = [
    "-DENABLE_TESTING=ON",
    "-DCMAKE_CXX_STANDARD=17"
]
```

For target-specific configuration, platform arguments completely replace the base configuration:

```toml
[package.build.config]
extra-args = ["-DCMAKE_BUILD_TYPE=Release"]

[package.build.target.linux-64.config]
extra-args = ["-DCMAKE_BUILD_TYPE=Debug", "-DLINUX_FLAG=ON"]
# Result for linux-64: ["-DCMAKE_BUILD_TYPE=Debug", "-DLINUX_FLAG=ON"]
```

### `env`

- **Type**: `Map<String, String>`
- **Default**: `{}`
- **Target Merge Behavior**: `Merge` - Platform environment variables override base variables with same name, others are merged

Environment variables to set during the build process. These variables are available to both the CMake configuration and build steps.

```toml
[package.build.config]
env = { CMAKE_VERBOSE_MAKEFILE = "ON", CXXFLAGS = "-O3 -march=native" }
```

For target-specific configuration, platform environment variables are merged with base variables:

```toml
[package.build.config]
env = { CMAKE_VERBOSE_MAKEFILE = "OFF", COMMON_VAR = "base" }

[package.build.target.linux-64.config]
env = { COMMON_VAR = "linux", LINUX_VAR = "value" }
# Result for linux-64: { CMAKE_VERBOSE_MAKEFILE = "OFF", COMMON_VAR = "linux", LINUX_VAR = "value" }
```

--8<-- "docs/partials/build-config-env-expansion.md"

### `debug-dir`

The backend always writes JSON-RPC request/response logs and the generated intermediate recipe to the `debug` subdirectory inside each work directory (for example `<work_directory>/debug`). The deprecated `debug-dir` configuration option is ignored; if it is present in a manifest a warning is emitted.

### `extra-input-globs`

- **Type**: `Array<String>`
- **Default**: `[]`
- **Target Merge Behavior**: `Overwrite` - Platform-specific globs completely replace base globs

Additional glob patterns to include as input files for the build process. These patterns are added to the input set that the backend extracts automatically from each successful build (see [Input tracking](#input-tracking) below). Use this for files that aren't visible to CMake (assets, runtime config, documentation, scripts invoked by `add_custom_command`, and so on).

```toml
[package.build.config]
extra-input-globs = [
    "assets/**/*",
    "config/*.ini",
    "*.md"
]
```

For target-specific configuration, platform-specific globs completely replace the base:

```toml
[package.build.config]
extra-input-globs = ["*.txt"]

[package.build.target.linux-64.config]
extra-input-globs = ["*.txt", "*.linux", "linux-configs/**/*"]
# Result for linux-64: ["*.txt", "*.linux", "linux-configs/**/*"]
```

### `compilers`

- **Type**: `Array<String>`
- **Default**: the languages enabled by the `project()` call in your top level `CMakeLists.txt`
- **Target Merge Behavior**: `Overwrite` - Platform-specific compilers completely replace base compilers

List of compilers to use for the build. The backend automatically generates appropriate compiler dependencies using conda-forge's compiler infrastructure.

Without this option the backend reads the languages from the `project()` call that CMake requires in the top level `CMakeLists.txt`, and requests exactly those compilers:

| `project()` declaration | Compilers |
| --- | --- |
| `project(demo)` | `["c", "cxx"]` - CMake enables both when the call names no language |
| `project(demo LANGUAGES CXX)` | `["cxx"]` |
| `project(demo LANGUAGES C CXX CUDA Fortran)` | `["c", "cxx", "cuda", "fortran"]` |
| `project(demo NONE)` | none |

`ASM` and `OBJC` map to the C compiler, `OBJCXX` maps to the C++ compiler. Languages that conda-forge has no compiler package for, such as `Swift`, are skipped. When the `CMakeLists.txt` cannot be read or has no `project()` call, the backend uses `["c", "cxx"]`, the same default CMake applies.

Set this option to request compilers the `project()` call does not name, for instance when a language is enabled later with `enable_language()`.

```toml
[package.build.config]
compilers = ["c", "cxx", "fortran"]
```

For target-specific configuration, platform compilers completely replace the base configuration:

```toml
[package.build.config]
compilers = ["cxx"]

[package.build.target.linux-64.config]
compilers = ["c", "cxx", "cuda"]
# Result for linux-64: ["c", "cxx", "cuda"]
```

!!! info "Comprehensive Compiler Documentation"
    For detailed information about available compilers, platform-specific behavior, and how conda-forge compilers work, see the [Compilers Documentation](../key_concepts/compilers.md).


## Build Process

The CMake backend follows this build process:

1. **Version Detection**: Displays CMake and Ninja versions for diagnostics
2. **Toolchain File**: Writes `conda-toolchain.cmake` into the build directory, see [Toolchain File](#toolchain-file) below
3. **Configuration**: Runs `cmake` with the following default options:
   - `-DCMAKE_TOOLCHAIN_FILE=<build directory>/conda-toolchain.cmake`: Use the compilers of the build environment
   - `-GNinja`: Use Ninja generator
   - `-DCMAKE_BUILD_TYPE=Release`: Release build by default
   - `-DCMAKE_INSTALL_PREFIX=$PREFIX`: Install to conda prefix
   - `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`: Export compile commands for tooling
   - `-DBUILD_SHARED_LIBS=ON`: Build shared libraries by default
   - `-DPython_EXECUTABLE=$PYTHON`: Added by a runtime check in the build script when a python interpreter is present in the host environment (e.g. through a `python` host dependency, conditional or not).
4. **Build**: Executes `cmake --build` to compile the project
5. **Install**: Installs the built artifacts to the conda package

## Package Metadata

The `project()` call also declares metadata about the project, which the backend uses for the fields your `pixi.toml` leaves out:

| `project()` keyword | Package field |
| --- | --- |
| `VERSION` | the package version |
| `DESCRIPTION` | `about.description` |
| `HOMEPAGE_URL` | `about.homepage` |

```cmake
project(demo VERSION 1.2.3
    DESCRIPTION "A demonstration library"
    HOMEPAGE_URL "https://example.com/demo"
    LANGUAGES CXX)
```

Anything the manifest declares wins, so these are only used where it is silent. A version in `pixi.toml` therefore takes precedence over the one in `project()`, the same way `Cargo.toml` and `pyproject.toml` are treated by the other backends.

Values that CMake only expands while configuring, such as `VERSION ${DEMO_VERSION}` or an `@DEMO_VERSION@` placeholder, cannot be resolved by the backend and are skipped. Declare those in `pixi.toml` instead. CMake has no keyword for the license, so `about.license` always comes from the manifest.

## Dependency Discovery

Before the real configure, the backend runs a throwaway one that records every package your project looks for. It installs a [dependency provider](https://cmake.org/cmake/help/latest/command/cmake_language.html#dependency-providers), a CMake callback invoked ahead of each `find_package()` search, through `CMAKE_PROJECT_TOP_LEVEL_INCLUDES`. No file of yours is touched: the provider lives in the `pixi-discovery` directory next to the build tree.

For each request the provider repeats the search CMake would have done, notes whether it succeeded, and stands in for whatever is missing so the run reaches the end. One pass therefore reports every missing package rather than stopping at the first:

```
pixi-build-cmake: SomeOptionalThing (optional) was not found
pixi-build-cmake: MustHaveThis (required) was not found
```

The real configure then runs in its own directory with no provider at all, so nothing about your build changes. A required package that is genuinely missing still fails there, with CMake's own error, exactly as before. The discovery run only happens for the first configure of a build tree.

The backend reads the result back after the build and repeats it as a warning, so it is visible in pixi's own output and not only in the build log:

```
WARN the build looked for packages that the environment does not have:
     MustHaveThis (required), SomeOptionalThing (optional). Add the ones the
     package needs to host-dependencies; the full record is in
     .../work/pixi-discovery/pixi-find-package.log
```

A missing optional package is the one worth reading twice: nothing fails, the feature it guards is simply left out of the package you ship.

The full record is that log, one line per package, kept in the work directory:

```
found required ZLIB
missing optional SomeOptionalThing
```

## Toolchain File

The conda compiler packages export the compiler they installed through environment variables such as `CC` and `CXX`. Instead of leaving it to CMake to pick those up, the backend writes a toolchain file that names them explicitly and passes it to the configure step. This keeps the compiler choice visible in the build directory and propagates it to sub-builds that inherit `CMAKE_TOOLCHAIN_FILE`, such as `ExternalProject` and `FetchContent`.

The file holds one entry per [configured compiler](#compilers):

```cmake
# Generated by pixi-build-cmake: the compilers of the build environment.
if(NOT "$ENV{CXX}" STREQUAL "")
set(CMAKE_CXX_COMPILER "$ENV{CXX}")
endif()
```

| Compiler | CMake variable | Environment variable |
| --- | --- | --- |
| `c` | `CMAKE_C_COMPILER` | `CC` |
| `cxx` | `CMAKE_CXX_COMPILER` | `CXX` |
| `fortran` | `CMAKE_Fortran_COMPILER` | `FC` |
| `cuda` | `CMAKE_CUDA_COMPILER` | `CUDACXX` |

Each entry is guarded, so a compiler that the build environment does not export keeps CMake's own detection. Compilers without a CMake equivalent get no entry, and a package that needs no compiler at all gets no toolchain file.

To use your own toolchain file instead, pass it through [`extra-args`](#extra-args); it is placed after the generated one, and CMake honors the last `-DCMAKE_TOOLCHAIN_FILE` it is given.

## Input tracking

After a successful build, the backend asks Ninja which files were actually used and stores that exact set as the build's inputs. Pixi uses those inputs to decide whether the build cache is still valid, so a tighter set means fewer false rebuilds and stale-cache misses.

Three Ninja sub-commands cover the build graph:

- **`ninja -t inputs all`**: declared translation units (the source files listed in `add_executable` / `add_library`).
- **`ninja -t deps`**: discovered headers, read from the depfile database the compiler emitted during the build.
- **`ninja -t targets all`**: `CMakeLists.txt` and any `*.cmake` file that CMake registers on its regen rule (e.g. via `include()` or `CMAKE_MODULE_PATH`).

Anything outside the project root is dropped (system headers, conda environment files, files in unrelated source trees). Anything under `<project>/.pixi/` is also dropped, since pixi's own cache and conda envs are tracked through the environment hash, not through input globs.

### The CMake file API

Ninja only knows what it built. It has no headers for a translation unit it never compiled, which covers targets excluded from `all` and any build that stopped early, and it never sees the files an `install()` copies out of the source tree, because installing runs a CMake script rather than a build edge.

The build script therefore also leaves a [CMake file API](https://cmake.org/cmake/help/latest/manual/cmake-file-api.7.html) query in the build directory. Every configure run answers it with a description of the project, and the backend unions that with what Ninja reports. The reply adds:

- the sources every target lists, including targets that never build
- the `CMakeLists.txt` files, the `.cmake` modules they include, and the inputs of `configure_file()`
- the files `install(FILES)`, `install(PROGRAMS)`, `install(DIRECTORY)` and header file sets copy out of the source tree

What the file API cannot report is a header that no target lists and that is only reached through an `#include`. That one only the depfile database knows, which is why both sources are used.

### `file(GLOB CONFIGURE_DEPENDS ...)`

If your `CMakeLists.txt` uses `file(GLOB ... CONFIGURE_DEPENDS ...)` to collect sources, the backend recovers the original glob patterns from CMake's `VerifyGlobs.cmake` and forwards them to pixi. Adding a new file matching one of those patterns invalidates the build cache and triggers a reconfigure, the same way it does for an in-tree `cmake --build` cycle.

Plain `file(GLOB ...)` without `CONFIGURE_DEPENDS` follows CMake's own semantics: only the files that matched at configure time are tracked. Adding a new file does not invalidate the cache. This mirrors what plain `file(GLOB)` does inside CMake itself: the documented footgun where you have to re-run CMake by hand. Prefer `CONFIGURE_DEPENDS` if you want auto-detection.

### Fallback

If any of the queries fail (for example, the build directory was wiped between phases, or Ninja exited non-zero), the backend logs a warning and falls back to a coarse glob set:

```text
**/*.{c,cc,cxx,cpp,h,hpp,hxx}
**/*.{cmake,cmake.in}
**/CMakeLists.txt
```

This is strictly less precise but never misses a real input. Most users will never see the fallback path.

## CMake Flag Precedence

With CMake, when duplicate flags are provided, the last flag takes precedence.
The `pixi-build-cmake` backend places `extra-args` after the default CMake flags, allowing you to override default settings.

For example, to switch from the default Release build to Debug mode:

```toml
[package.build.config]
extra-args = ["-DCMAKE_BUILD_TYPE=Debug"]
```

## Default variants

On Windows platforms, the backend automatically sets the following default variants:

- `c_compiler`: `vs2022` - Visual Studio 2022 C compiler
- `cxx_compiler`: `vs2022` - Visual Studio 2022 C++ compiler

These variants are used when you specify compilers in your [`[package.build.config.compilers]`](#compilers) configuration.
Only `cxx_compiler` will be installed by default, the `c_compiler` is set to help when you would add that compiler.

This default is set to align with conda-forge's switch to Visual Studio 2022 and because [mainstream support for Visual Studio 2019 ended in 2024](https://learn.microsoft.com/en-us/lifecycle/products/visual-studio-2019).
The `vs2022` compiler is more widely supported on modern GitHub runners and build environments.

You can override these defaults by explicitly setting variants using [`[workspace.build-variants]`](https://pixi.sh/latest/reference/pixi_manifest/#build-variants-optional) in your `pixi.toml`:

```toml
[workspace.build-variants]
c_compiler = ["vs2019"]
cxx_compiler = ["vs2019"]
```

## Limitations

- Languages enabled with `enable_language()` instead of `project()` are not detected; list them under [`compilers`](#compilers)
- A header that no target lists is only tracked once the translation unit including it has been compiled, so a header of a target that never builds is missed by both input sources

## See Also

- [Building C++ Packages](https://pixi.sh/latest/build/cpp/) - Tutorial for building C++ packages with Pixi
- [CMake Documentation](https://cmake.org/documentation/) - Official CMake documentation
