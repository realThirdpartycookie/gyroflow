# Environment for the browser build. GF_WEB_TOOLCHAIN = folder with Qt/, emsdk/, rustup/ and binaryen-shim/ (see README.md)
WEB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export QS="${GF_WEB_TOOLCHAIN:-$(cd "$WEB/../../.." && pwd)}"
source "$QS/emsdk/emsdk_env.sh" >/dev/null 2>&1
export QTW=$QS/Qt/6.7.3/wasm_multithread
export QMAKE=$QTW/bin/qmake
export CXXFLAGS_wasm32_unknown_emscripten="-pthread -ffile-prefix-map=$HOME=~"
export CFLAGS_wasm32_unknown_emscripten="-pthread -ffile-prefix-map=$HOME=~"
export RUSTUP_HOME=$QS/rustup RUSTUP_TOOLCHAIN=nightly
export RUSTFLAGS="-Ctarget-feature=+atomics,+bulk-memory -Cpanic=abort -Clinker=$WEB/emcc-link.sh --remap-path-prefix=$HOME=~"
export CARGO_BUILD_TARGET=wasm32-unknown-emscripten
export EM_BINARYEN_ROOT=$QS/binaryen-shim
export QT_WASM_LINK_ARGS=$QS/qtlink.args
