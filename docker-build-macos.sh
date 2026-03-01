#!/bin/bash
# Script to set up macOS cross-compilation with ONNX Runtime + CoreML

set -euo pipefail  # Exit on any error, undefined var, or failed pipe
set -x             # Trace commands for easier debugging

# Remember repo root to restore later
SCRIPT_ROOT=$(pwd)

# Install build dependencies in the container
echo "Installing build dependencies..."
apt update -y
apt install -y wget cmake protobuf-compiler git git-lfs build-essential python3 python3-pip python3-dev ninja-build libeigen3-dev

# Install iconv library for cross-compilation
apt install -y libiconv-hook-dev || apt install -y libc6-dev

# Clone ONNX Runtime if not present
if [ ! -d "onnxruntime" ]; then
    echo "Cloning ONNX Runtime source..."
    git clone --recursive https://github.com/Microsoft/onnxruntime.git
    cd onnxruntime
    # Use a stable release tag
    git checkout v1.18.1
    git submodule update --init --recursive
    cd ..
fi

# Always patch google_nsync external project to pass Darwin/NSYNC flags into its CMake configure step
if [ -f "onnxruntime/cmake/external/google_nsync.cmake" ]; then
    echo "Ensuring cmake/external/google_nsync.cmake disables futex and forces Darwin for google_nsync..."
    # If our CMAKE_ARGS line is not already present, inject it after the ExternalProject_Add(google_nsync ...)
    if ! grep -q "NSYNC_ENABLE_FUTEX=OFF" onnxruntime/cmake/external/google_nsync.cmake; then
        awk '
            BEGIN{patched=0}
            /ExternalProject_Add\(google_nsync/{print; print "  CMAKE_ARGS -DNSYNC_ENABLE_FUTEX=OFF -DNSYNC_USE_FUTEX=OFF -DNSYNC_PLATFORM=c++11 -DNSYNC_TARGET_PLATFORM=posix -DNSYNC_TARGET_OS=Darwin -DNSYNC_ENABLE_LINUX_FUTEX=OFF -DCMAKE_SYSTEM_NAME=Darwin -DAPPLE=1"; patched=1; next}
            {print}
            END{ if(patched==0) exit 1 }
        ' onnxruntime/cmake/external/google_nsync.cmake > onnxruntime/cmake/external/google_nsync.cmake.patched 2>/dev/null && mv onnxruntime/cmake/external/google_nsync.cmake.patched onnxruntime/cmake/external/google_nsync.cmake || echo "Warning: Failed to patch google_nsync.cmake; will rely on top-level flags and subbuild cleanup."
    fi
fi

# Build ONNX Runtime with CoreML for macOS
# We don't rely on the platform-named folder (ONNX Runtime names it by host, e.g., Linux/Release
# even when cross-compiling). We'll always invoke the build; CMake will be incremental.
echo "Building ONNX Runtime with CoreML for macOS..."
cd onnxruntime
    
    # Set up cross-compilation environment for the build
        # Set real compilers
        REAL_CC=/osxcross/bin/x86_64-apple-darwin24-clang
        REAL_CXX=/osxcross/bin/x86_64-apple-darwin24-clang++
        # Create wrapper compilers that strip GNU-only linker flags
        WRAP_DIR="$SCRIPT_ROOT/.ort-wrap"
        mkdir -p "$WRAP_DIR"
        cat >"$WRAP_DIR/x86_64-apple-darwin24-clang" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
real="/osxcross/bin/x86_64-apple-darwin24-clang"
args=()
for a in "$@"; do
    # Drop GNU ld flags and obsolete -s
    if [ "$a" = "-Wl,--gc-sections" ] || [ "$a" = "--gc-sections" ] || [ "$a" = "-s" ]; then
        continue
    fi
    if [[ "$a" == *"--gc-sections"* ]]; then
        a="${a/--gc-sections/}"
        [ -z "$a" ] && continue
    fi
    args+=("$a")
done
exec "$real" "${args[@]}"
EOF
        cat >"$WRAP_DIR/x86_64-apple-darwin24-clang++" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
real="/osxcross/bin/x86_64-apple-darwin24-clang++"
args=()
for a in "$@"; do
    # Drop GNU ld flags and obsolete -s
    if [ "$a" = "-Wl,--gc-sections" ] || [ "$a" = "--gc-sections" ] || [ "$a" = "-s" ]; then
        continue
    fi
    if [[ "$a" == *"--gc-sections"* ]]; then
        a="${a/--gc-sections/}"
        [ -z "$a" ] && continue
    fi
    args+=("$a")
done
exec "$real" "${args[@]}"
EOF
        chmod +x "$WRAP_DIR/x86_64-apple-darwin24-clang" "$WRAP_DIR/x86_64-apple-darwin24-clang++"

        # Export wrappers as compilers
        export CC="$WRAP_DIR/x86_64-apple-darwin24-clang"
        export CXX="$WRAP_DIR/x86_64-apple-darwin24-clang++"
    export AR=x86_64-apple-darwin24-ar
    export STRIP=x86_64-apple-darwin24-strip

    # Detect macOS SDK sysroot from osxcross
    SYSROOT=""
    for base in /osxcross /opt/osxcross; do
        if [ -d "$base/SDK" ]; then
            candidate=$(ls -d "$base"/SDK/MacOSX*.sdk 2>/dev/null | sort -V | tail -n1)
            if [ -n "$candidate" ]; then
                SYSROOT="$candidate"
                break
            fi
        fi
    done
    if [ -n "$SYSROOT" ]; then
        echo "Using macOS SDK sysroot: $SYSROOT"
        export SDKROOT="$SYSROOT"
        EXTRA_SYSROOT_ARGS=( --cmake_extra_defines CMAKE_OSX_SYSROOT="$SYSROOT" )
    else
        echo "Warning: macOS SDK sysroot not found under /osxcross/SDK or /opt/osxcross/SDK. Continuing without CMAKE_OSX_SYSROOT."
        EXTRA_SYSROOT_ARGS=()
    fi
    
    # Set up Iconv paths for macOS SDK (robust detection)
    ICONV_INCLUDE=""
    ICONV_LIBRARY=""
    ICONV_BUILTIN=0
    # Zlib (many deps expect zlib; point to SDK .tbd/.dylib explicitly for Darwin)
    ZLIB_INCLUDE=""
    ZLIB_LIBRARY=""
    if [ -n "$SYSROOT" ]; then
        ICONV_INCLUDE="$SYSROOT/usr/include"
        for cand in \
            "$SYSROOT/usr/lib/libiconv.tbd" \
            "$SYSROOT/usr/lib/libiconv.dylib" \
            "$SYSROOT/usr/lib/libiconv.a"; do
            if [ -f "$cand" ]; then
                ICONV_LIBRARY="$cand"
                break
            fi
        done
        if [ -n "$ICONV_LIBRARY" ]; then
            echo "Using macOS SDK Iconv: include=$ICONV_INCLUDE, lib=$ICONV_LIBRARY"
        else
            echo "Iconv library not found in SDK. Will treat iconv as built-in."
            ICONV_BUILTIN=1
        fi

        # Find zlib in SDK
        for zlib_cand in \
            "$SYSROOT/usr/lib/libz.tbd" \
            "$SYSROOT/usr/lib/libz.dylib" \
            "$SYSROOT/usr/lib/libz.a"; do
            if [ -f "$zlib_cand" ]; then
                ZLIB_LIBRARY="$zlib_cand"
                break
            fi
        done
        if [ -f "$SYSROOT/usr/include/zlib.h" ]; then
            ZLIB_INCLUDE="$SYSROOT/usr/include"
        fi
        if [ -n "$ZLIB_LIBRARY" ] && [ -n "$ZLIB_INCLUDE" ]; then
            echo "Using macOS SDK Zlib: include=$ZLIB_INCLUDE, lib=$ZLIB_LIBRARY"
        else
            echo "Warning: Could not fully locate Zlib in SDK. Continuing; CMake may warn."
        fi

        # Set environment variables to ensure sysroot is used consistently
    export CMAKE_OSX_SYSROOT="$SYSROOT"
    export MACOSX_DEPLOYMENT_TARGET=13.3
    export CFLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -D_LIBCPP_DISABLE_AVAILABILITY -Wno-error -Wno-error=deprecated-pragma -Wno-error=deprecated-declarations"
    export CXXFLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -D_LIBCPP_DISABLE_AVAILABILITY -Wno-deprecated-this-capture -Wno-error=deprecated-this-capture -Wno-error -Wno-error=deprecated-pragma -Wno-error=deprecated-declarations"
    export LDFLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -Wl,-dead_strip"
        export OSXCROSS_NO_INCLUDE_PATH_WARNINGS=1
    fi

    # Parallelism
    JOBS=${JOBS:-$(command -v nproc >/dev/null 2>&1 && nproc || echo 4)}

    # If a previous build directory exists, proactively remove any GNU ld flags from Ninja/CMake artifacts
    PRE_BUILD_DIR="$(pwd)/build/Linux/Release"
    if [ -d "$PRE_BUILD_DIR" ]; then
        echo "Pre-patching existing Ninja/CMake artifacts in $PRE_BUILD_DIR to remove -Wl,--gc-sections"
        if [ -f "$PRE_BUILD_DIR/build.ninja" ]; then
            sed -i.bak 's/-Wl,--gc-sections//g' "$PRE_BUILD_DIR/build.ninja" || true
        fi
        if [ -f "$PRE_BUILD_DIR/CMakeCache.txt" ]; then
            sed -i.bak 's/-Wl,--gc-sections//g' "$PRE_BUILD_DIR/CMakeCache.txt" || true
        fi
        grep -RIl -- '--gc-sections' "$PRE_BUILD_DIR"/CMakeFiles 2>/dev/null | xargs -r sed -i.bak 's/-Wl,--gc-sections//g' || true
    fi

    # Clean nsync sub-builds to force reconfigure with Darwin flags
    if [ -d build/Linux/Release/_deps ]; then
        rm -rf build/Linux/Release/_deps/google_nsync-build build/Linux/Release/_deps/google_nsync-subbuild 2>/dev/null || true
    fi

    # Workaround: google_nsync subproject sometimes picks linux futex headers via platform/c++11.futex.
    # Replace that directory with a symlink to platform/c++11 so includes resolve to the non-futex implementation.
    if [ -d build/Linux/Release/_deps/google_nsync-src/platform ]; then
        pushd build/Linux/Release/_deps/google_nsync-src/platform >/dev/null || true
        if [ -d "c++11.futex" ] && [ -d "c++11" ]; then
            echo "Redirecting google_nsync platform/c++11.futex -> platform/c++11 (symlink)"
            rm -rf "c++11.futex.bak" 2>/dev/null || true
            mv "c++11.futex" "c++11.futex.bak" 2>/dev/null || true
            ln -s "c++11" "c++11.futex" 2>/dev/null || true
        fi
        if [ -d "linux" ]; then
            echo "Disabling google_nsync platform/linux (rename to linux.disabled)"
            rm -rf linux.disabled 2>/dev/null || true
            mv linux linux.disabled 2>/dev/null || true
        fi
        popd >/dev/null || true
        # Also patch top-level CMakeLists.txt to drop linux sources on Apple
        if [ -f build/Linux/Release/_deps/google_nsync-src/CMakeLists.txt ]; then
            echo "Patching google_nsync CMakeLists.txt to exclude platform/linux sources on Apple"
            sed -i.bak '/platform\/linux\/src\//d' build/Linux/Release/_deps/google_nsync-src/CMakeLists.txt || true
        fi
    fi

    # Build with CoreML support and extended minimal build (required for CoreML)
    # Run build allowing failure so we can patch and retry if needed
    set +e
    ./build.sh --config Release \
        --build_shared_lib \
        --use_coreml \
        --allow_running_as_root \
        --use_preinstalled_eigen \
        --eigen_path /usr/include/eigen3 \
        --minimal_build extended \
    --disable_contrib_ops \
    --cmake_generator Ninja \
        --cmake_extra_defines CMAKE_OSX_ARCHITECTURES=x86_64 \
    --cmake_extra_defines CMAKE_OSX_DEPLOYMENT_TARGET=13.3 \
    --cmake_extra_defines CMAKE_SYSTEM_NAME=Darwin \
    --cmake_extra_defines CMAKE_CXX_STANDARD=17 \
    --cmake_extra_defines CMAKE_CXX_STANDARD_REQUIRED=ON \
        --cmake_extra_defines CMAKE_BUILD_WITH_INSTALL_RPATH=ON \
        --cmake_extra_defines CMAKE_INSTALL_RPATH=@rpath \
        --cmake_extra_defines CMAKE_MACOSX_RPATH=ON \
    --cmake_extra_defines NSYNC_ENABLE_FUTEX=OFF \
    --cmake_extra_defines NSYNC_USE_FUTEX=OFF \
    --cmake_extra_defines NSYNC_PLATFORM=c++11 \
    --cmake_extra_defines NSYNC_TARGET_PLATFORM=posix \
    --cmake_extra_defines NSYNC_TARGET_OS=Darwin \
    --cmake_extra_defines CMAKE_C_COMPILER="$WRAP_DIR/x86_64-apple-darwin24-clang" \
    --cmake_extra_defines CMAKE_CXX_COMPILER="$WRAP_DIR/x86_64-apple-darwin24-clang++" \
        --cmake_extra_defines CMAKE_CROSSCOMPILING=ON \
        --cmake_extra_defines CMAKE_SYSTEM_PROCESSOR=x86_64 \
        --cmake_extra_defines CMAKE_VERBOSE_MAKEFILE=ON \
        --cmake_extra_defines onnxruntime_BUILD_UNIT_TESTS=OFF \
        --cmake_extra_defines onnxruntime_ENABLE_TRAINING=OFF \
        $( [ -n "$ICONV_INCLUDE" ] && printf -- "--cmake_extra_defines Iconv_INCLUDE_DIR=%s " "$ICONV_INCLUDE" ) \
        $( [ -n "$ICONV_LIBRARY" ] && printf -- "--cmake_extra_defines Iconv_LIBRARY=%s " "$ICONV_LIBRARY" ) \
        $( [ "$ICONV_BUILTIN" = 1 ] && printf -- "--cmake_extra_defines Iconv_IS_BUILT_IN=ON " ) \
    $( [ -n "$ZLIB_INCLUDE" ] && printf -- "--cmake_extra_defines ZLIB_INCLUDE_DIR=%s " "$ZLIB_INCLUDE" ) \
    $( [ -n "$ZLIB_LIBRARY" ] && printf -- "--cmake_extra_defines ZLIB_LIBRARY=%s " "$ZLIB_LIBRARY" ) \
    --cmake_extra_defines CMAKE_C_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -D_LIBCPP_DISABLE_AVAILABILITY -Wno-error -Wno-error=deprecated-pragma -Wno-error=deprecated-declarations" \
    --cmake_extra_defines CMAKE_CXX_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -D_LIBCPP_DISABLE_AVAILABILITY -Wno-deprecated-this-capture -Wno-error=deprecated-this-capture -Wno-error -Wno-error=deprecated-pragma -Wno-error=deprecated-declarations" \
    --cmake_extra_defines CMAKE_EXE_LINKER_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -Wl,-dead_strip" \
    --cmake_extra_defines CMAKE_SHARED_LINKER_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -Wl,-dead_strip" \
        --cmake_extra_defines FLATBUFFERS_BUILD_FLATC=OFF \
    --cmake_extra_defines FLATBUFFERS_BUILD_TESTS=OFF \
    --cmake_extra_defines onnxruntime_BUILD_CUSTOM_OP_LIBRARY=OFF \
    --target onnxruntime \
    "${EXTRA_SYSROOT_ARGS[@]}" \
    --parallel ${JOBS}

    BUILD_STATUS=$?
    set -e
cd ..

if [ $BUILD_STATUS -ne 0 ]; then
    echo "Initial ONNX Runtime build failed. Attempting to patch unsupported linker flags and retry..."
    # Try to remove GNU ld-only flag --gc-sections from Ninja/link commands and rebuild.
    set +e
    ORT_DIR="$SCRIPT_ROOT/onnxruntime"
    BUILD_DIR="$ORT_DIR/build"
    REL_DIR="$BUILD_DIR/Linux/Release"
    if [ ! -d "$REL_DIR" ]; then
        # Fallback: search for any Release folder
        REL_DIR=$(find "$BUILD_DIR" -type d -name Release | head -n1)
    fi
        if [ -n "$REL_DIR" ] && [ -d "$REL_DIR" ]; then
        if [ -f "$REL_DIR/build.ninja" ]; then
            echo "Patching $REL_DIR/build.ninja to remove -Wl,--gc-sections"
            sed -i.bak 's/-Wl,--gc-sections//g' "$REL_DIR/build.ninja"
        fi
        if [ -f "$REL_DIR/CMakeCache.txt" ]; then
            echo "Patching $REL_DIR/CMakeCache.txt to remove -Wl,--gc-sections"
            sed -i.bak 's/-Wl,--gc-sections//g' "$REL_DIR/CMakeCache.txt"
        fi
        echo "Removing any occurrences in link.txt files (if present)"
        grep -RIl -- '--gc-sections' "$REL_DIR"/CMakeFiles 2>/dev/null | xargs -r sed -i.bak 's/-Wl,--gc-sections//g'
                        if [ -f "$REL_DIR/build.ninja" ] && [ -f "$REL_DIR/CMakeFiles/rules.ninja" ]; then
                            # Try to reconfigure google_nsync subproject to Darwin (no futex) before retrying
                            if [ -d "$REL_DIR/_deps/google_nsync-build" ] && [ -d "$REL_DIR/_deps/google_nsync-src" ]; then
                                echo "Reconfiguring google_nsync sub-build for Darwin (disable futex)..."
                                # Also ensure platform c++11.futex points to c++11
                                if [ -d "$REL_DIR/_deps/google_nsync-src/platform" ]; then
                                    ( cd "$REL_DIR/_deps/google_nsync-src/platform" && \
                                        [ -d c++11 ] && [ -e c++11.futex ] && rm -rf c++11.futex || true; \
                                        [ -d c++11 ] && ln -s c++11 c++11.futex || true )
                                    # Remove linux-specific platform sources to prevent CMake from selecting them
                                    if [ -d "$REL_DIR/_deps/google_nsync-src/platform/linux" ]; then
                                        echo "Disabling google_nsync platform/linux by renaming it (forces non-futex build)"
                                        mv "$REL_DIR/_deps/google_nsync-src/platform/linux" "$REL_DIR/_deps/google_nsync-src/platform/linux.disabled" 2>/dev/null || true
                                    fi
                                fi
                                if [ -f "$REL_DIR/_deps/google_nsync-src/CMakeLists.txt" ]; then
                                    echo "Patching google_nsync CMakeLists.txt (retry) to exclude platform/linux sources on Apple"
                                    sed -i.bak '/platform\/linux\/src\//d' "$REL_DIR/_deps/google_nsync-src/CMakeLists.txt" || true
                                fi
                                cmake -S "$REL_DIR/_deps/google_nsync-src" -B "$REL_DIR/_deps/google_nsync-build" -G Ninja \
                                    -DCMAKE_SYSTEM_NAME=Darwin -DAPPLE=1 \
                                    -DNSYNC_ENABLE_LINUX_FUTEX=OFF -DNSYNC_USE_FUTEX=OFF -DNSYNC_ENABLE_FUTEX=OFF \
                                    -DNSYNC_PLATFORM=c++11 -DNSYNC_TARGET_PLATFORM=posix -DNSYNC_TARGET_OS=Darwin \
                                    -DCMAKE_OSX_ARCHITECTURES=x86_64 \
                                    $( [ -n "$SYSROOT" ] && printf -- "-DCMAKE_OSX_SYSROOT=%s " "$SYSROOT" ) \
                                    -DCMAKE_OSX_DEPLOYMENT_TARGET=13.3 \
                                    -DCMAKE_C_COMPILER="$WRAP_DIR/x86_64-apple-darwin24-clang" \
                                    -DCMAKE_CXX_COMPILER="$WRAP_DIR/x86_64-apple-darwin24-clang++" \
                                    -DCMAKE_C_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -D_LIBCPP_DISABLE_AVAILABILITY" \
                                    -DCMAKE_CXX_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -D_LIBCPP_DISABLE_AVAILABILITY" \
                                    -DCMAKE_EXE_LINKER_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -Wl,-dead_strip" \
                                    -DCMAKE_SHARED_LINKER_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -Wl,-dead_strip" \
                                    || true
                                cmake --build "$REL_DIR/_deps/google_nsync-build" -j"${JOBS}" || true
                            fi
                            echo "Retrying Ninja build in $REL_DIR with ${JOBS} jobs..."
                            ninja -C "$REL_DIR" -j"${JOBS}" && PATCHED_OK=0 || PATCHED_OK=$?
                        else
            echo "Build system files missing; re-running full configure+build after patching..."
            # Ensure google_nsync will be reconfigured with Darwin settings
            rm -rf "$REL_DIR/_deps/google_nsync-build" "$REL_DIR/_deps/google_nsync-subbuild" 2>/dev/null || true
                        ( cd "$ORT_DIR" && \
                            set +e; \
                            ./build.sh --config Release \
                                --build_shared_lib \
                                --use_coreml \
                                --allow_running_as_root \
                                --use_preinstalled_eigen \
                                --eigen_path /usr/include/eigen3 \
                                --minimal_build extended \
                                --disable_contrib_ops \
                                --cmake_generator Ninja \
                                --cmake_extra_defines CMAKE_OSX_ARCHITECTURES=x86_64 \
                                --cmake_extra_defines CMAKE_OSX_DEPLOYMENT_TARGET=13.3 \
                                --cmake_extra_defines CMAKE_SYSTEM_NAME=Darwin \
                                --cmake_extra_defines CMAKE_CXX_STANDARD=17 \
                                --cmake_extra_defines CMAKE_CXX_STANDARD_REQUIRED=ON \
                                        --cmake_extra_defines CMAKE_BUILD_WITH_INSTALL_RPATH=ON \
                                        --cmake_extra_defines CMAKE_INSTALL_RPATH=@rpath \
                                        --cmake_extra_defines CMAKE_MACOSX_RPATH=ON \
                --cmake_extra_defines NSYNC_ENABLE_FUTEX=OFF \
                --cmake_extra_defines NSYNC_USE_FUTEX=OFF \
                --cmake_extra_defines NSYNC_PLATFORM=c++11 \
                --cmake_extra_defines NSYNC_TARGET_PLATFORM=posix \
                --cmake_extra_defines NSYNC_TARGET_OS=Darwin \
                                --cmake_extra_defines CMAKE_C_COMPILER="$WRAP_DIR/x86_64-apple-darwin24-clang" \
                                --cmake_extra_defines CMAKE_CXX_COMPILER="$WRAP_DIR/x86_64-apple-darwin24-clang++" \
                                --cmake_extra_defines CMAKE_CROSSCOMPILING=ON \
                                --cmake_extra_defines CMAKE_SYSTEM_PROCESSOR=x86_64 \
                                --cmake_extra_defines CMAKE_VERBOSE_MAKEFILE=ON \
                                --cmake_extra_defines onnxruntime_BUILD_UNIT_TESTS=OFF \
                                --cmake_extra_defines onnxruntime_ENABLE_TRAINING=OFF \
                                $( [ -n "$ICONV_INCLUDE" ] && printf -- "--cmake_extra_defines Iconv_INCLUDE_DIR=%s " "$ICONV_INCLUDE" ) \
                                $( [ -n "$ICONV_LIBRARY" ] && printf -- "--cmake_extra_defines Iconv_LIBRARY=%s " "$ICONV_LIBRARY" ) \
                                $( [ "$ICONV_BUILTIN" = 1 ] && printf -- "--cmake_extra_defines Iconv_IS_BUILT_IN=ON " ) \
                $( [ -n "$ZLIB_INCLUDE" ] && printf -- "--cmake_extra_defines ZLIB_INCLUDE_DIR=%s " "$ZLIB_INCLUDE" ) \
                $( [ -n "$ZLIB_LIBRARY" ] && printf -- "--cmake_extra_defines ZLIB_LIBRARY=%s " "$ZLIB_LIBRARY" ) \
                                --cmake_extra_defines CMAKE_C_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -D_LIBCPP_DISABLE_AVAILABILITY -Wno-error -Wno-error=deprecated-pragma -Wno-error=deprecated-declarations" \
                                --cmake_extra_defines CMAKE_CXX_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -D_LIBCPP_DISABLE_AVAILABILITY -Wno-deprecated-this-capture -Wno-error=deprecated-this-capture -Wno-error -Wno-error=deprecated-pragma -Wno-error=deprecated-declarations" \
                                --cmake_extra_defines CMAKE_EXE_LINKER_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -Wl,-dead_strip" \
                                --cmake_extra_defines CMAKE_SHARED_LINKER_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=13.3 -Wl,-dead_strip" \
                                --cmake_extra_defines FLATBUFFERS_BUILD_FLATC=OFF \
                                --cmake_extra_defines FLATBUFFERS_BUILD_TESTS=OFF \
                                --cmake_extra_defines onnxruntime_BUILD_CUSTOM_OP_LIBRARY=OFF \
                                --target onnxruntime \
                                --parallel ${JOBS}; \
                                    STATUS=$?; set -e; exit $STATUS )
                                PATCHED_OK=$?
                fi
                        if [ "$PATCHED_OK" -eq 0 ]; then
            echo "Patched build succeeded. Continuing."
        else
            echo "Patched build still failed. For verbose output, rerun with -j1 in $REL_DIR."
            echo "  ninja -C $REL_DIR -v -j1"
            exit 1
        fi
    else
        echo "Could not locate Release build directory under $BUILD_DIR to patch. Aborting."
        exit 1
    fi
    set -e
fi

# Set environment variables for the custom-built ONNX Runtime
# Auto-detect the Release directory that contains the Darwin library (libonnxruntime.*)
# Ensure we are at repo root before scanning for artifacts
cd "$SCRIPT_ROOT"
BUILD_ROOT="$SCRIPT_ROOT/onnxruntime/build"
ORT_LIB_FILE=$(find "$BUILD_ROOT" -type f \( -name "libonnxruntime.*.dylib" -o -name "libonnxruntime.dylib" -o -name "libonnxruntime.*.a" \) 2>/dev/null | head -n1 || true)
if [ -z "${ORT_LIB_FILE:-}" ]; then
    echo "ONNX Runtime build did not produce a library. Check logs in $BUILD_ROOT and rerun the build step with -j1 for verbose output." >&2
    exit 1
fi
export ORT_LIB_LOCATION="$(dirname "$ORT_LIB_FILE")"

# C API headers
export ORT_INCLUDE_LOCATION=$(pwd)/onnxruntime/include/onnxruntime/core/session

# Force use of system libraries (our custom build)
export ORT_USE_SYSTEM_LIB=1
export ORT_STRATEGY=system

# Set up cross-compilation environment for Rust build
export CC=x86_64-apple-darwin24-clang
export CXX=x86_64-apple-darwin24-clang++
export AR=x86_64-apple-darwin24-ar
export STRIP=x86_64-apple-darwin24-strip

# Force Cargo to target macOS in this shell
export CARGO_BUILD_TARGET=x86_64-apple-darwin

# Add the macOS target if not already added
command -v rustup >/dev/null 2>&1 && rustup target add x86_64-apple-darwin || true

echo "Building Rust project with CoreML-enabled ONNX Runtime..."
# Build for macOS
if command -v cargo >/dev/null 2>&1; then
    cargo build --release --target x86_64-apple-darwin
else
    echo "cargo not found in this container. Skipping Rust build step."
fi
