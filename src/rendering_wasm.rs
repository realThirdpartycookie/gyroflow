// emscripten: ffmpeg-next (decode/encode/export) is not available. Stubs so the UI can boot.
// ponytail: no export/render queue in the browser; replace with an ffmpeg.wasm/WebCodecs-backed rendering module.
pub fn init_log() { }
pub fn clear_log() { }
pub fn get_log() -> String { String::new() }
pub fn set_gpu_type_from_name(_name: &str) { }

pub mod render_queue {
    use qmetaobject::*;
    use std::sync::Arc;
    use gyroflow_core::StabilizationManager;

    #[derive(Default, QObject)]
    pub struct RenderQueue {
        base: qt_base_class!(trait QObject),
    }
    impl RenderQueue {
        pub fn new(_stabilizer: Arc<StabilizationManager>) -> Self { Self::default() }
    }
}
