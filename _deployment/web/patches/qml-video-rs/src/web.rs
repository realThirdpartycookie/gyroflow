// emscripten: MP4/MOV demuxing for the browser backend. WebCodecs decodes individual samples, so the sample
// tables and codec configuration come from here (mp4parse, via telemetry-parser's moov-only reader).
use mp4parse::{ CodecType, SampleEntry, TrackType, VideoCodecSpecific, AudioCodecSpecific };

pub struct Sample {
    pub offset: u64,
    pub size: u32,
    pub pts_us: i64, // presentation time, 0-based
    pub key: bool,
}
pub struct VideoTrack {
    pub codec: String,        // WebCodecs codec string
    pub description: Vec<u8>, // avcC / hvcC payload
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub samples: Vec<Sample>, // decode order
}
pub struct AudioTrack {
    pub timescale: u32,
    pub channels: u16,
    pub sample_rate: u32,
    pub esds: Vec<u8>, // ES_Descriptor, copied verbatim into the output
    pub samples: Vec<(u64, u32, i64, u32)>, // offset, size, pts in timescale ticks (0-based), duration ticks
}
pub struct Media {
    pub video: Option<VideoTrack>,
    pub audio: Option<AudioTrack>,
}

fn avc_codec(c: &[u8]) -> String {
    if c.len() < 4 { return "avc1.640033".into(); }
    format!("avc1.{:02x}{:02x}{:02x}", c[1], c[2], c[3])
}
// ISO/IEC 14496-15 E.3: hvc1.<space><profile>.<compat, bit-reversed>.<tier><level>.<constraints>
fn hevc_codec(c: &[u8]) -> String {
    if c.len() < 13 { return "hvc1.1.6.L153.B0".into(); }
    let space = ["", "A", "B", "C"][(c[1] >> 6) as usize];
    let tier = if c[1] & 0x20 != 0 { 'H' } else { 'L' };
    let compat = u32::from_be_bytes([c[2], c[3], c[4], c[5]]).reverse_bits();
    let mut cons: Vec<String> = c[6..12].iter().map(|b| format!("{b:X}")).collect();
    while cons.len() > 1 && cons.last().map(|x| x == "0").unwrap_or(false) { cons.pop(); }
    format!("hvc1.{space}{}.{compat:X}.{tier}{}.{}", c[1] & 0x1f, c[12], cons.join("."))
}

pub fn demux(path: &str) -> Result<Media, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let ctx = telemetry_parser::util::parse_mp4(&mut std::io::BufReader::with_capacity(1 << 20, file), size).map_err(|e| format!("{path}: {e:?}"))?;

    let mut media = Media { video: None, audio: None };
    for track in &ctx.tracks {
        let Some(entry) = track.stsd.as_ref().and_then(|x| x.descriptions.first()) else { continue };
        let Some(timescale) = track.timescale.map(|x| x.0).filter(|x| *x > 0) else { continue };
        let Some(table) = mp4parse::unstable::create_sample_table(track, 0i64.into()) else { continue };
        let first_pts = table.iter().map(|x| x.start_composition.0).min().unwrap_or(0);

        match (&track.track_type, entry) {
            (TrackType::Video, SampleEntry::Video(v)) if media.video.is_none() => {
                let (codec, description) = match (&v.codec_type, &v.codec_specific) {
                    (CodecType::H264, VideoCodecSpecific::AVCConfig(c)) => (avc_codec(c), c.to_vec()),
                    (CodecType::HEVC, VideoCodecSpecific::HEVCConfig(c)) => (hevc_codec(c), c.to_vec()),
                    (t, _) => return Err(format!("Unsupported video codec {t:?} (browser build supports H.264 and H.265)")),
                };
                let samples: Vec<Sample> = table.iter().map(|x| Sample {
                    offset: x.start_offset.0,
                    size: (x.end_offset.0 - x.start_offset.0) as u32,
                    pts_us: ((x.start_composition.0 - first_pts) as i128 * 1_000_000 / timescale as i128) as i64,
                    key: x.sync,
                }).collect();
                let duration_s = track.duration.map(|d| d.0 as f64 / timescale as f64).unwrap_or(0.0);
                let fps = if duration_s > 0.0 { samples.len() as f64 / duration_s } else { 30.0 };
                media.video = Some(VideoTrack { codec, description, width: v.width as u32, height: v.height as u32, fps, samples });
            }
            (TrackType::Audio, SampleEntry::Audio(a)) if media.audio.is_none() => {
                // Only AAC is passed through: its ES descriptor is all an MP4 decoder needs.
                if let AudioCodecSpecific::ES_Descriptor(es) = &a.codec_specific {
                    if es.audio_codec == CodecType::AAC {
                        media.audio = Some(AudioTrack {
                            timescale: timescale as u32,
                            channels: es.audio_channel_count.unwrap_or(a.channelcount as u16),
                            sample_rate: es.audio_sample_rate.unwrap_or(a.samplerate as u32),
                            esds: es.codec_esds.to_vec(),
                            samples: table.iter().map(|x| (x.start_offset.0, (x.end_offset.0 - x.start_offset.0) as u32,
                                                           x.start_composition.0 - first_pts, (x.end_composition.0 - x.start_composition.0) as u32)).collect(),
                        });
                    }
                }
            }
            _ => { }
        }
    }
    Ok(media)
}

#[cfg(test)]
mod tests {
    #[test]
    fn codec_strings() {
        assert_eq!(super::avc_codec(&[1, 0x64, 0x00, 0x33]), "avc1.640033");
        // Main profile, level 5.1, general_profile_compatibility_flags 0x60000000, progressive/frame-only constraint
        assert_eq!(super::hevc_codec(&[1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 153]), "hvc1.1.6.L153.90");
    }
}

extern "C" {
    fn gf_dec_open(id: *mut std::ffi::c_void, path: *const std::ffi::c_char, codec: *const std::ffi::c_char, desc: *const u8, desc_len: usize, coded_w: u32, coded_h: u32,
                   samples: *const f64, n_samples: usize, ranges: *const f64, n_ranges: usize, mode: i32, out_w: u32, out_h: u32, backlog: *const i32);
    pub fn gf_dec_close(id: *mut std::ffi::c_void);
}

/// Demuxes `path` and starts a WebCodecs decode session `id` (see gf_dec_open in Gyroflow's library_gfweb.js).
/// `ranges_us` limits decoding to those presentation-time ranges. `out_h` > 0 with `out_w` == 0 keeps the aspect.
/// Returns the video track and the output size.
pub fn open_decoder(id: *mut std::ffi::c_void, path: &str, ranges_us: &[(f64, f64)], mode: i32, mut out_w: u32, mut out_h: u32, backlog: *const i32) -> Result<(VideoTrack, u32, u32), String> {
    let video = demux(path)?.video.ok_or_else(|| format!("{path}: no video track"))?;
    if out_w == 0 && out_h > 0 && video.height > 0 { out_w = ((video.width as u64 * out_h as u64 / video.height as u64) as u32 + 1) & !1; }
    if out_w == 0 || out_h == 0 { out_w = video.width; out_h = video.height; }
    let samples: Vec<f64> = video.samples.iter().flat_map(|s| [s.offset as f64, s.size as f64, s.pts_us as f64, s.key as i32 as f64]).collect();
    let ranges: Vec<f64> = ranges_us.iter().flat_map(|r| [r.0, r.1]).collect();
    let path_c = std::ffi::CString::new(path).unwrap_or_default();
    let codec_c = std::ffi::CString::new(video.codec.clone()).unwrap_or_default();
    unsafe {
        gf_dec_open(id, path_c.as_ptr(), codec_c.as_ptr(), video.description.as_ptr(), video.description.len(), video.width, video.height,
                    samples.as_ptr(), video.samples.len(), ranges.as_ptr(), ranges_us.len(), mode, out_w, out_h, backlog);
    }
    Ok((video, out_w, out_h))
}

/// C++ (MDKPlayer_web.cpp) entry: ranges are [from_ms, to_ms] pairs, u64::MAX = open end.
/// info = [fps, coded width, coded height, duration_ms, frame count, out width, out height]
#[no_mangle]
pub extern "C" fn qvr_web_open_decoder(id: *mut std::ffi::c_void, path: *const std::ffi::c_char, ranges_ms: *const u64, n_ranges: usize, mode: i32, out_w: u32, out_h: u32, backlog: *const i32, info: *mut f64) -> bool {
    let path = unsafe { std::ffi::CStr::from_ptr(path) }.to_string_lossy().into_owned();
    let ranges: Vec<(f64, f64)> = (0..n_ranges).map(|i| unsafe {
        let (a, b) = (*ranges_ms.add(i * 2), *ranges_ms.add(i * 2 + 1));
        (a as f64 * 1000.0, if b == u64::MAX { f64::INFINITY } else { b as f64 * 1000.0 })
    }).collect();
    match open_decoder(id, &path, &ranges, mode, out_w, out_h, backlog) {
        Ok((v, w, h)) => {
            let n = v.samples.len() as f64;
            let v = [v.fps, v.width as f64, v.height as f64, n / v.fps.max(1.0) * 1000.0, n, w as f64, h as f64];
            unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), info, v.len()) };
            true
        }
        Err(e) => { eprintln!("qvr_web_open_decoder: {e}"); false }
    }
}
