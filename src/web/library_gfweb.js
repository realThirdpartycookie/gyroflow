/**
 * Browser glue for the Gyroflow Qt/WebAssembly build (linked with --js-library).
 *
 *  - /web/<n>/<name>: read-only mount of files picked in the browser. Reads are served lazily from the File by a
 *    worker (FileReaderSync), so multi-GB videos never get copied into memory. std::fs works on these paths.
 *  - gf_web_pick_files(): browser file picker, results go back to Rust via gf_web_files_picked().
 *  - gf_video_*(): hardware-decoded <video> element used by the MDKPlayer replacement (qml-video-rs), with frames
 *    uploaded straight into a WebGL2 texture.
 */
addToLibrary({
  $GFWEB__deps: ['$FS'],
  $GFWEB: {
    DIR_MODE: {{{ cDefs.S_IFDIR }}} | 365 /* 0555 */,
    FILE_MODE: {{{ cDefs.S_IFREG }}} | 292 /* 0444 */,
    CHUNK: 4 << 20,
    root: null,
    dirs: 0,
    files: [null],
    players: [null],
    decoders: {},
    exports: {},
    gpuDevice() {
      return GFWEB.device ??= navigator.gpu.requestAdapter({ powerPreference: 'high-performance' }).then((a) => a.requestDevice({ requiredLimits: { maxStorageBufferBindingSize: a.limits.maxStorageBufferBindingSize } }));
    },
    // gyroflow's wgpu_undistort.wgsl in its texture-input variant (bindings as in src/core/gpu/wgpu.rs)
    undistort(d, code, coeffs) {
      var module = d.createShaderModule({ code });
      var ro = { type: 'read-only-storage' }, F = GPUShaderStage.FRAGMENT;
      var layout = d.createBindGroupLayout({ entries: [
        { binding: 0, visibility: F, buffer: { type: 'uniform' } }, { binding: 1, visibility: F, buffer: ro }, { binding: 2, visibility: F, buffer: ro },
        { binding: 3, visibility: F, buffer: ro }, { binding: 4, visibility: F, buffer: ro }, { binding: 5, visibility: F, texture: { sampleType: 'float' } },
      ] });
      var buf = (size, usage) => d.createBuffer({ size: Math.max(16, Math.ceil(size / 16) * 16), usage: usage | GPUBufferUsage.COPY_DST });
      var b = { coeffs: buf(coeffs.byteLength, GPUBufferUsage.STORAGE), drawing: buf(16, GPUBufferUsage.STORAGE) };
      d.queue.writeBuffer(b.coeffs, 0, coeffs);
      var pipelines = {}, tex = null, bind = null;
      var ensure = (k, size, usage) => { if (!b[k] || b[k].size < size) { b[k]?.destroy(); b[k] = buf(size, usage); bind = null; } };
      return {
        module,
        render(source, w, h, params, matrices, mesh, view) {
          var pi = new Int32Array(params.buffer, params.byteOffset, params.byteLength >> 2);
          pi[9] &= ~8; // no overlay drawing buffer
          if (!tex || tex.width != w || tex.height != h) {
            tex?.destroy();
            tex = d.createTexture({ size: [w, h], format: 'rgba8unorm', usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST | GPUTextureUsage.RENDER_ATTACHMENT });
            bind = null;
          }
          ensure('params', params.byteLength, GPUBufferUsage.UNIFORM);
          ensure('matrices', matrices.byteLength, GPUBufferUsage.STORAGE);
          ensure('mesh', Math.max(4096, mesh.byteLength), GPUBufferUsage.STORAGE);
          d.queue.writeBuffer(b.params, 0, params);
          d.queue.writeBuffer(b.matrices, 0, matrices);
          if (mesh.byteLength) d.queue.writeBuffer(b.mesh, 0, mesh);
          bind ??= d.createBindGroup({ layout, entries: [
            ...['params', 'matrices', 'coeffs', 'mesh', 'drawing'].map((k, i) => ({ binding: i, resource: { buffer: b[k] } })), { binding: 5, resource: tex.createView() },
          ] });
          d.queue.copyExternalImageToTexture({ source }, { texture: tex }, [w, h]);
          var key = pi[7] + ':' + pi[9];
          pipelines[key] ??= d.createRenderPipeline({
            layout: d.createPipelineLayout({ bindGroupLayouts: [layout] }),
            vertex: { module, entryPoint: 'undistort_vertex' },
            fragment: { module, entryPoint: 'undistort_fragment', targets: [{ format: 'rgba8unorm' }], constants: { 100: pi[7], 101: 4, 102: 4, 103: pi[9] } },
            primitive: { topology: 'triangle-strip' },
          });
          var enc = d.createCommandEncoder();
          var pass = enc.beginRenderPass({ colorAttachments: [{ view, loadOp: 'clear', storeOp: 'store', clearValue: [0, 0, 0, 1] }] });
          pass.setPipeline(pipelines[key]);
          pass.setBindGroup(0, bind);
          pass.draw(6);
          pass.end();
          d.queue.submit([enc.finish()]);
        },
      };
    },
    encode(e, f) {
      e.encoder.encode(f, { keyFrame: e.n++ % e.cfg.keyint == 0 });
      f.close();
    },
    reader: null,

    // ---- /web mount (legacy FS runs on the browser main thread; pthread syscalls are proxied there) ----
    mount(mount) {
      return GFWEB.createNode(null, '/', GFWEB.DIR_MODE, null);
    },
    createNode(parent, name, mode, file) {
      var node = FS.createNode(parent, name, mode);
      node.mode = mode;
      node.node_ops = GFWEB.node_ops;
      node.stream_ops = GFWEB.stream_ops;
      node.timestamp = file ? file.lastModified : Date.now();
      if (file) {
        node.size = file.size;
        node.fileId = GFWEB.files.push(file) - 1;
        GFWEB.reader.postMessage({ id: node.fileId, file });
      } else {
        node.size = 4096;
        node.contents = {};
      }
      if (parent) parent.contents[name] = node;
      return node;
    },
    // One directory per pick, so sidecar files picked together (e.g. clip.mp4 + clip.gcsv) sit next to each other
    addFiles(list) {
      GFWEB.startReader();
      if (!GFWEB.root) {
        FS.mkdir('/web');
        GFWEB.root = FS.mount(GFWEB, {}, '/web').mount.root;
      }
      var n = ++GFWEB.dirs;
      var dir = GFWEB.createNode(GFWEB.root, String(n), GFWEB.DIR_MODE, null);
      return Array.from(list).map(f => { GFWEB.createNode(dir, f.name, GFWEB.FILE_MODE, f); return '/web/' + n + '/' + f.name; });
    },
    fileByPath(path) {
      try { return GFWEB.files[FS.lookupPath(path).node.fileId] || null; } catch (e) { return null; }
    },
    node_ops: {
      getattr(node) {
        var t = new Date(node.timestamp);
        return { dev: 1, ino: node.id, mode: node.mode, nlink: 1, uid: 0, gid: 0, rdev: 0, size: node.size,
                 atime: t, mtime: t, ctime: t, blksize: 4096, blocks: Math.ceil(node.size / 4096) };
      },
      setattr(node, attr) { if (attr.timestamp !== undefined) node.timestamp = attr.timestamp; },
      lookup(parent, name) { throw new FS.ErrnoError({{{ cDefs.ENOENT }}}); },
      mknod(parent, name, mode, dev) { throw new FS.ErrnoError({{{ cDefs.EPERM }}}); },
      rename(oldNode, newDir, newName) { throw new FS.ErrnoError({{{ cDefs.EPERM }}}); },
      unlink(parent, name) { throw new FS.ErrnoError({{{ cDefs.EPERM }}}); },
      rmdir(parent, name) { throw new FS.ErrnoError({{{ cDefs.EPERM }}}); },
      readdir(node) { return ['.', '..'].concat(Object.keys(node.contents)); },
      symlink(parent, newName, oldPath) { throw new FS.ErrnoError({{{ cDefs.EPERM }}}); },
    },
    stream_ops: {
      read(stream, buffer, offset, length, position) {
        var size = stream.node.size, done = 0;
        while (done < length && position + done < size) {
          var n = GFWEB.readChunk(stream.node.fileId, position + done, Math.min(length - done, size - position - done, GFWEB.CHUNK));
          buffer.set(GFWEB.stage.subarray(0, n), offset + done);
          done += n;
          if (!n) break;
        }
        return done;
      },
      write(stream, buffer, offset, length, position) { throw new FS.ErrnoError({{{ cDefs.EIO }}}); },
      llseek(stream, offset, whence) {
        var position = offset;
        if (whence === {{{ cDefs.SEEK_CUR }}}) position += stream.position;
        else if (whence === {{{ cDefs.SEEK_END }}}) position += stream.node.size;
        if (position < 0) throw new FS.ErrnoError({{{ cDefs.EINVAL }}});
        return position;
      },
    },

    // ---- synchronous reads through a worker (only workers have FileReaderSync) ----
    // ctrl: [state 0 idle | 1 request | 2 done, fileId, length, result], pos: f64 byte offset, stage: data
    startReader() {
      if (GFWEB.reader) return;
      var src = `
        const fr = new FileReaderSync(), files = {};
        let ctrl, pos, stage;
        onmessage = (e) => {
          const d = e.data;
          if (d.sab) { ctrl = new Int32Array(d.sab, 0, 4); pos = new Float64Array(d.sab, 16, 1); stage = new Uint8Array(d.stage); loop(); }
          else files[d.id] = d.file;
        };
        async function loop() {
          for (;;) {
            const s = Atomics.load(ctrl, 0);
            if (s !== 1) { const w = Atomics.waitAsync(ctrl, 0, s); if (w.async) await w.value; continue; }
            try {
              const b = fr.readAsArrayBuffer(files[ctrl[1]].slice(pos[0], pos[0] + ctrl[2]));
              stage.set(new Uint8Array(b));
              ctrl[3] = b.byteLength;
            } catch (e) { console.error('gfweb read failed', e); ctrl[3] = -1; }
            Atomics.store(ctrl, 0, 2);
            Atomics.notify(ctrl, 0);
          }
        }`;
      var sab = new SharedArrayBuffer(24), stage = new SharedArrayBuffer(GFWEB.CHUNK);
      GFWEB.ctrl = new Int32Array(sab, 0, 4);
      GFWEB.pos = new Float64Array(sab, 16, 1);
      GFWEB.stage = new Uint8Array(stage);
      GFWEB.reader = new Worker(URL.createObjectURL(new Blob([src], { type: 'text/javascript' })));
      GFWEB.reader.postMessage({ sab, stage });
    },
    readChunk(id, position, length) {
      var c = GFWEB.ctrl;
      c[1] = id; c[2] = length; GFWEB.pos[0] = position;
      Atomics.store(c, 0, 1);
      Atomics.notify(c, 0);
      // ponytail: the browser main thread can't Atomics.wait, so spin; a chunk is ≤ 4 MB, i.e. ~1 ms
      while (Atomics.load(c, 0) !== 2) { }
      var n = c[3];
      Atomics.store(c, 0, 0);
      Atomics.notify(c, 0);
      if (n < 0) throw new FS.ErrnoError({{{ cDefs.EIO }}});
      return n;
    },
  },

  // ---- file picker ----
  gf_web_pick_files__deps: ['$GFWEB', '$UTF8ToString', '$stringToNewUTF8', 'gf_web_files_picked'],
  // Automation/test hook: register File objects exactly like the picker does, returns their /web paths
  // Drops are taken before Qt sees them (Qt would copy the whole file into memory) and opened like picked files.
  gf_web_pick_files__postset: `globalThis.gfWebAddFiles = (files) => GFWEB.addFiles(files);
    globalThis.gfHeapMB = () => Math.round(HEAPU8.length / 1048576);
    if (typeof window != 'undefined' && !ENVIRONMENT_IS_PTHREAD) {
      addEventListener('dragover', (e) => { if (e.dataTransfer?.types.includes('Files')) { e.preventDefault(); e.stopImmediatePropagation(); } }, true);
      addEventListener('drop', (e) => {
        if (!e.dataTransfer?.files.length) return;
        e.preventDefault(); e.stopImmediatePropagation();
        _gf_web_files_picked(-1, stringToNewUTF8(JSON.stringify(GFWEB.addFiles(e.dataTransfer.files))));
      }, true);
    }`,
  gf_web_pick_files: (accept, multiple, cbId) => {
    var input = document.createElement('input');
    input.type = 'file';
    input.multiple = !!multiple;
    var a = UTF8ToString(accept);
    if (a) input.accept = a;
    var done = (paths) => { input.onchange = input.oncancel = null; _gf_web_files_picked(cbId, stringToNewUTF8(JSON.stringify(paths))); };
    input.onchange = () => done(GFWEB.addFiles(input.files));
    input.oncancel = () => done([]);
    input.click();
  },

  // ---- WebCodecs decode sessions (frame processing: autosync/thumbnails -> CPU; export -> GL texture) ----
  // samples: f64[n*4] = offset, size, pts_us, key (decode order); ranges: f64[n*2] us (empty = everything).
  // mode 0: frames are scaled on the GPU (2D canvas) to outW x outH RGBA and handed to qvr_dec_frame(id, ...) until
  //         qvr_dec_frame(id, -1); `backlog` (int32 in wasm memory) is the consumer's queue length, used for backpressure.
  // mode 1: frames wait in a queue for gf_dec_upload() (export, GPU only).
  gf_dec_open__deps: ['$GFWEB', '$UTF8ToString', 'malloc', 'qvr_dec_frame'],
  gf_dec_open__proxy: 'sync',
  gf_dec_open: (id, pathPtr, codecPtr, descPtr, descLen, codedW, codedH, samplesPtr, nSamples, rangesPtr, nRanges, mode, outW, outH, backlogPtr) => {
    var file = GFWEB.fileByPath(UTF8ToString(pathPtr));
    var samples = HEAPF64.slice(samplesPtr >> 3, (samplesPtr >> 3) + nSamples * 4);
    var ranges = [];
    for (var i = 0; i < nRanges; i++) ranges.push([HEAPF64[(rangesPtr >> 3) + i * 2], HEAPF64[(rangesPtr >> 3) + i * 2 + 1]]);
    if (!ranges.length) ranges.push([-Infinity, Infinity]);
    var s = { id, mode, outW, outH, backlogPtr, frames: [], done: false, closed: false, error: null, wake: null };
    GFWEB.decoders[id] = s;
    var kick = () => { var w = s.wake; s.wake = null; w?.(); };
    var deliver = (ts, w, h, ptr, len) => _qvr_dec_frame(id, ts, w, h, ptr, len);
    var inRange = (t) => ranges.some(([a, b]) => t >= a && t <= b);
    var canvas = mode == 0 ? new OffscreenCanvas(outW || codedW, outH || codedH) : null;
    var ctx = canvas?.getContext('2d', { willReadFrequently: true });
    s.decoder = new VideoDecoder({
      output: (f) => {
        if (s.closed || !inRange(f.timestamp)) { f.close(); kick(); return; }
        if (mode == 1) { s.frames.push(f); var c = s.consumer; s.consumer = null; c?.(); return; }
        ctx.drawImage(f, 0, 0, canvas.width, canvas.height);
        f.close();
        var img = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
        var ptr = _malloc(img.length);
        HEAPU8.set(img, ptr);
        deliver(f.timestamp, canvas.width, canvas.height, ptr, img.length);
        kick();
      },
      error: (e) => { s.error = e; console.error('gfweb decoder:', e); kick(); },
    });
    s.decoder.configure({ codec: UTF8ToString(codecPtr), description: HEAPU8.slice(descPtr, descPtr + descLen), codedWidth: codedW, codedHeight: codedH, hardwareAcceleration: 'prefer-hardware' });

    // Samples to feed: for each range, from the last keyframe at or before its start until past its end
    var n = nSamples, plan = [];
    for (var [a, b] of ranges) {
      var start = 0;
      for (var i = 0; i < n; i++) if (samples[i * 4 + 3] && samples[i * 4 + 2] <= a) start = i;
      for (var i = start; i < n; i++) {
        if (samples[i * 4 + 3] && samples[i * 4 + 2] > b) break; // next GOP starts after the range
        plan.push(i);
      }
    }
    console.log(`gfweb: decode ${plan.length}/${n} samples, ranges ${JSON.stringify(ranges.map((r) => r.map((x) => isFinite(x) ? Math.round(x / 1000) : x)))} ms, mode ${mode}, ${outW}x${outH}`);
    var busy = () => s.decoder.decodeQueueSize > 4 || (mode == 1 ? s.frames.length > 6 : HEAP32[backlogPtr >> 2] > 6);
    (async () => {
      try {
        for (var i of plan) {
          while (!s.closed && !s.error && busy()) await new Promise((r) => { s.wake = r; setTimeout(r, 20); });
          if (s.closed || s.error) break;
          var off = samples[i * 4], size = samples[i * 4 + 1];
          var data = new Uint8Array(await file.slice(off, off + size).arrayBuffer());
          s.decoder.decode(new EncodedVideoChunk({ type: samples[i * 4 + 3] ? 'key' : 'delta', timestamp: samples[i * 4 + 2], data }));
        }
        if (!s.closed && !s.error) await s.decoder.flush();
      } catch (e) { s.error = s.error || e; console.error('gfweb decode:', e); }
      s.done = true;
      console.log(`gfweb: decode session done (${s.error ? 'error: ' + s.error : 'ok'})`);
      var c = s.consumer; s.consumer = null; c?.();
      if (mode == 0 && !s.closed) deliver(-1, 0, 0, 0, 0);
      kick();
    })();
  },
  gf_dec_close__deps: ['$GFWEB'],
  gf_dec_close__proxy: 'async',
  gf_dec_close: (id) => {
    var s = GFWEB.decoders[id];
    if (!s) return;
    s.closed = true;
    s.frames.forEach((f) => f.close());
    s.frames = [];
    if (s.decoder.state != 'closed') s.decoder.close();
    delete GFWEB.decoders[id];
  },
  // mode 1: upload the next decoded frame into GL texture `tex`. Returns its pts in us, -1 if none is ready yet,
  // -2 when the stream is finished, -3 on a decoder error.
  gf_dec_upload__deps: ['$GFWEB', '$GL'],
  gf_dec_upload: (id, tex, flipY) => {
    var s = GFWEB.decoders[id];
    if (!s) return -2;
    if (s.error) return -3;
    var f = s.frames.shift();
    if (!f) return s.done ? -2 : -1;
    var gl = GLctx;
    gl.bindTexture(gl.TEXTURE_2D, GL.textures[tex]);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, !!flipY);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.NONE);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, f);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.BROWSER_DEFAULT_WEBGL);
    gl.bindTexture(gl.TEXTURE_2D, null);
    var ts = f.timestamp;
    f.close();
    s.wake?.();
    return ts;
  },

  // Presentation time (us) of the next decoded frame without consuming it; -1 none yet, -2 finished, -3 error
  gf_dec_peek__deps: ['$GFWEB'],
  gf_dec_peek: (id) => {
    var s = GFWEB.decoders[id];
    if (!s) return -2;
    if (s.error) return -3;
    return s.frames.length ? s.frames[0].timestamp : (s.done ? -2 : -1);
  },

  // ---- export: WebCodecs encoder + MP4 download sink (the muxer itself is Rust, web_export.rs) ----
  // cfg: { family: 'avc'|'hevc', width, height, bitrate, fps, keyint }. Encoded chunks go to the job's blob parts,
  // their sizes/flags to gf_export_chunk() so Rust can write the sample tables.
  gf_export_start__deps: ['$GFWEB', '$UTF8ToString', '$stringToNewUTF8', 'malloc', 'gf_export_chunk', 'gf_export_config', 'gf_export_error'],
  gf_export_start__proxy: 'sync',
  gf_export_start: (job, cfgPtr) => {
    var cfg = JSON.parse(UTF8ToString(cfgPtr));
    var e = { cfg, parts: [], pending: [], encoder: null, ready: false, n: 0, failed: false };
    GFWEB.exports[job] = e;
    var fail = (msg) => { if (e.failed) return; e.failed = true; console.error('gfweb export:', msg); var p = stringToNewUTF8(String(msg)); _gf_export_error(job, p); };
    e.encoder = new VideoEncoder({
      output: (chunk, meta) => {
        var desc = meta?.decoderConfig?.description;
        if (desc) {
          var d = new Uint8Array(desc instanceof ArrayBuffer ? desc : desc.buffer, desc.byteOffset || 0, desc.byteLength);
          var p = _malloc(d.length); HEAPU8.set(d, p); _gf_export_config(job, p, d.length);
        }
        var data = new Uint8Array(chunk.byteLength);
        chunk.copyTo(data);
        e.parts.push(data);
        _gf_export_chunk(job, data.length, chunk.type == 'key' ? 1 : 0);
      },
      error: (err) => fail(err.message || err),
    });
    var candidates = cfg.family == 'hevc'
      ? ['hvc1.1.6.L156.B0', 'hvc1.1.6.L153.B0', 'hvc1.1.6.L150.B0', 'hvc1.1.6.L123.B0']
      : ['avc1.640034', 'avc1.640033', 'avc1.640032', 'avc1.640028'];
    (async () => {
      for (var hw of ['prefer-hardware', 'no-preference']) for (var codec of candidates) {
        var c = { codec, width: cfg.width, height: cfg.height, bitrate: cfg.bitrate, framerate: cfg.fps, hardwareAcceleration: hw, latencyMode: 'quality',
                  ...(cfg.family == 'hevc' ? { hevc: { format: 'hevc' } } : { avc: { format: 'avc' } }) };
        try { if (!(await VideoEncoder.isConfigSupported(c)).supported) continue; } catch (err) { continue; }
        console.log(`gfweb: encoding ${codec} ${cfg.width}x${cfg.height} @ ${cfg.fps} fps, ${Math.round(cfg.bitrate / 1e6)} Mbps (${hw})`);
        e.encoder.configure(c);
        e.ready = true;
        e.pending.forEach((x) => GFWEB.encode(e, x));
        e.pending = [];
        return;
      }
      fail(`This browser can't encode ${cfg.family.toUpperCase()} at ${cfg.width}x${cfg.height}`);
    })();
  },
  // Called on the main thread with the processed RGBA frame (copied, so the wasm buffer can be reused)
  gf_export_encode__deps: ['$GFWEB'],
  gf_export_encode: (job, ptr, len, w, h, ts, dur) => {
    var e = GFWEB.exports[job];
    if (!e || e.failed) return;
    var f = new VideoFrame(HEAPU8.slice(ptr, ptr + len), { format: 'RGBA', codedWidth: w, codedHeight: h, timestamp: ts, duration: dur });
    if (e.ready) GFWEB.encode(e, f); else e.pending.push(f);
  },
  // The export GPU loop: decoded VideoFrame -> 2D canvas (same RGB values as ffmpeg, see gfUpload) -> WebGPU texture ->
  // wgpu_undistort.wgsl with gyroflow-core's per-frame params -> WebGPU canvas -> VideoFrame -> encoder. No readback.
  gf_export_run__deps: ['$GFWEB', '$UTF8ToString', 'malloc', 'gf_export_frame_params', 'gf_export_rendered', 'gf_export_flush'],
  gf_export_run__proxy: 'sync',
  gf_export_run: (job, shaderPtr, coeffsPtr, nCoeffs, inW, inH, outW, outH, frameDur) => {
    var e = GFWEB.exports[job], s = GFWEB.decoders[job];
    var code = UTF8ToString(shaderPtr).replace(/@fragment var/g, 'var'); // naga-only per-stage var attributes
    var coeffs = HEAPF32.slice(coeffsPtr >> 2, (coeffsPtr >> 2) + nCoeffs);
    var paramsOut = _malloc(24);
    var fail = () => _gf_export_rendered(job, 0, 1);
    (async () => {
      try {
        var gpu = await GFWEB.gpuDevice();
        var u = GFWEB.undistort(gpu, code, coeffs);
        var errs = (await u.module.getCompilationInfo()).messages.filter((m) => m.type == 'error');
        if (errs.length) throw new Error('WGSL: ' + errs.map((m) => m.lineNum + ': ' + m.message).join('\n'));
        var src = new OffscreenCanvas(inW, inH), src2d = src.getContext('2d', { alpha: false });
        var out = new OffscreenCanvas(outW, outH), ctx = out.getContext('webgpu');
        ctx.configure({ device: gpu, format: 'rgba8unorm', alphaMode: 'opaque' });
        var n = 0, t0 = performance.now();
        for (;;) {
          while (!s.frames.length && !s.done && !s.error && !e.failed) await new Promise((r) => { s.consumer = r; setTimeout(r, 50); });
          if (e.failed) return;
          if (s.error) return fail();
          var f = s.frames.shift();
          s.wake?.();
          if (!f) break;
          if (!_gf_export_frame_params(job, f.timestamp, paramsOut)) { f.close(); return fail(); }
          var p = HEAPU32.subarray(paramsOut >> 2, (paramsOut >> 2) + 6);
          src2d.drawImage(f, 0, 0, inW, inH);
          f.close();
          u.render(src, inW, inH, HEAPU8.slice(p[0], p[0] + p[1]), HEAPU8.slice(p[2], p[2] + p[3]), HEAPU8.slice(p[4], p[4] + p[5]), ctx.getCurrentTexture().createView());
          var vf = new VideoFrame(out, { timestamp: Math.round(n * frameDur), duration: Math.round(frameDur) });
          n++;
          while (e.encoder.state == 'configured' && e.encoder.encodeQueueSize > 4) await new Promise((r) => e.encoder.addEventListener('dequeue', r, { once: true }));
          if (e.ready) GFWEB.encode(e, vf); else e.pending.push(vf);
          _gf_export_rendered(job, 0, 0);
        }
        console.log(`gfweb: processed ${n} frames in ${((performance.now() - t0) / 1000).toFixed(1)} s (${(n / (performance.now() - t0) * 1000).toFixed(1)} fps)`);
        _gf_export_flush(job);
        _gf_export_rendered(job, 1, 0);
      } catch (err) { console.error('gfweb export:', err); fail(); }
    })();
  },
  // Frames the encoder hasn't consumed yet (C++ stops feeding above a few)
  gf_export_backlog__deps: ['$GFWEB'],
  gf_export_backlog: (job) => { var e = GFWEB.exports[job]; return e ? e.pending.length + (e.encoder.state == 'configured' ? e.encoder.encodeQueueSize : 0) : 0; },
  gf_export_flush__deps: ['$GFWEB', 'gf_export_done'],
  gf_export_flush: (job) => {
    var e = GFWEB.exports[job];
    if (!e) return;
    var wait = () => e.ready || e.failed ? Promise.resolve() : new Promise((r) => setTimeout(() => wait().then(r), 20));
    wait().then(() => e.failed ? null : e.encoder.flush()).then(() => { if (!e.failed) _gf_export_done(job); }, (err) => { console.error(err); });
  },
  gf_export_cancel__deps: ['$GFWEB'],
  gf_export_cancel__proxy: 'async',
  gf_export_cancel: (job) => {
    var e = GFWEB.exports[job];
    if (!e) return;
    e.failed = true;
    if (e.encoder.state != 'closed') e.encoder.close();
    delete GFWEB.exports[job];
  },
  // Extra mdat payload (audio samples read by Rust)
  gf_sink_write__deps: ['$GFWEB'],
  gf_sink_write__proxy: 'sync',
  gf_sink_write: (job, ptr, len) => { GFWEB.exports[job]?.parts.push(HEAPU8.slice(ptr, ptr + len)); },
  // header = ftyp + mdat header, moov at the end; downloads the file
  gf_sink_finish__deps: ['$GFWEB', '$UTF8ToString'],
  gf_sink_finish__proxy: 'sync',
  gf_sink_finish: (job, hPtr, hLen, mPtr, mLen, namePtr) => {
    var e = GFWEB.exports[job];
    if (!e) return;
    var blob = new Blob([HEAPU8.slice(hPtr, hPtr + hLen), ...e.parts, HEAPU8.slice(mPtr, mPtr + mLen)], { type: 'video/mp4' });
    delete GFWEB.exports[job];
    var a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = UTF8ToString(namePtr);
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 60000);
    console.log(`gfweb: exported ${a.download} (${(blob.size / 1048576).toFixed(1)} MB)`);
  },

  // Saving next to a picked file (/web/...) = downloading it (gyroflow_core::filesystem::write)
  gf_settings_load__deps: ['$stringToNewUTF8'],
  gf_settings_load__proxy: 'sync',
  gf_settings_load: () => { try { const s = localStorage.getItem('gyroflow-settings'); return s ? stringToNewUTF8(s) : 0; } catch { return 0; } },
  gf_settings_save__deps: ['$UTF8ToString'],
  gf_settings_save__proxy: 'sync',
  gf_settings_save: (ptr) => { try { localStorage.setItem('gyroflow-settings', UTF8ToString(ptr)); } catch (e) { console.warn('settings not saved', e); } },
  gf_web_download__deps: ['$UTF8ToString'],
  gf_web_download__proxy: 'sync',
  gf_web_download: (namePtr, ptr, len) => {
    var a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([HEAPU8.slice(ptr, ptr + len)]));
    a.download = UTF8ToString(namePtr);
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 60000);
  },

  // ---- <video> backend for qml-video-rs ----
  gf_video_create__deps: ['$GFWEB', 'qvr_web_event'],
  gf_video_create: (owner) => {
    var v = document.createElement('video');
    v.playsInline = true;
    v.preload = 'auto';
    v.loop = true;
    // Must stay rendered or requestVideoFrameCallback stops firing; Qt's canvas covers it
    Object.assign(v.style, { position: 'fixed', left: '0', top: '0', width: '2px', height: '2px', zIndex: '-1', pointerEvents: 'none' });
    document.body.appendChild(v);
    var p = { v, owner, time: -1, from: 0, to: 0, url: null };
    var ev = (type, a) => { if (p.owner) _qvr_web_event(p.owner, type, a || 0); };
    var onFrame = (now, md) => {
      p.time = md.mediaTime;
      if (p.to > p.from && md.mediaTime * 1000 >= p.to) v.currentTime = p.from / 1000;
      ev(1, md.mediaTime);
      v.requestVideoFrameCallback(onFrame);
    };
    v.requestVideoFrameCallback(onFrame);
    v.addEventListener('loadedmetadata', () => ev(2, v.duration));
    v.addEventListener('loadeddata', () => ev(1, v.currentTime));
    v.addEventListener('play', () => ev(3, 1));
    v.addEventListener('pause', () => ev(3, 2));
    v.addEventListener('waiting', () => ev(4, 1));
    v.addEventListener('playing', () => ev(4, 0));
    v.addEventListener('seeked', () => { p.time = v.currentTime; ev(1, v.currentTime); ev(4, 0); });
    v.addEventListener('error', () => ev(5, 0));
    return GFWEB.players.push(p) - 1;
  },
  gf_video_destroy__deps: ['$GFWEB'],
  gf_video_destroy: (h) => {
    var p = GFWEB.players[h];
    if (!p) return;
    p.owner = 0;
    p.v.pause();
    p.v.removeAttribute('src');
    p.v.load();
    p.v.remove();
    if (p.url) URL.revokeObjectURL(p.url);
    GFWEB.players[h] = null;
  },
  gf_video_set_url__deps: ['$GFWEB', '$UTF8ToString'],
  gf_video_set_url: (h, path) => {
    var p = GFWEB.players[h], s = UTF8ToString(path), f = GFWEB.fileByPath(s);
    if (p.url) URL.revokeObjectURL(p.url);
    p.url = f ? URL.createObjectURL(f) : null;
    p.time = -1;
    p.rot = undefined;
    p.v.src = p.url || s;
  },
  gf_video_play__deps: ['$GFWEB'],
  gf_video_play: (h) => { GFWEB.players[h].v.play().catch((e) => console.warn('video play:', e.message)); },
  gf_video_pause__deps: ['$GFWEB'],
  gf_video_pause: (h) => GFWEB.players[h].v.pause(),
  gf_video_seek__deps: ['$GFWEB'],
  gf_video_seek: (h, ms, exact) => {
    var v = GFWEB.players[h].v;
    if (!exact && v.fastSeek) v.fastSeek(ms / 1000); else v.currentTime = ms / 1000;
  },
  gf_video_set_range__deps: ['$GFWEB'],
  gf_video_set_range: (h, from, to) => { var p = GFWEB.players[h]; p.from = from; p.to = to; },
  gf_video_set__deps: ['$GFWEB'],
  gf_video_set: (h, what, val) => {
    var v = GFWEB.players[h].v;
    if (what == 0) v.muted = !!val;
    else if (what == 1) v.volume = Math.max(0, Math.min(1, val));
    else if (what == 2) v.playbackRate = val;
  },
  gf_video_get__deps: ['$GFWEB'],
  gf_video_get: (h, what) => {
    var v = GFWEB.players[h].v;
    var p = GFWEB.players[h];
    return [v.muted ? 1 : 0, v.volume, v.playbackRate, v.currentTime, v.duration || 0, v.videoWidth, v.videoHeight, v.paused ? 1 : 0, p.tw || 0, p.th || 0][what];
  },
  // Copies the current video frame into GL texture `tex` (GPU to GPU, the browser's hardware decoder output).
  // Colour-space conversion is off so RGB values match ffmpeg/desktop Gyroflow. Returns the frame's time in s, or -1.
  gf_video_upload__deps: ['$GFWEB', '$GL'],
  gf_video_upload: (h, tex) => {
    var p = GFWEB.players[h], v = p?.v;
    if (!p || v.readyState < 2) return -1;
    // The browser presents frames with the container rotation applied; Gyroflow wants them sensor-oriented (as MDK
    // delivers them) and applies the rotation itself, so rotate them back.
    if (p.rot === undefined) { try { var vf = new VideoFrame(v); p.rot = vf.rotation || 0; vf.close(); } catch (e) { p.rot = 0; } }
    var src = v, swap = p.rot % 180 != 0;
    p.tw = swap ? v.videoHeight : v.videoWidth;
    p.th = swap ? v.videoWidth : v.videoHeight;
    if (p.rot) {
      if (!p.cv || p.cv.width != p.tw || p.cv.height != p.th) { p.cv = new OffscreenCanvas(p.tw, p.th); p.cx = p.cv.getContext('2d', { alpha: false }); }
      p.cx.setTransform(1, 0, 0, 1, p.tw / 2, p.th / 2);
      p.cx.rotate(-p.rot * Math.PI / 180);
      p.cx.drawImage(v, -v.videoWidth / 2, -v.videoHeight / 2);
      src = p.cv;
    }
    var gl = GLctx;
    gl.bindTexture(gl.TEXTURE_2D, GL.textures[tex]);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.NONE);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, src);
    gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.BROWSER_DEFAULT_WEBGL);
    gl.bindTexture(gl.TEXTURE_2D, null);
    return p.time >= 0 ? p.time : p.v.currentTime;
  },
});
