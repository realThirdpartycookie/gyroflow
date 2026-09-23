#!/bin/bash
# Creates $QS/binaryen-shim: emsdk's LLVM/binaryen, with the binaryen tools wrapped so they accept the
# feature flags newer rustc/LLVM passes (binaryen from emsdk 3.1.50 predates them).
set -e
source "$(dirname "$0")/env.sh"
S=$QS/binaryen-shim; E=$QS/emsdk/upstream
mkdir -p "$S/bin"; ln -sfn "$E/lib" "$S/lib"
for f in "$E"/bin/*; do ln -sfn "$f" "$S/bin/${f##*/}"; done
for t in wasm-opt wasm-as wasm-ctor-eval wasm-dis wasm-emscripten-finalize wasm-metadce wasm-split wasm2js; do
  rm -f "$S/bin/$t"
  printf '#!/bin/bash\nargs=(); for a in "$@"; do case "$a" in --enable-bulk-memory-opt|--enable-call-indirect-overlong) ;; *) args+=("$a");; esac; done\nexec "%s" "${args[@]}"\n' "$E/bin/$t" > "$S/bin/$t"
  chmod +x "$S/bin/$t"
done
