// Browser build of the rendering module. Gyroflow's own render queue and MDK frame processor are reused as-is;
// underneath, frames come from WebCodecs (hardware decoding) via the MDKPlayer web backend, and export runs on the
// GPU (WebCodecs decode -> Qt RHI undistort on WebGL2 -> WebCodecs encode, see web_export.rs).
use std::sync::{ Arc, atomic::AtomicBool };
use gyroflow_core::StabilizationManager;

#[path = "rendering/render_queue.rs"]  pub mod render_queue;
#[path = "rendering/mdk_processor.rs"] pub mod mdk_processor;
#[path = "web/web_export.rs"]          pub mod web_export;

pub use ffmpeg_processor::FFmpegError;
pub use render_queue::RenderOptions;

pub mod ffmpeg_video {
    pub struct RateControl { pub out_timestamp_us: i64, pub repeat_times: i64, pub repeat_interval: i64 }
    impl Default for RateControl { fn default() -> Self { Self { out_timestamp_us: 0, repeat_times: 1, repeat_interval: 0 } } }
}

pub mod ffmpeg_processor {
    use ffmpeg_next::format;
    use gyroflow_core::filesystem::FilesystemError;

    #[derive(Debug, Clone, Default)]
    pub struct VideoInfo {
        pub duration_ms: f64,
        pub frame_count: usize,
        pub fps: f64,
        pub width: u32,
        pub height: u32,
        pub bitrate: f64, // in Mbps
        pub rotation: i32,
        pub created_at: Option<u64>
    }

    #[derive(Debug)]
    pub enum FFmpegError {
        EncoderNotFound,
        DecoderNotFound,
        GPUDecodingFailed,
        PixelFormatNotSupported((format::Pixel, Vec<format::Pixel>, Option<format::Pixel>)),
        InternalError(ffmpeg_next::Error),
        CannotOpenInputFile((String, FilesystemError)),
        CannotOpenOutputFile((String, FilesystemError)),
        Web(String),
    }
    impl std::fmt::Display for FFmpegError {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            match self {
                FFmpegError::EncoderNotFound => write!(f, "Encoder not found"),
                FFmpegError::DecoderNotFound => write!(f, "Decoder not found"),
                FFmpegError::GPUDecodingFailed => write!(f, "GPU decoding failed, please try again."),
                FFmpegError::PixelFormatNotSupported(v) => write!(f, "Pixel format {:?} is not supported. Supported ones: {:?}. Optimal choice: {:?}", v.0, v.1, v.2),
                FFmpegError::InternalError(e) => write!(f, "ffmpeg error: {e:?}"),
                FFmpegError::CannotOpenInputFile((url, e)) => write!(f, "Cannot open input file {url}: {e:?}"),
                FFmpegError::CannotOpenOutputFile((url, e)) => write!(f, "Cannot open output file {url}: {e:?}"),
                FFmpegError::Web(e) => write!(f, "{e}"),
            }
        }
    }
    impl std::error::Error for FFmpegError { }
    impl From<ffmpeg_next::Error> for FFmpegError { fn from(e: ffmpeg_next::Error) -> Self { FFmpegError::InternalError(e) } }
}

pub mod ffmpeg_video_converter {
    use ffmpeg_next::{ format::Pixel, frame };
    use super::FFmpegError;

    /// CPU resize + pixel format conversion for the RGBA/BGRA frames the web decoder produces (autosync, thumbnails).
    #[derive(Default)]
    pub struct Converter;
    impl Converter {
        pub fn scale(&mut self, src: &mut frame::Video, format: Pixel, width: u32, height: u32) -> Result<frame::Video, FFmpegError> {
            let (sw, sh, ss) = (src.width() as usize, src.height() as usize, src.stride(0));
            let bgra = src.format() == Pixel::BGRA;
            if !matches!(src.format(), Pixel::RGBA | Pixel::BGRA) || sw == 0 || sh == 0 || !matches!(format, Pixel::RGBA | Pixel::GRAY8) {
                return Err(FFmpegError::PixelFormatNotSupported((src.format(), vec![Pixel::RGBA, Pixel::BGRA], None)));
            }
            let mut out = frame::Video::new(format, width, height);
            let (dw, dh, ds, bpp) = (width as usize, height as usize, out.stride(0), format.bytes_per_pixel());
            let input = src.data(0);
            let output = out.data_mut(0);
            for y in 0..dh {
                let sy = ((y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
                let (y0, fy) = (sy as usize, sy.fract());
                let y1 = (y0 + 1).min(sh - 1);
                for x in 0..dw {
                    let sx = ((x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
                    let (x0, fx) = (sx as usize, sx.fract());
                    let x1 = (x0 + 1).min(sw - 1);
                    let px = |c: usize| {
                        let p = |xx: usize, yy: usize| input[yy * ss + xx * 4 + c] as f32;
                        (p(x0, y0) * (1.0 - fx) + p(x1, y0) * fx) * (1.0 - fy) + (p(x0, y1) * (1.0 - fx) + p(x1, y1) * fx) * fy
                    };
                    let (r, g, b) = if bgra { (px(2), px(1), px(0)) } else { (px(0), px(1), px(2)) };
                    let o = y * ds + x * bpp;
                    if bpp == 1 {
                        output[o] = (0.299 * r + 0.587 * g + 0.114 * b).round() as u8; // BT.601 luma, like swscale's GRAY8
                    } else {
                        output[o] = r as u8; output[o + 1] = g as u8; output[o + 2] = b as u8; output[o + 3] = px(3) as u8;
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Frame source for autosync, thumbnails etc. Always the MDK processor, whose web backend decodes with WebCodecs.
pub struct VideoProcessor { inner: mdk_processor::MDKProcessor }
impl VideoProcessor {
    pub fn from_file(url: &str, gpu_decoding: bool, _gpu_decoder_index: usize, decoder_options: Option<ffmpeg_next::Dictionary>) -> Result<Self, FFmpegError> {
        Ok(Self { inner: mdk_processor::MDKProcessor::from_file(url, decoder_options, gpu_decoding) })
    }
    pub fn get_video_info(url: &str) -> Result<ffmpeg_processor::VideoInfo, ffmpeg_next::Error> {
        let path = gyroflow_core::filesystem::url_to_path(url);
        let file = std::fs::File::open(&path).map_err(|e| ffmpeg_next::Error::Unsupported(e.to_string()))?;
        let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
        let md = gyroflow_core::telemetry_parser::util::get_video_metadata(&mut std::io::BufReader::with_capacity(1 << 20, file), size).map_err(|e| ffmpeg_next::Error::Unsupported(e.to_string()))?;
        Ok(ffmpeg_processor::VideoInfo {
            duration_ms: md.duration_s * 1000.0,
            frame_count: (md.duration_s * md.fps).round() as usize,
            fps: md.fps,
            width: md.width as u32,
            height: md.height as u32,
            bitrate: size as f64 * 8.0 / md.duration_s.max(0.001) / 1024.0 / 1024.0,
            rotation: md.rotation,
            created_at: None,
        })
    }
    pub fn on_frame<F>(&mut self, cb: F) where F: FnMut(i64, &mut ffmpeg_next::frame::Video, Option<&mut ffmpeg_next::frame::Video>, &mut ffmpeg_video_converter::Converter, &mut ffmpeg_video::RateControl) -> Result<(), FFmpegError> + 'static {
        self.inner.on_frame(cb)
    }
    pub fn start_decoder_only(&mut self, ranges: Vec<(f64, f64)>, cancel_flag: Arc<AtomicBool>) -> Result<(), FFmpegError> {
        self.inner.start_decoder_only(ranges, cancel_flag)
    }
}

pub fn render<F, F2>(stab: Arc<StabilizationManager>, progress: F, input_file: &gyroflow_core::InputFile, render_options: &RenderOptions, _gpu_decoder_index: i32, trim_range_ind: Option<usize>, cancel_flag: Arc<AtomicBool>, pause_flag: Arc<AtomicBool>, encoder_initialized: F2) -> Result<(), FFmpegError>
    where F: Fn((f64, usize, usize, bool, bool)) + Send + Sync + Clone,
          F2: Fn(String) + Send + Sync + Clone
{
    web_export::render(stab, progress, input_file, render_options, trim_range_ind, cancel_flag, pause_flag, encoder_initialized)
}

pub fn init_log() { }
pub fn clear_log() { }
pub fn get_log() -> String { String::new() }
pub fn append_log(msg: &str) { ::log::debug!("{msg}"); }
pub fn set_gpu_type_from_name(_name: &str) { }
pub fn fps_to_rational(fps: f64) -> ffmpeg_next::Rational {
    if (fps.fract() - 0.976).abs() < 0.01 || (fps.fract() - 0.97).abs() < 0.01 { ffmpeg_next::Rational::new((fps.ceil() * 1000.0) as i32, 1001) }
    else { ffmpeg_next::Rational::new((fps * 1000.0).round() as i32, 1000) }
}
pub fn get_default_encoder(codec: &str, _gpu: bool) -> String { web_export::webcodecs_codec(codec).map(|_| "WebCodecs".into()).unwrap_or_default() }
pub fn get_encoder_options(_name: &str) -> String { String::new() }
pub fn get_possible_encoders(codec: &str, _use_gpu: bool) -> Vec<(&'static str, bool)> {
    if web_export::webcodecs_codec(codec).is_some() { vec![("WebCodecs", true)] } else { vec![] }
}
