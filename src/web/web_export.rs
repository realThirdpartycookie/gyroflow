// Browser export, all heavy lifting on the GPU:
//   WebCodecs decode (hardware) -> WebGPU running Gyroflow's export shader (src/core/gpu/wgpu_undistort.wgsl, the one
//   desktop export uses via wgpu) -> WebCodecs encode (hardware) -> MP4 muxed here, AAC audio passed through untouched.
// The per-frame kernel parameters come from gyroflow-core (gf_export_frame_params); the GPU loop lives in
// library_gfweb.js (gf_export_run) on the browser main thread. This render() runs on a render-queue thread and waits.
// (Qt's RHI shader in src/qt_gpu is the bilinear preview shader: ~10x slower at export sizes, lower quality.)
use std::sync::{ Arc, Mutex, Condvar, atomic::{ AtomicBool, AtomicUsize, Ordering::SeqCst } };
use std::collections::HashMap;
use std::ffi::{ c_char, CStr, CString };
use gyroflow_core::{ StabilizationManager, filesystem, stabilization::{ Stabilization, ComputeParams, RGBA8, Interpolation }, gpu::{ Buffers, BufferDescription } };
use super::{ FFmpegError, RenderOptions };

/// Gyroflow codec name -> WebCodecs encoder family
pub fn webcodecs_codec(codec: &str) -> Option<&'static str> {
    match codec { "H.264/AVC" => Some("avc"), "H.265/HEVC" => Some("hevc"), _ => None }
}

unsafe extern "C" {
    fn gf_export_start(job: usize, cfg: *const c_char);
    fn gf_export_run(job: usize, shader: *const c_char, coeffs: *const f32, n_coeffs: usize, in_w: u32, in_h: u32, out_w: u32, out_h: u32, frame_dur_us: f64);
    fn gf_export_cancel(job: usize);
    fn gf_dec_close(job: usize);
    fn gf_sink_write(job: usize, data: *const u8, len: usize);
    fn gf_sink_finish(job: usize, header: *const u8, header_len: usize, moov: *const u8, moov_len: usize, name: *const c_char);
}

#[derive(Default)]
struct State { gpu_done: bool, encoded: bool, error: Option<String> }

struct Job {
    plane: Mutex<Stabilization>,
    in_size: (usize, usize),
    out_size: (usize, usize),
    offset_us: i64,
    total: usize,
    rendered: AtomicUsize,
    state: Mutex<State>,
    cv: Condvar,
    mux: Mutex<Mux>,
    scratch: Mutex<(Vec<u8>, Vec<u8>, Vec<u8>)>, // kernel params, matrices, mesh data for the C++ pass
}
lazy_static::lazy_static! {
    static ref JOBS: Mutex<HashMap<usize, Arc<Job>>> = Mutex::new(HashMap::new());
}
fn job(id: usize) -> Option<Arc<Job>> { JOBS.lock().unwrap().get(&id).cloned() }
fn fail(id: usize, e: String) {
    if let Some(j) = job(id) { j.state.lock().unwrap().error.get_or_insert(e); j.cv.notify_all(); }
}

// ------------------------------------------------------------------ called from JS / the C++ pass (main thread)

#[unsafe(no_mangle)]
pub extern "C" fn gf_export_config(id: usize, desc: *mut u8, len: usize) {
    unsafe extern "C" { fn free(p: *mut std::ffi::c_void); }
    if let Some(j) = job(id) { j.mux.lock().unwrap().video_desc = unsafe { std::slice::from_raw_parts(desc, len) }.to_vec(); }
    unsafe { free(desc as *mut _); }
}
#[unsafe(no_mangle)]
pub extern "C" fn gf_export_chunk(id: usize, len: u32, key: i32) {
    let Some(j) = job(id) else { return };
    let n = {
        let mut m = j.mux.lock().unwrap();
        m.video.push((len, key != 0));
        m.mdat_len += len as u64;
        m.video.len()
    };
    if n == j.total { j.state.lock().unwrap().encoded = true; j.cv.notify_all(); }
}
#[unsafe(no_mangle)]
pub extern "C" fn gf_export_done(id: usize) {
    if let Some(j) = job(id) { j.state.lock().unwrap().encoded = true; j.cv.notify_all(); }
}
#[unsafe(no_mangle)]
pub extern "C" fn gf_export_error(id: usize, msg: *mut c_char) {
    unsafe extern "C" { fn free(p: *mut std::ffi::c_void); }
    let m = unsafe { CStr::from_ptr(msg) }.to_string_lossy().into_owned();
    unsafe { free(msg as *mut _); }
    fail(id, m);
}

#[repr(C)]
pub struct FrameParams { params: *const u8, params_len: u32, matrices: *const u8, matrices_len: u32, mesh: *const u8, mesh_len: u32 }

/// Kernel params for the frame at `ts_us` (source time), laid out for wgpu_undistort.wgsl. Valid until the next call.
#[unsafe(no_mangle)]
pub extern "C" fn gf_export_frame_params(id: usize, ts_us: f64, out: *mut FrameParams) -> bool {
    let Some(j) = job(id) else { return false };
    let ts = ts_us.round() as i64 + j.offset_us;
    let buffers = Buffers {
        input:  BufferDescription { size: (j.in_size.0, j.in_size.1, j.in_size.0 * 4), ..Default::default() },
        output: BufferDescription { size: (j.out_size.0, j.out_size.1, j.out_size.0 * 4), ..Default::default() },
    };
    let mut t = j.plane.lock().unwrap().get_frame_transform_at::<RGBA8>(ts, None, &buffers);
    t.kernel_params.max_pixel_value = 1.0; // rgba8 textures sample as 0..1
    t.kernel_params.pixel_value_limit = 1.0;
    let mut s = j.scratch.lock().unwrap();
    s.0 = bytemuck::bytes_of(&t.kernel_params).to_vec();
    s.1 = bytemuck::cast_slice(&t.matrices).to_vec();
    s.2 = bytemuck::cast_slice(&t.mesh_data).to_vec();
    unsafe { *out = FrameParams { params: s.0.as_ptr(), params_len: s.0.len() as u32, matrices: s.1.as_ptr(), matrices_len: s.1.len() as u32, mesh: s.2.as_ptr(), mesh_len: s.2.len() as u32 }; }
    true
}
#[unsafe(no_mangle)]
pub extern "C" fn gf_export_rendered(id: usize, finished: i32, error: i32) {
    let Some(j) = job(id) else { return };
    if error != 0 { return fail(id, "Decoding failed".into()); }
    if finished != 0 { j.state.lock().unwrap().gpu_done = true; } else { j.rendered.fetch_add(1, SeqCst); }
    j.cv.notify_all();
}

// ------------------------------------------------------------------ the render-queue entry point

pub fn render<F, F2>(stab: Arc<StabilizationManager>, progress: F, input_file: &gyroflow_core::InputFile, render_options: &RenderOptions, trim_range_ind: Option<usize>, cancel_flag: Arc<AtomicBool>, _pause_flag: Arc<AtomicBool>, encoder_initialized: F2) -> Result<(), FFmpegError>
    where F: Fn((f64, usize, usize, bool, bool)) + Send + Sync + Clone,
          F2: Fn(String) + Send + Sync + Clone
{
    let web = |e: String| FFmpegError::Web(e);
    let family = webcodecs_codec(&render_options.codec)
        .ok_or_else(|| web(format!("{} export isn't available in the browser, please choose H.264/AVC or H.265/HEVC.", render_options.codec)))?;
    let path = filesystem::url_to_path(&input_file.url);
    let media = qml_video_rs::web::demux(&path).map_err(web)?;
    let video = media.video.as_ref().ok_or_else(|| web("No video track".into()))?;

    let (fps, duration_ms, ranges_ms, in_size, org_out, offset_us) = {
        let p = stab.params.read();
        let ranges: Vec<(f64, f64)> = match trim_range_ind { Some(i) => vec![p.trim_ranges[i]], None => p.trim_ranges.clone() }
            .into_iter().map(|(a, b)| (a * p.duration_ms, b * p.duration_ms)).collect();
        (p.fps, p.duration_ms, ranges, p.size, p.output_size, (p.frame_offset as f64 / p.fps * 1_000_000.0).round() as i64)
    };
    let ranges_us: Vec<(f64, f64)> = if ranges_ms.is_empty() { vec![(f64::NEG_INFINITY, f64::INFINITY)] } else { ranges_ms.iter().map(|r| (r.0 * 1000.0, r.1 * 1000.0)).collect() };
    let in_range = |t: f64| ranges_us.iter().any(|r| t >= r.0 && t <= r.1);
    let total = video.samples.iter().filter(|s| in_range(s.pts_us as f64)).count();
    if total == 0 { return Err(web("Nothing to render in the selected range".into())); }
    let out_size = ((render_options.output_width & !1).max(2), (render_options.output_height & !1).max(2));

    let mut plane = Stabilization::default();
    plane.interpolation = Interpolation::from(render_options.interpolation.as_str());
    plane.init_size(in_size, org_out);
    plane.set_compute_params(ComputeParams::from_manager(&stab));
    // desktop export's shader, specialised for this lens model and rgba8 textures (same substitutions as gpu/wgpu.rs)
    let shader = {
        let cp = ComputeParams::from_manager(&stab);
        let mut k = include_str!("../core/gpu/wgpu_undistort.wgsl").to_string();
        let mut f = cp.distortion_model.wgsl_functions().to_string();
        f.push_str(cp.digital_lens.as_ref().map(|x| x.wgsl_functions()).unwrap_or("fn digital_undistort_point(uv: vec2<f32>) -> vec2<f32> { return uv; }\nfn digital_distort_point(uv: vec2<f32>) -> vec2<f32> { return uv; }"));
        k = k.replace("LENS_MODEL_FUNCTIONS;", &f).replace("SCALAR", "f32");
        while let Some(pos) = k.find("{buffer_input}") { k.replace_range(pos..k.find("{/buffer_input}").unwrap() + 15, ""); }
        CString::new(k).unwrap_or_default()
    };

    let timescale = (fps * 1000.0).round() as u32;
    let id = Box::into_raw(Box::new(0u8)) as usize; // unique job id, also the decode session id in JS
    let job = Arc::new(Job {
        plane: Mutex::new(plane),
        in_size: (video.width as usize, video.height as usize),
        out_size,
        offset_us,
        total,
        rendered: AtomicUsize::new(0),
        state: Mutex::new(State::default()),
        cv: Condvar::new(),
        mux: Mutex::new(Mux { family, width: out_size.0 as u32, height: out_size.1 as u32, timescale, ..Default::default() }),
        scratch: Mutex::new(Default::default()),
    });
    JOBS.lock().unwrap().insert(id, job.clone());
    let _cleanup = scopeguard(move || { JOBS.lock().unwrap().remove(&id); unsafe { drop(Box::from_raw(id as *mut u8)); } });

    let bitrate = (render_options.bitrate.max(1.0) * 1_000_000.0) as u64;
    let keyint = ((render_options.keyframe_distance.max(0.1)) * fps).round().max(1.0) as u32;
    let cfg = CString::new(serde_json::json!({ "family": family, "width": out_size.0, "height": out_size.1, "bitrate": bitrate, "fps": fps, "keyint": keyint }).to_string()).unwrap();
    unsafe { gf_export_start(id, cfg.as_ptr()); }
    qml_video_rs::web::open_decoder(id as *mut _, &path, &ranges_us, 1, video.width, video.height, std::ptr::null()).map_err(web)?;
    encoder_initialized(format!("WebCodecs {} ({}x{})", family.to_uppercase(), out_size.0, out_size.1));

    let coeffs = &gyroflow_core::stabilization::COEFFS;
    unsafe { gf_export_run(id, shader.as_ptr(), coeffs.as_ptr(), coeffs.len(), video.width, video.height, out_size.0 as u32, out_size.1 as u32, 1_000_000.0 / fps); }

    // Wait, reporting progress
    let mut st = job.state.lock().unwrap();
    loop {
        if st.error.is_some() || (st.gpu_done && st.encoded) { break; }
        if cancel_flag.load(SeqCst) { st.error = Some("cancelled".into()); break; }
        let (s, _) = job.cv.wait_timeout(st, std::time::Duration::from_millis(250)).unwrap();
        st = s;
        let n = job.mux.lock().unwrap().video.len();
        progress((n as f64 / total as f64, n, total, false, false));
    }
    let error = st.error.take();
    drop(st);
    unsafe { gf_dec_close(id); }
    if let Some(e) = error {
        unsafe { gf_export_cancel(id); }
        return if e == "cancelled" { Ok(()) } else { Err(web(e)) };
    }

    // Audio passthrough (AAC samples inside the rendered ranges), appended to mdat after the video
    let mut mux = std::mem::take(&mut *job.mux.lock().unwrap());
    if render_options.audio {
        if let Some(a) = media.audio.as_ref() {
            if let Ok(mut f) = std::fs::File::open(&path) {
                use std::io::{ Read, Seek, SeekFrom };
                let mut buf = Vec::new();
                for &(off, size, pts, dur) in &a.samples {
                    if !in_range(pts as f64 * 1_000_000.0 / a.timescale as f64) { continue; }
                    buf.resize(size as usize, 0);
                    if f.seek(SeekFrom::Start(off)).is_err() || f.read_exact(&mut buf).is_err() { break; }
                    unsafe { gf_sink_write(id, buf.as_ptr(), buf.len()); }
                    mux.audio.push((size, dur));
                    mux.mdat_len += size as u64;
                }
                mux.audio_cfg = Some((a.timescale, a.channels, a.sample_rate, a.esds.clone()));
            }
        }
    }
    let (header, moov) = mux.finish();
    let name = CString::new(if render_options.output_filename.is_empty() { "gyroflow_stabilized.mp4".into() } else { render_options.output_filename.clone() }).unwrap();
    unsafe { gf_sink_finish(id, header.as_ptr(), header.len(), moov.as_ptr(), moov.len(), name.as_ptr()); }
    let _ = duration_ms;
    progress((1.0, total, total, true, false));
    Ok(())
}

struct ScopeGuard<F: FnOnce()>(Option<F>);
impl<F: FnOnce()> Drop for ScopeGuard<F> { fn drop(&mut self) { if let Some(f) = self.0.take() { f() } } }
fn scopeguard<F: FnOnce()>(f: F) -> ScopeGuard<F> { ScopeGuard(Some(f)) }

// ------------------------------------------------------------------ minimal MP4 writer (moov at the end)

#[derive(Default)]
struct Mux {
    family: &'static str,
    width: u32,
    height: u32,
    timescale: u32,         // video: fps * 1000, each frame lasts 1000
    video_desc: Vec<u8>,    // avcC / hvcC from the encoder
    video: Vec<(u32, bool)>, // size, keyframe
    audio: Vec<(u32, u32)>, // size, duration
    audio_cfg: Option<(u32, u16, u32, Vec<u8>)>, // timescale, channels, sample rate, esds
    mdat_len: u64,
}

fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend(((body.len() + 8) as u32).to_be_bytes());
    v.extend(typ);
    v.extend(body);
    v
}
fn fullbox(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
    b.extend(body);
    bx(typ, &b)
}
fn cat(parts: &[&[u8]]) -> Vec<u8> { parts.concat() }
const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000];
fn be32s(v: &[u32]) -> Vec<u8> { v.iter().flat_map(|x| x.to_be_bytes()).collect() }

impl Mux {
    fn stbl(entry: Vec<u8>, sizes: &[u32], durations: &[u32], sync: Option<Vec<u32>>, first_offset: u64) -> Vec<u8> {
        let mut stts: Vec<(u32, u32)> = Vec::new();
        for &d in durations { match stts.last_mut() { Some(l) if l.1 == d => l.0 += 1, _ => stts.push((1, d)) } }
        let stts = fullbox(b"stts", 0, 0, &cat(&[&(stts.len() as u32).to_be_bytes(), &stts.iter().flat_map(|x| be32s(&[x.0, x.1])).collect::<Vec<u8>>()]));
        let stsz = fullbox(b"stsz", 0, 0, &cat(&[&0u32.to_be_bytes(), &(sizes.len() as u32).to_be_bytes(), &be32s(sizes)]));
        let stsc = fullbox(b"stsc", 0, 0, &be32s(&[1, 1, 1, 1])); // every sample is its own chunk
        let mut off = first_offset;
        let co64 = fullbox(b"co64", 0, 0, &cat(&[&(sizes.len() as u32).to_be_bytes(), &sizes.iter().flat_map(|s| { let o = off; off += *s as u64; o.to_be_bytes() }).collect::<Vec<u8>>()]));
        let stsd = fullbox(b"stsd", 0, 0, &cat(&[&1u32.to_be_bytes(), &entry]));
        let stss = sync.map(|s| fullbox(b"stss", 0, 0, &cat(&[&(s.len() as u32).to_be_bytes(), &be32s(&s)]))).unwrap_or_default();
        bx(b"stbl", &cat(&[&stsd, &stts, &stss, &stsz, &stsc, &co64]))
    }
    fn trak(id: u32, handler: &[u8; 4], name: &str, timescale: u32, duration: u64, movie_duration_ms: u32, width: u32, height: u32, media_header: Vec<u8>, stbl: Vec<u8>) -> Vec<u8> {
        let volume: u16 = if handler == b"soun" { 0x0100 } else { 0 };
        let tkhd = fullbox(b"tkhd", 0, 3, &cat(&[&be32s(&[0, 0, id, 0, movie_duration_ms, 0, 0]), &0u32.to_be_bytes(), &volume.to_be_bytes(), &0u16.to_be_bytes(), &be32s(&MATRIX), &be32s(&[width << 16, height << 16])]));
        let mdhd = fullbox(b"mdhd", 0, 0, &cat(&[&be32s(&[0, 0, timescale, duration as u32]), &0x55c4u16.to_be_bytes(), &0u16.to_be_bytes()]));
        let hdlr = fullbox(b"hdlr", 0, 0, &cat(&[&0u32.to_be_bytes(), handler, &[0u8; 12], name.as_bytes(), &[0]]));
        let dinf = bx(b"dinf", &fullbox(b"dref", 0, 0, &cat(&[&1u32.to_be_bytes(), &fullbox(b"url ", 0, 1, &[])])));
        let minf = bx(b"minf", &cat(&[&media_header, &dinf, &stbl]));
        bx(b"trak", &cat(&[&tkhd, &bx(b"mdia", &cat(&[&mdhd, &hdlr, &minf]))]))
    }
    /// Returns (ftyp + mdat header, moov). The file is header + mdat payload (already sent to the sink) + moov.
    fn finish(&self) -> (Vec<u8>, Vec<u8>) {
        let codec4: &[u8; 4] = if self.family == "hevc" { b"hvc1" } else { b"avc1" };
        let ftyp = bx(b"ftyp", &cat(&[b"isom", &0x200u32.to_be_bytes(), b"isom", b"iso2", codec4, b"mp41"]));
        let mut header = ftyp;
        header.extend(1u32.to_be_bytes());
        header.extend(b"mdat");
        header.extend((16 + self.mdat_len).to_be_bytes());
        let data_start = header.len() as u64;

        let n = self.video.len() as u64;
        let movie_ms = (n * 1000 * 1000 / self.timescale.max(1) as u64) as u32;
        let mut entry = cat(&[&[0u8; 6], &1u16.to_be_bytes(), &[0u8; 16], &(self.width as u16).to_be_bytes(), &(self.height as u16).to_be_bytes(),
                              &be32s(&[0x480000, 0x480000, 0]), &1u16.to_be_bytes(), &[0u8; 32], &0x18u16.to_be_bytes(), &0xffffu16.to_be_bytes()]);
        entry.extend(bx(if self.family == "hevc" { b"hvcC" } else { b"avcC" }, &self.video_desc));
        let sizes: Vec<u32> = self.video.iter().map(|x| x.0).collect();
        let sync: Vec<u32> = self.video.iter().enumerate().filter(|x| x.1.1).map(|x| x.0 as u32 + 1).collect();
        let vmhd = fullbox(b"vmhd", 0, 1, &[0u8; 8]);
        let vtrak = Self::trak(1, b"vide", "VideoHandler", self.timescale, n * 1000, movie_ms, self.width, self.height, vmhd,
                               Self::stbl(bx(codec4, &entry), &sizes, &vec![1000; sizes.len()], if sync.len() == sizes.len() { None } else { Some(sync) }, data_start));
        let video_bytes: u64 = sizes.iter().map(|x| *x as u64).sum();

        let mut traks = vtrak;
        if let (Some((ts, channels, rate, esds)), false) = (&self.audio_cfg, self.audio.is_empty()) {
            let entry = cat(&[&[0u8; 6], &1u16.to_be_bytes(), &[0u8; 8], &channels.to_be_bytes(), &16u16.to_be_bytes(), &[0u8; 4],
                              &((*rate).min(65535) << 16).to_be_bytes(), &fullbox(b"esds", 0, 0, esds)]);
            let sizes: Vec<u32> = self.audio.iter().map(|x| x.0).collect();
            let durs: Vec<u32> = self.audio.iter().map(|x| x.1).collect();
            let dur: u64 = durs.iter().map(|x| *x as u64).sum();
            let smhd = fullbox(b"smhd", 0, 0, &[0u8; 4]);
            traks.extend(Self::trak(2, b"soun", "SoundHandler", *ts, dur, movie_ms, 0, 0, smhd, Self::stbl(bx(b"mp4a", &entry), &sizes, &durs, None, data_start + video_bytes)));
        }
        let next_id = if self.audio.is_empty() { 2 } else { 3 };
        let mvhd = fullbox(b"mvhd", 0, 0, &cat(&[&be32s(&[0, 0, 1000, movie_ms, 0x10000]), &0x0100u16.to_be_bytes(), &[0u8; 10], &be32s(&MATRIX), &[0u8; 24], &(next_id as u32).to_be_bytes()]));
        (header, bx(b"moov", &cat(&[&mvhd, &traks])))
    }
}
