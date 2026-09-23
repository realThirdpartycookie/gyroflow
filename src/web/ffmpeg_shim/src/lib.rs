//! The subset of the `ffmpeg-next` API that Gyroflow's shared code (render queue, autosync, calibration, MDK frame
//! processor) touches, implemented without libav*. In the browser build frames are decoded by WebCodecs.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error { Other { errno: i32 }, Bug, InvalidData, Unsupported(String) }
impl std::fmt::Display for Error { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, "{self:?}") } }
impl std::error::Error for Error { }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rational(pub i32, pub i32);
impl Rational {
    pub fn new(num: i32, den: i32) -> Self { Self(num, den) }
    pub fn numerator(&self) -> i32 { self.0 }
    pub fn denominator(&self) -> i32 { self.1 }
}
impl From<Rational> for f64 { fn from(r: Rational) -> f64 { r.0 as f64 / r.1 as f64 } }

#[derive(Debug, Clone, Default)]
pub struct Dictionary<'a>(Vec<(String, String)>, std::marker::PhantomData<&'a ()>);
impl<'a> Dictionary<'a> {
    pub fn new() -> Self { Self(Vec::new(), Default::default()) }
    pub fn set(&mut self, k: &str, v: &str) { self.0.retain(|x| x.0 != k); self.0.push((k.into(), v.into())); }
    pub fn get(&self, k: &str) -> Option<&str> { self.0.iter().find(|x| x.0 == k).map(|x| x.1.as_str()) }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> { self.0.iter().map(|x| (x.0.as_str(), x.1.as_str())) }
}
impl<'a> IntoIterator for Dictionary<'a> {
    type Item = (String, String);
    type IntoIter = std::vec::IntoIter<(String, String)>;
    fn into_iter(self) -> Self::IntoIter { self.0.into_iter() }
}

pub mod format {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Pixel { #[default] None, RGBA, BGRA, GRAY8, YUV420P, NV12 }
    impl Pixel {
        pub fn bytes_per_pixel(&self) -> usize { match self { Pixel::RGBA | Pixel::BGRA => 4, _ => 1 } }
    }
}

pub mod ffi {
    #[repr(C)] pub struct AVBufferRef { _p: u8 }
    #[repr(C)]
    #[allow(non_camel_case_types)]
    pub struct AVFrame { pub buf: [*mut AVBufferRef; 8], pub data: [*mut u8; 8], pub linesize: [i32; 8] }
    static mut DUMMY: AVBufferRef = AVBufferRef { _p: 0 };
    /// Frames only borrow the caller's buffer here; nothing is refcounted.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn av_buffer_create(_data: *mut u8, _size: usize, _free: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *mut u8)>, _opaque: *mut std::ffi::c_void, _flags: i32) -> *mut AVBufferRef {
        std::ptr::addr_of_mut!(DUMMY)
    }
}

pub mod frame {
    use super::{ ffi, format::Pixel };
    /// Single packed plane (RGBA/BGRA/GRAY8), either owned or pointing at a caller's buffer via `as_mut_ptr()`.
    pub struct Video { format: Pixel, width: u32, height: u32, owned: Vec<u8>, raw: Box<ffi::AVFrame> }
    impl Video {
        pub fn empty() -> Self {
            Self { format: Pixel::None, width: 0, height: 0, owned: Vec::new(),
                   raw: Box::new(ffi::AVFrame { buf: [std::ptr::null_mut(); 8], data: [std::ptr::null_mut(); 8], linesize: [0; 8] }) }
        }
        pub fn new(format: Pixel, width: u32, height: u32) -> Self {
            let mut v = Self::empty();
            v.format = format; v.width = width; v.height = height;
            v.owned = vec![0u8; width as usize * height as usize * format.bytes_per_pixel()];
            v.raw.data[0] = v.owned.as_mut_ptr();
            v.raw.linesize[0] = (width as usize * format.bytes_per_pixel()) as i32;
            v
        }
        pub fn set_format(&mut self, f: Pixel) { self.format = f; }
        pub fn set_width(&mut self, w: u32) { self.width = w; }
        pub fn set_height(&mut self, h: u32) { self.height = h; }
        pub fn format(&self) -> Pixel { self.format }
        pub fn width(&self) -> u32 { self.width }
        pub fn height(&self) -> u32 { self.height }
        pub fn planes(&self) -> usize { 1 }
        pub fn plane_width(&self, _i: usize) -> u32 { self.width }
        pub fn plane_height(&self, _i: usize) -> u32 { self.height }
        pub fn stride(&self, _i: usize) -> usize { self.raw.linesize[0] as usize }
        pub fn data(&self, _i: usize) -> &[u8] {
            if self.raw.data[0].is_null() { return &[]; }
            unsafe { std::slice::from_raw_parts(self.raw.data[0], self.stride(0) * self.height as usize) }
        }
        pub fn data_mut(&mut self, _i: usize) -> &mut [u8] {
            if self.raw.data[0].is_null() { return &mut []; }
            unsafe { std::slice::from_raw_parts_mut(self.raw.data[0], self.stride(0) * self.height as usize) }
        }
        pub fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame { &mut *self.raw }
        pub fn as_ptr(&self) -> *const ffi::AVFrame { &*self.raw }
    }
}
