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
  gf_web_pick_files__postset: 'globalThis.gfWebAddFiles = (files) => GFWEB.addFiles(files);',
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
    return [v.muted ? 1 : 0, v.volume, v.playbackRate, v.currentTime, v.duration || 0, v.videoWidth, v.videoHeight, v.paused ? 1 : 0][what];
  },
  // Copies the current video frame into GL texture `tex` (GPU to GPU, the browser's hardware decoder output).
  // Colour-space conversion is off so RGB values match ffmpeg/desktop Gyroflow. Returns the frame's time in s, or -1.
  gf_video_upload__deps: ['$GFWEB', '$GL'],
  gf_video_upload: (h, tex) => {
    var p = GFWEB.players[h];
    if (!p || p.v.readyState < 2) return -1;
    var gl = GLctx;
    gl.bindTexture(gl.TEXTURE_2D, GL.textures[tex]);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.NONE);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, p.v);
    gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.BROWSER_DEFAULT_WEBGL);
    gl.bindTexture(gl.TEXTURE_2D, null);
    return p.time >= 0 ? p.time : p.v.currentTime;
  },
});
