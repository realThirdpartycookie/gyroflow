// Browser export: WebCodecs decode -> Qt RHI undistort (WebGL2) -> WebCodecs encode -> MP4 (Rust muxer).
use std::sync::{ Arc, atomic::AtomicBool };
use gyroflow_core::StabilizationManager;
use super::{ FFmpegError, RenderOptions };

/// Gyroflow codec name -> WebCodecs encoder family
pub fn webcodecs_codec(codec: &str) -> Option<&'static str> {
    match codec { "H.264/AVC" => Some("avc"), "H.265/HEVC" => Some("hevc"), _ => None }
}

pub fn render<F, F2>(_stab: Arc<StabilizationManager>, _progress: F, _input_file: &gyroflow_core::InputFile, _render_options: &RenderOptions, _trim_range_ind: Option<usize>, _cancel_flag: Arc<AtomicBool>, _pause_flag: Arc<AtomicBool>, _encoder_initialized: F2) -> Result<(), FFmpegError>
    where F: Fn((f64, usize, usize, bool, bool)) + Send + Sync + Clone,
          F2: Fn(String) + Send + Sync + Clone
{
    Err(FFmpegError::Web("not implemented yet".into()))
}
