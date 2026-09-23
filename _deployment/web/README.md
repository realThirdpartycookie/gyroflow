# Gyroflow in the browser (unofficial)

This is Gyroflow's own Qt/QML app compiled to WebAssembly. Nothing is uploaded: videos are read from disk by the browser and processed locally.

**Try it:** https://realthirdpartycookie.github.io/gyroflow/ (Chrome or Edge on desktop recommended; it needs WebGL 2, WebGPU and WebCodecs).

## How it works

| Part | Browser implementation |
|---|---|
| UI | Qt 6.7.3 `wasm_multithread`, the unmodified Gyroflow QML |
| Opening files | Browser file picker / drag & drop, mounted read-only at `/web/` (`src/web/library_gfweb.js`) |
| Preview | `<video>` (hardware decoding) → WebGL 2 texture → Gyroflow's Qt RHI undistortion shader |
| Export | WebCodecs decode → `wgpu_undistort.wgsl` on WebGPU → WebCodecs encode → MP4 muxer in Rust (`src/web/web_export.rs`). AAC audio is passed through. |
| Autosync, thumbnails | WebCodecs decode → Gyroflow's frame processing on worker threads |
| Saving (projects, presets, lens profiles, video) | Browser downloads |
| Settings | `localStorage` |

## Not available in the browser build

- Lens calibration (needs OpenCV). Existing lens profiles (the whole database is bundled) work.
- OpenCV-based sync methods. Autosync uses AKAZE + Almeida and is much slower than on desktop.
- Export formats other than H.264 and H.265. No ProRes, DNxHD, CineForm, EXR or PNG.
- Input formats other than MP4/MOV with H.264/H.265 video (no BRAW, R3D, ...). HDR/10-bit sources are processed as 8-bit.
- Choosing an output folder: exports and saved files land in the browser's downloads.

## Building

The toolchain lives in one folder (`GF_WEB_TOOLCHAIN`, default: the parent folder of this repository). It needs:

- **Qt 6.7.3** for `wasm_multithread`, plus the desktop Qt of the same version for the host tools:
  `aqt install-qt mac desktop 6.7.3 clang_64 -O $GF_WEB_TOOLCHAIN/Qt` and `aqt install-qt all_os wasm 6.7.3 wasm_multithread -m all -O $GF_WEB_TOOLCHAIN/Qt`
- **emsdk 3.1.50** (the version Qt 6.7 is built with) in `$GF_WEB_TOOLCHAIN/emsdk`
- **Rust nightly** with `rust-src` (for `-Zbuild-std`) in `$GF_WEB_TOOLCHAIN/rustup`. Tested with nightly 2026-09-22.
- `_deployment/web/setup-binaryen-shim.sh` once, to wrap emsdk's binaryen for newer LLVM flags.

Then:

```bash
_deployment/web/build.sh
python3 _deployment/web/serve.py _deployment/web/dist 8766
```

`serve.py` sends the COOP/COEP headers that threads need. On hosts that can't set headers (GitHub Pages), `coi-sw.js` adds them through a service worker.

`gftest.mjs` drives the app in headless Chrome for testing (`node _deployment/web/gftest.mjs <out-dir> steps.json`).

`patches/` holds the two patched dependencies: `qml-video-rs` with a web backend for its player, and `cpp_build` with a fix for `#[path]` modules.
