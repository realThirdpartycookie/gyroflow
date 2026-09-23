#!/bin/bash
# Qt 6.7 wasm is built with JS exceptions/sjlj; rustc now always passes -fwasm-exceptions. Drop it (Rust uses panic=abort).
args=(); for a in "$@"; do [[ "$a" == "-fwasm-exceptions" ]] || args+=("$a"); done
exec emcc "${args[@]}"
