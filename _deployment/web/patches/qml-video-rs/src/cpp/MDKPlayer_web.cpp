// emscripten replacement for MDKPlayer.cpp (MDK has no WebAssembly build).
// Playback goes through a hardware-decoded <video> element (gf_video_* in Gyroflow's library_gfweb.js). Each frame is
// copied GPU->GPU into the item's QRhiTexture, where Gyroflow's processTexture callback (Qt RHI undistort) runs,
// exactly like with MDK's renderVideo().
#include <emscripten.h>
#include <GLES3/gl3.h>
#include <cmath>
#include <QJsonObject>
#include <QFileInfo>
#include <thread>
#include <mutex>
#include <condition_variable>
#include <deque>

// One frame-processing session (autosync, thumbnails...): JS decodes with WebCodecs and pushes RGBA frames here,
// a worker thread hands them to Gyroflow's callback like MDK's decoder thread did.
struct WebProcSession {
    struct Frame { double ts_us; int w, h; uint8_t *data; int len; };
    std::mutex m;
    std::condition_variable cv;
    std::deque<Frame> q;
    int32_t backlog{0}; // read by JS for backpressure
    std::atomic<bool> stop{false};
};

extern "C" {
    int    gf_video_create(void *owner);
    void   gf_video_destroy(int h);
    void   gf_video_set_url(int h, const char *path);
    void   gf_video_play(int h);
    void   gf_video_pause(int h);
    void   gf_video_seek(int h, double ms, int exact);
    void   gf_video_set_range(int h, double from_ms, double to_ms);
    void   gf_video_set(int h, int what, double value);
    double gf_video_get(int h, int what);
    double gf_video_upload(int h, unsigned texture);
    bool   qvr_web_probe(const char *path, double *out); // lib.rs: [width, height, fps, duration_s, rotation]
    bool   qvr_web_open_decoder(void *id, const char *path, const uint64_t *ranges_ms, size_t n_ranges, int mode, uint32_t out_w, uint32_t out_h, const int32_t *backlog, double *info); // web.rs
    void   gf_dec_close(void *id);

    EMSCRIPTEN_KEEPALIVE void qvr_dec_frame(WebProcSession *s, double ts_us, int w, int h, uint8_t *data, int len) {
        { std::lock_guard<std::mutex> l(s->m); s->q.push_back({ ts_us, w, h, data, len }); s->backlog = int32_t(s->q.size()); }
        s->cv.notify_one();
    }

    EMSCRIPTEN_KEEPALIVE void qvr_web_event(MDKPlayer *p, int type, double value) { p->webEvent(type, value); }
}
enum { SetMuted = 0, SetVolume = 1, SetRate = 2 };
enum { GetMuted = 0, GetVolume, GetRate, GetTime, GetDuration, GetWidth, GetHeight, GetPaused, GetFrameWidth, GetFrameHeight };
enum { EvFrame = 1, EvMetadata, EvState, EvBuffering, EvError };

static mdk::Player s_noPlayer; // createTexture() wants one; its render API hookup is a no-op here

MDKPlayer::MDKPlayer() { }

void MDKPlayer::initPlayer() {
    m_web = gf_video_create(this);
    m_metadata = QJsonObject();
    m_shuttingDown = false;
    if (m_item && m_node && m_window) {
        setupPlayer();
        if (m_size.width() > 0 && m_size.height() > 0) m_syncNext = true;
    }
}

void MDKPlayer::destroyPlayer() {
    m_shuttingDown = true;
    m_videoLoaded = false;
    m_firstFrameLoaded = false;
    if (m_connectionBeforeRendering) QObject::disconnect(m_connectionBeforeRendering);
    if (m_connectionScreenChanged) QObject::disconnect(m_connectionScreenChanged);
    if (m_web) { gf_video_destroy(m_web); m_web = 0; }
}

MDKPlayer::~MDKPlayer() {
    m_shuttingDown = true;
    m_processPixels = nullptr;
    m_processTexture = nullptr;
    m_readyForProcessing = nullptr;
    if (m_userDataDestructor && m_userData) { m_userDataDestructor(m_userData); m_userData = nullptr; }
    if (m_userData2Destructor && m_userData2) { m_userData2Destructor(m_userData2); m_userData2 = nullptr; }
    destroyPlayer();
    m_item = nullptr;
    m_window = nullptr;
}

void MDKPlayer::setProperty(const QString &, const QString &) { }                          // MDK tuning knobs
void MDKPlayer::setDefaultProperty(const QString &k, const QString &v) { m_defaultProperties.insert(k, v); }

void MDKPlayer::setUrl(const QUrl &url, const QString &customDecoder) {
    m_overrideFps = 0.0;
    if (!m_item || !m_window || !m_node) {
        m_pendingUrl = url;
        m_pendingCustomDecoder = customDecoder;
        return;
    }
    destroyPlayer();
    initPlayer();
    m_webPath = url.isLocalFile() ? url.toLocalFile() : url.toString();
    qDebug2("setUrl") << "web player:" << m_webPath;
    gf_video_set_url(m_web, qUtf8Printable(m_webPath));
}

void MDKPlayer::setBackgroundColor(const QColor &color) { m_bgColor = color; forceRedraw(); }
void MDKPlayer::setMuted(bool v) { if (m_web) gf_video_set(m_web, SetMuted, v); }
bool MDKPlayer::getMuted() { return m_web && gf_video_get(m_web, GetMuted) != 0; }
void MDKPlayer::setVolume(float v) { if (m_web) gf_video_set(m_web, SetVolume, v); }
float MDKPlayer::getVolume() { return m_web ? gf_video_get(m_web, GetVolume) : 0.0f; }

void MDKPlayer::setupNode(QSGImageNode *node, QQuickItem *item) {
    m_node = node;
    m_item = item;
    m_window = item ? item->window() : nullptr;
    if (!m_window) return;
    node->setOwnsTexture(true);
    if (!m_pendingUrl.isEmpty()) {
        setUrl(m_pendingUrl, m_pendingCustomDecoder);
        m_pendingUrl = QUrl();
    } else if (!m_web) {
        initPlayer();
    }
}

void MDKPlayer::setProcessPixelsCallback(ProcessPixelsCb &&cb) { m_processPixels = cb; }
void MDKPlayer::setProcessTextureCallback(ProcessTextureCb &&cb) { m_processTexture = cb; }
void MDKPlayer::setReadyForProcessingCallback(ReadyForProcessingCb &&cb) { m_readyForProcessing = cb; }

void MDKPlayer::setupPlayer() {
    gf_video_set(m_web, SetRate, m_playbackRate);
    if (m_size.width() > 0 && m_size.height() > 0) m_syncNext = true;
    forceRedraw();
}

void MDKPlayer::webEvent(int type, double value) {
    if (m_shuttingDown.load() || !m_item) return;
    switch (type) {
        case EvFrame: m_item->update(); break;
        case EvMetadata: {
            double md[5] = { 0 };
            const bool probed = qvr_web_probe(qUtf8Printable(m_webPath), md);
            const uint w = probed ? uint(md[0]) : uint(gf_video_get(m_web, GetWidth));
            const uint h = probed ? uint(md[1]) : uint(gf_video_get(m_web, GetHeight));
            m_fps = probed && md[2] > 0 ? md[2] : 30.0; // ponytail: the browser doesn't expose fps; unparseable containers assume 30
            m_duration = gf_video_get(m_web, GetDuration) * 1000.0;
            m_webRotation = probed ? (int(std::lround(md[4])) % 360 + 360) % 360 : 0;
            double fps = m_fps;
            if (m_overrideFps > 0.0) { m_duration *= m_fps / m_overrideFps; fps = m_overrideFps; }
            const qlonglong frames = std::llround(m_duration / 1000.0 * fps);

            QJsonObject obj;
            const QString v = "stream.video[0].";
            obj.insert("format", QFileInfo(m_webPath).suffix().toLower());
            obj.insert(v + "rotation", QString::number(m_webRotation));
            obj.insert(v + "duration", QString::number(m_duration));
            obj.insert(v + "frames", QString::number(frames));
            obj.insert(v + "codec.width", QString::number(w));
            obj.insert(v + "codec.height", QString::number(h));
            obj.insert(v + "codec.frame_rate", QString::number(fps, 'f', 6));
            m_metadata = obj;

            m_videoLoaded = true;
            m_firstFrameLoaded = true;
            QMetaObject::invokeMethod(m_item, "videoLoaded", Q_ARG(double, m_duration), Q_ARG(qlonglong, frames), Q_ARG(double, fps), Q_ARG(uint, w), Q_ARG(uint, h));
            QMetaObject::invokeMethod(m_item, "metadataLoaded", Qt::QueuedConnection, Q_ARG(QJsonObject, m_metadata));
            if (!m_connectionBeforeRendering)
                m_connectionBeforeRendering = QObject::connect(m_window, &QQuickWindow::beforeRendering, [this] { this->windowBeforeRendering(); });
            forceRedraw();
            m_item->update();
        } break;
        case EvState: QMetaObject::invokeMethod(m_item, "stateChanged", Q_ARG(int, int(value))); break; // 1 playing, 2 paused
        case EvBuffering: QMetaObject::invokeMethod(m_item, "setBuffering", Q_ARG(bool, value != 0)); break;
        case EvError:
            qDebug2("webEvent") << "the browser can't decode" << m_webPath;
            QMetaObject::invokeMethod(m_item, "videoLoaded", Q_ARG(double, 0), Q_ARG(qlonglong, 0), Q_ARG(double, 0), Q_ARG(uint, 0), Q_ARG(uint, 0));
            QMetaObject::invokeMethod(m_item, "metadataLoaded", Q_ARG(QJsonObject, QJsonObject()));
            break;
    }
}

// Copies the uploaded frame (m_webTex, already sensor-oriented) into the item's render target, aspect-fit like MDK.
void MDKPlayer::webBlit() {
    const GLint vw = GLint(gf_video_get(m_web, GetFrameWidth)), vh = GLint(gf_video_get(m_web, GetFrameHeight));
    if (vw <= 0 || vh <= 0) return;
    if (!m_webFbo) glGenFramebuffers(1, &m_webFbo);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, m_webFbo);
    glFramebufferTexture2D(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, m_webTex, 0);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, static_cast<QGles2TextureRenderTarget *>(m_rt.get())->framebuffer);
    glDisable(GL_SCISSOR_TEST);
    const QColor bg = m_bgColor.isValid() ? m_bgColor : QColor(Qt::black);
    glClearColor(bg.redF(), bg.greenF(), bg.blueF(), bg.alphaF());
    glClear(GL_COLOR_BUFFER_BIT);

    const int W = m_size.width(), H = m_size.height();
    const double s = std::min(double(W) / vw, double(H) / vh);
    const int dw = int(std::lround(vw * s)), dh = int(std::lround(vh * s)), x0 = (W - dw) / 2, y0 = (H - dh) / 2;
    // Texture row 0 is the image top but GL framebuffers are bottom-up (the scene graph mirrors them back): flip Y.
    glBlitFramebuffer(0, 0, vw, vh, x0, y0 + dh, x0 + dw, y0, GL_COLOR_BUFFER_BIT, GL_LINEAR);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
}

void MDKPlayer::windowBeforeRendering() {
    if (m_shuttingDown.load() || !m_item || !m_window || !m_videoLoaded.load() || !m_web) return;
    if (m_syncNext || !m_rt) return;
    if (m_renderedPosition == m_playerPosition && m_renderedReturnCount++ > 100) return;
    if (m_readyForProcessing && !m_readyForProcessing(m_item)) return;

    auto context = rhiContext();
    auto cb = context->currentFrameCommandBuffer();
    if (m_rtNeedsClear) clearTexture();

    cb->beginPass(m_rt.get(), QColor(Qt::black), { 1.0f, 0 }, context->rhi()->nextResourceUpdateBatch(), QRhiCommandBuffer::ExternalContent);
    cb->beginExternal();
    if (!m_webTex) glGenTextures(1, &m_webTex);
    double timestamp = gf_video_upload(m_web, m_webTex);
    if (timestamp >= 0) webBlit();
    cb->endExternal();
    cb->endPass();
    if (timestamp < 0) return;

    m_playerPosition = timestamp * 1000;

    double fps = m_fps;
    if (m_overrideFps > 0.0) {
        timestamp *= m_fps / m_overrideFps;
        fps = m_overrideFps;
    }
    int frame = std::ceil(std::round(timestamp * fps * 100.0) / 100.0);

    bool processed = false;
    if (m_firstFrameLoaded.load()) {
        if (m_processTexture && m_texture) {
            QSGRendererInterface *rif = m_window->rendererInterface();
            const uint64_t ptr1 = m_texture->nativeTexture().object;
            const uint64_t ptr2 = uint64_t(uintptr_t(rif->getResource(m_window, QSGRendererInterface::OpenGLContextResource)));
            processed = m_processTexture(m_item, frame, timestamp * 1000.0, m_size.width(), m_size.height(), 1 /* OpenGL */, ptr1, ptr2, 0, 0, 0);
            if (processed) m_renderFailCounter = 0;
            else           m_renderFailCounter++;
        }
        if (!processed && m_processPixels && (!m_processTexture || m_renderFailCounter > 10)) {
            auto img = toImage();
            const auto img2 = m_processPixels(m_item, frame, timestamp * 1000.0, img);
            if (!img2.isNull() && img2.constBits()) fromImage(img2);
        }
    }

    if (m_renderedPosition != m_playerPosition) m_renderedReturnCount = 0;
    m_renderedPosition = m_playerPosition;

    QMetaObject::invokeMethod(m_item, "frameRendered", Q_ARG(double, timestamp * 1000.0), Q_ARG(int, frame));
}

void MDKPlayer::sync(QSGImageNode *node, QSize newSize, QQuickItem *item, bool force) {
    if (m_shuttingDown.load()) return;
    if (!m_item || !m_window || !item || m_item != item || !node) return;
    if (m_syncNext) { force = true; m_syncNext = false; }
    if (!m_web) { m_size = newSize; return; }
    if (!force && node->texture() && newSize == m_size) return;
    if (newSize.width() < 32 || newSize.height() < 32) newSize = QSize(32, 32);
    m_size = newSize;

    releaseResources();
    auto tex = createTexture(&s_noPlayer, m_size);
    if (!tex) return;
    QMetaObject::invokeMethod(m_item, "surfaceSizeUpdated", Q_ARG(uint, m_size.width()), Q_ARG(uint, m_size.height()));
    node->setTexture(tex);
    node->setOwnsTexture(true);
    node->setTextureCoordinatesTransform(m_tx);
    node->setFiltering(QSGTexture::Linear);
    node->setRect(0, 0, m_item->width(), m_item->height());
}

void MDKPlayer::play()  { if (m_videoLoaded && m_web) { gf_video_play(m_web);  forceRedraw(); } }
void MDKPlayer::pause() { if (m_videoLoaded && m_web) { gf_video_pause(m_web); forceRedraw(); } }
void MDKPlayer::stop()  { if (m_videoLoaded && m_web) { gf_video_pause(m_web); gf_video_seek(m_web, 0, 1); } }
void MDKPlayer::setFrameRate(float fps) { m_overrideFps = fps; }

// +0.1 ms so a seek to a frame's exact start never lands on the previous frame through float rounding
void MDKPlayer::seekToTimestamp(float timestampMs, bool exact) {
    if (!m_videoLoaded || !m_web) return;
    gf_video_seek(m_web, timestampMs + (exact ? 0.1 : 0.0), exact);
    forceRedraw();
}
void MDKPlayer::seekToFrameDelta(int64_t frameDelta) {
    if (!m_videoLoaded || !m_web || m_fps <= 0) return;
    const double frame = std::round(gf_video_get(m_web, GetTime) * m_fps) + frameDelta;
    gf_video_seek(m_web, std::max(0.0, frame) / m_fps * 1000.0 + 0.1, 1);
    forceRedraw();
}
void MDKPlayer::seekToFrame(int64_t frame, int64_t, bool exact) {
    if (m_fps > 0) seekToTimestamp((frame / m_fps) * 1000.0, exact);
}

void MDKPlayer::setPlaybackRate(float rate) { m_playbackRate = rate; if (m_web) gf_video_set(m_web, SetRate, rate); }
float MDKPlayer::playbackRate() { return m_playbackRate; }

void MDKPlayer::setPlaybackRange(int64_t from_ms, int64_t to_ms) {
    if (m_overrideFps > 0.0) {
        from_ms /= m_fps / m_overrideFps;
        to_ms   /= m_fps / m_overrideFps;
    }
    if (m_web) gf_video_set_range(m_web, from_ms, to_ms);
}

void MDKPlayer::setRotation(int) { } // no QML caller sets it
int MDKPlayer::getRotation() { return m_webRotation; }

// Frames for autosync/thumbnails: WebCodecs (hardware) decode, scaled to width x height on the GPU, then read back
// as RGBA. width == 0 with height > 0 keeps the aspect ratio; both 0 = full size.
void MDKPlayer::initProcessingPlayer(uint64_t id, uint64_t width, uint64_t height, bool, std::string, const std::vector<std::pair<uint64_t, uint64_t>> &ranges, VideoProcessCb &&cb) {
    const QString path = !m_webPath.isEmpty() ? m_webPath : (m_pendingUrl.isLocalFile() ? m_pendingUrl.toLocalFile() : m_pendingUrl.toString());
    auto s = new WebProcSession(); // ponytail: never freed, JS may still deliver a frame after close; a few bytes per session
    stopProcessingPlayer(id);
    m_webProc[id] = s;
    std::vector<uint64_t> r;
    for (const auto &x : ranges) { r.push_back(x.first); r.push_back(x.second); }
    double info[7] = { 0 }; // fps, coded w, coded h, duration_ms, frames, out w, out h
    if (!qvr_web_open_decoder(s, qUtf8Printable(path), r.data(), ranges.size(), 0, width, height, &s->backlog, info)) {
        cb(-1, -1.0, 0, 0, 0, 0, 0, 0, 0, nullptr, 0);
        return;
    }
    std::thread([s, cb, info0 = info[0], cw = uint32_t(info[1]), ch = uint32_t(info[2]), dur = info[3], frames = uint32_t(info[4])] {
        for (;;) {
            WebProcSession::Frame f;
            {
                std::unique_lock<std::mutex> l(s->m);
                s->cv.wait(l, [s] { return !s->q.empty() || s->stop.load(); });
                if (s->q.empty()) break;
                f = s->q.front();
                s->q.pop_front();
                s->backlog = int32_t(s->q.size());
            }
            if (f.ts_us < 0) break; // end of stream
            const double ts_ms = f.ts_us / 1000.0;
            const int frame = int(std::ceil(std::round(ts_ms / 1000.0 * info0 * 100.0) / 100.0));
            const bool more = !s->stop.load() && cb(frame, ts_ms, f.w, f.h, cw, ch, info0, dur, frames, f.data, f.len);
            free(f.data);
            if (!more) { s->stop = true; gf_dec_close(s); break; }
        }
        cb(-1, -1.0, 0, 0, 0, 0, 0, 0, 0, nullptr, 0);
    }).detach();
}
void MDKPlayer::stopProcessingPlayer(uint64_t id) {
    auto it = m_webProc.find(id);
    if (it == m_webProc.end()) return;
    auto s = static_cast<WebProcSession *>(it->second);
    s->stop = true;
    s->cv.notify_all();
    gf_dec_close(s);
    m_webProc.erase(it);
}
std::map<std::string, std::string> MDKPlayer::getMediaInfo(const MediaInfo &) { return {}; }

QSGDefaultRenderContext *MDKPlayer::rhiContext() { return static_cast<QSGDefaultRenderContext *>(QQuickItemPrivate::get(m_item)->sceneGraphRenderContext()); }
QRhiTexture *MDKPlayer::rhiTexture() { return m_texture; }
QRhiTextureRenderTarget *MDKPlayer::rhiRenderTarget() { return m_rt.get(); }
QRhiRenderPassDescriptor *MDKPlayer::rhiRenderPassDescriptor() { return m_rtRp.get(); }
QQuickWindow *MDKPlayer::qmlWindow() { return m_window; }
QQuickItem *MDKPlayer::qmlItem() { return m_item; }
QSize MDKPlayer::textureSize() { return m_size; }
QMatrix4x4 MDKPlayer::textureMatrix() { return m_proj; }
void *MDKPlayer::userData() const { return m_userData; }
void MDKPlayer::setUserData(void *ptr) { m_userData = ptr; }
void MDKPlayer::setUserDataDestructor(std::function<void(void *)> &&cb) { m_userDataDestructor = cb; }
void *MDKPlayer::userData2() const { return m_userData2; }
void MDKPlayer::setUserData2(void *ptr) { m_userData2 = ptr; }
void MDKPlayer::setUserData2Destructor(std::function<void(void *)> &&cb) { m_userData2Destructor = cb; }
