// Dev-only: drive the Gyroflow wasm build in headless Chrome over CDP.
// node tools/gftest.mjs <out-dir> [steps.json]   steps: [["wait", ms] | ["click", x, y] | ["type", text] | ["key", key] | ["eval", js] | ["shot", name] | ["pick", [files]]]
import { spawn } from 'node:child_process';
import fs from "node:fs";
import os from "node:os";

const [,, outDir = '.', stepsFile] = process.argv;
const PORT = 9333, URL_ = 'http://localhost:8766/';
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function browser() {
  try { return await (await fetch(`http://127.0.0.1:${PORT}/json/version`)).json(); } catch { }
  spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, `--user-data-dir=${os.tmpdir()}/chrome-gftest`, '--autoplay-policy=no-user-gesture-required',
                 '--window-size=1600,1000', 'about:blank'], { detached: true, stdio: 'ignore' }).unref();
  for (let i = 0; i < 50; i++) { await sleep(200); try { return await (await fetch(`http://127.0.0.1:${PORT}/json/version`)).json(); } catch { } }
  throw new Error('chrome did not start');
}

const ver = await browser();
const bws = new WebSocket(ver.webSocketDebuggerUrl);
await new Promise((r) => bws.onopen = r);
let id = 0; const pend = new Map(); const logs = [];
const handlers = [];
bws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.id && pend.has(m.id)) { pend.get(m.id)(m); pend.delete(m.id); return; }
  if (m.method === 'Runtime.consoleAPICalled') logs.push(`${m.params.type}: ` + m.params.args.map((a) => a.value ?? a.description).join(' '));
  if (m.method === 'Runtime.exceptionThrown') logs.push('EXC: ' + (m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text));
  handlers.forEach((h) => h(m));
};
const send = (method, params = {}, sessionId) => new Promise((r) => { const i = ++id; pend.set(i, (m) => r(m.error ? { error: m.error } : m.result)); bws.send(JSON.stringify({ id: i, method, params, sessionId })); });

const { targetId } = await send('Target.createTarget', { url: 'about:blank' });
const { sessionId } = await send('Target.attachToTarget', { targetId, flatten: true });
const s = (method, params) => send(method, params, sessionId);
await s('Runtime.enable'); await s('Page.enable');
await s('Emulation.setDeviceMetricsOverride', { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false });
if (process.env.THROTTLE_MBPS) { await s('Network.enable'); await s('Network.setCacheDisabled', { cacheDisabled: true }); await s('Network.emulateNetworkConditions', { offline: false, latency: 20, uploadThroughput: -1, downloadThroughput: +process.env.THROTTLE_MBPS * 131072 }); }
if (process.env.DOWNLOAD_DIR) await send('Browser.setDownloadBehavior', { behavior: 'allow', downloadPath: process.env.DOWNLOAD_DIR, eventsEnabled: true });
await s('Page.navigate', { url: URL_ });
if (process.env.EARLY_SHOT_MS) setTimeout(async () => { const shot = await s('Page.captureScreenshot', { format: 'png' }); fs.writeFileSync(`${outDir}/loading.png`, Buffer.from(shot.data, 'base64')); }, +process.env.EARLY_SHOT_MS);

// ready = lens DB loaded (Gyroflow logs it once the UI is up)
const t0 = Date.now();
while (!logs.some((l) => /Loaded \d+ lens profiles/.test(l)) && Date.now() - t0 < 90000) await sleep(250);
await sleep(1500);
console.log(`ready after ${((Date.now() - t0) / 1000).toFixed(1)}s`);

const evaluate = async (expr) => {
  const r = await s('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true, timeout: 60000 });
  return r?.result?.value ?? r?.exceptionDetails?.exception?.description ?? r?.error;
};
const steps = stepsFile ? JSON.parse(fs.readFileSync(stepsFile, 'utf8')) : [['shot', 'start']];
for (const [op, ...a] of steps) {
  if (op === 'wait') await sleep(a[0]);
  else if (op === 'click') {
    for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased'])
      await s('Input.dispatchMouseEvent', { type, x: a[0], y: a[1], button: 'left', clickCount: 1 });
  } else if (op === 'key') {
    await s('Input.dispatchKeyEvent', { type: 'keyDown', key: a[0] }); await s('Input.dispatchKeyEvent', { type: 'keyUp', key: a[0] });
  } else if (op === 'type') {
    await s('Input.insertText', { text: a[0] });
  } else if (op === 'eval') console.log('eval:', JSON.stringify(await evaluate(a[0])));
  else if (op === 'pick') {
    // the next <input type=file>.click() gets these files (served from www-gyroflow/test/)
    console.log('pick:', await evaluate(`(async () => {
      const files = await Promise.all(${JSON.stringify(a[0])}.map(async (n) => new File([await (await fetch('test/' + n)).blob()], n)));
      HTMLInputElement.prototype.click = function () {
        if (this.type !== 'file') return;
        const dt = new DataTransfer(); files.forEach((f) => dt.items.add(f)); this.files = dt.files; this.dispatchEvent(new Event('change'));
      };
      return files.map((f) => f.name + ':' + f.size).join(',');
    })()`));
  } else if (op === 'shot') {
    const shot = await s('Page.captureScreenshot', { format: 'png' });
    fs.writeFileSync(`${outDir}/${a[0]}.png`, Buffer.from(shot.data, 'base64'));
    console.log('shot:', `${outDir}/${a[0]}.png`);
  }
}
console.log('--- console (' + logs.length + ' lines) ---');
console.log(logs.filter((l) => !/QML Connections: Detected function|RenderQueue\.qml/.test(l)).map((l) => l.slice(0, 300)).join('\n'));
await send('Target.closeTarget', { targetId });
process.exit(0);
