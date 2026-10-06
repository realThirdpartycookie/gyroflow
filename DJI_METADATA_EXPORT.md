# DJI metadata on stabilized exports

MP4/MOV exports of a DJI Action 2 (`dvtm_ac103`) or WM169 clip preserve DJI metadata by default. The advanced export checkbox **Preserve DJI metadata with stabilized motion** controls this independently of **Preserve other tracks**. Projects/CLI presets can opt out with `strip_dji_metadata: true`.

The export first encodes video/audio, then adds the original native `djmd`/`dbgi` track structures. FFmpeg's normal stream copier loses the unknown DJI sample entry and changes the track handler, so the patch bypasses it for these tracks. Native `stsd`, handlers, timing, and unknown protobuf fields are retained. Chunk offsets, sample sizes, track IDs, movie duration units, and the next track ID are rebuilt. Video/audio bytes stay in place. The old movie atom becomes `free` only after the replacement has been written.

`djmd` attitude samples contain absolute stabilized camera orientation, reconstructed from Gyroflow's original and correction quaternions. The inverse DJI coordinate conversion and output image rotation are applied. Sample timing mirrors the pinned telemetry-parser, including fractional sample offsets and the sensor/video FPS ratio. Source readout is zeroed when rolling-shutter correction was applied. Source fisheye calibration is removed because it no longer describes the exported geometry; video resolution is updated. Original exposure and other capture metadata remain.

DJI's private maker atom and camera-authored metadata, including original source thumbnails, are retained. The `dbgi` stream and thumbnails describe the source capture and are not recomputed from the stabilized pixels. Existing ordinary output metadata is merged when it uses the same `ilst` namespace. DJI Mimo recognition is **not verified or guaranteed**; preserving a track alone did not preserve the camera's container structure.

Current limits: full-length exports at original FPS/speed; non-fragmented MP4/MOV; Action 2/WM169 protobuf layouts; frames without active metadata checksums. Trim/speed changes produce a clear error with the option to disable DJI preservation, rather than exporting stale timing. Other DJI protobuf layouts produce an export error. Failed preparation leaves the original output movie atom active.

## Validation

Run the focused, dependency-free tests:

```sh
rustc --edition=2024 --test src/core/dji_metadata.rs -o /tmp/gyroflow-dji-tests
/tmp/gyroflow-dji-tests
```

The supplied DJI_0073.MP4 was used for native track round-trip and stabilization checks. Both DJI tracks remain readable with 61 packets, and 1,213 orientation samples are replaced. Gyroflow re-reads the resulting curve with a maximum orientation error below 0.0001 radians. All 61 video frames and audio decode successfully. Six focused tests pass, including MP4 header dependency ordering.

The desktop app builds with the installed OpenCV 5. A version-aware compatibility module maps the moved geometry/corner-detection functions and overload names while retaining the OpenCV 4 branch. Calibration and automatic synchronization remain enabled.

The packaged macOS application was also tested end to end: its CLI rendered the supplied clip at 1536x2048/50 fps, and all 61 video frames and audio decoded. Both native DJI tracks contain 61 packets. Camera-curve JSON exported by the same app confirms 1,213 corrected samples with a maximum orientation error of 6.4e-7 radians (including the 90-degree output rotation). The UI starts, displays the supplied clip, and exposes the checked preservation option. The bundle is ad-hoc signed for local use and its startup loads no Homebrew libraries. DJI Mimo recognition remains untested.

## Local macOS build

Use a matching Qt installation, FFmpeg 9, OpenCV 5, and an MDK SDK. `MDK_SDK` needs a trailing slash because the upstream video widget concatenates its library path. Homebrew's OpenCV package advertises every module; limiting the link list avoids pulling unrelated modules into the application.

```sh
MDK_SDK="/path/to/mdk-sdk/" \
EXTRA_LINK_PATHS="/opt/homebrew/opt/x264/lib;/opt/homebrew/opt/x265/lib" \
OPENCV_LINK_LIBS="opencv_core,opencv_imgproc,opencv_calib,opencv_features,opencv_geometry,opencv_objdetect,opencv_optflow,opencv_video" \
cargo build --release
```

Package the resulting executable with Qt, MDK, and all transitive libraries. Rewrite both library dependencies and build-machine rpaths before signing the bundle. Otherwise OpenCV can load another OpenMP runtime from Homebrew and abort at startup.
