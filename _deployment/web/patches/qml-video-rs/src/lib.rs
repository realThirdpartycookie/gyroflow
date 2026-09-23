#![recursion_limit = "256"]

use qmetaobject::*;

pub mod video_player;
pub mod video_item;
#[cfg(target_os = "emscripten")]
pub mod web;

/// emscripten: container metadata for MDKPlayer_web.cpp, since browsers don't expose the frame rate.
/// out = [width, height, fps, duration_s, rotation]
#[cfg(target_os = "emscripten")]
#[no_mangle]
pub extern "C" fn qvr_web_probe(path: *const std::ffi::c_char, out: *mut f64) -> bool {
    let path = unsafe { std::ffi::CStr::from_ptr(path) }.to_string_lossy().into_owned();
    let Ok(file) = std::fs::File::open(&path) else { return false };
    let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    match telemetry_parser::util::get_video_metadata(&mut std::io::BufReader::with_capacity(1 << 20, file), size) {
        Ok(md) => {
            let v = [md.width as f64, md.height as f64, md.fps, md.duration_s, md.rotation as f64];
            unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), out, v.len()) };
            true
        }
        Err(e) => { eprintln!("qvr_web_probe({path}): {e:?}"); false }
    }
}

pub fn register_qml_types() {
    qml_register_type::<video_item::MDKVideoItem>(cstr::cstr!("MDKVideo"), 1, 0, cstr::cstr!("MDKVideo"));
}
