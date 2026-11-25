// ruxos: Node.js features that lean on the kernel. Prints "node-test: ok" at the end.
const assert = require('node:assert');
const { Worker } = require('node:worker_threads');
const cp = require('node:child_process');
const crypto = require('node:crypto');
const zlib = require('node:zlib');
const fs = require('node:fs');
const os = require('node:os');
const results = [];
const check = (name, ok) => { results.push([name, ok]); console.log(`${ok ? 'ok' : 'FAIL'} ${name}`); };

(async () => {
  // WebAssembly: out-of-bounds access traps (V8 uses guard pages and a SIGSEGV handler)
  const bytes = new Uint8Array([0,0x61,0x73,0x6d,1,0,0,0, 1,6,1,0x60,1,0x7f,1,0x7f, 3,2,1,0, 5,3,1,0,1, 7,8,1,4,0x6c,0x6f,0x61,0x64,0,0, 10,9,1,7,0,0x20,0,0x28,2,0,0x0b]);
  const { instance } = await WebAssembly.instantiate(bytes);
  let trapped = false;
  try { instance.exports.load(1 << 20); } catch (e) { trapped = e instanceof WebAssembly.RuntimeError; }
  check('wasm-trap', trapped && instance.exports.load(0) === 0);
  // worker threads
  const w = new Worker('const { parentPort } = require("node:worker_threads"); parentPort.postMessage(6 * 7);', { eval: true });
  const v = await new Promise((r) => w.on('message', r));
  check('worker', v === 42);
  // child processes
  check('execSync', cp.execSync('echo hi').toString() === 'hi\n');
  const out = await new Promise((r) => cp.execFile(process.execPath, ['-e', 'process.stdout.write(String(process.pid > 1))'], (e, o) => r(o)));
  check('execFile', out === 'true');
  // crypto, zlib
  check('sha256', crypto.createHash('sha256').update('abc').digest('hex').startsWith('ba7816bf'));
  check('random', crypto.randomBytes(16).length === 16);
  const z = zlib.gzipSync(Buffer.alloc(100000, 'a'));
  check('zlib', zlib.gunzipSync(z).length === 100000);
  // files
  fs.writeFileSync('/tmp/n.txt', 'x'.repeat(10000));
  check('fs', fs.readFileSync('/tmp/n.txt', 'utf8').length === 10000 && fs.statSync('/tmp/n.txt').size === 10000);
  const dir = fs.readdirSync('/usr/local/lib/node_modules');
  check('readdir', dir.includes('npm'));
  check('os', os.cpus().length >= 1 && os.totalmem() > 0 && os.uptime() >= 0);
  // timers and promises
  const t0 = Date.now();
  await new Promise((r) => setTimeout(r, 50));
  check('timer', Date.now() - t0 >= 49);
  // a big allocation and GC
  const arrs = []; for (let i = 0; i < 50; i++) arrs.push(new Float64Array(1 << 17));
  check('heap', arrs.length === 50);
  const failed = results.filter((r) => !r[1]).length;
  console.log(failed ? `node-test: ${failed} failures` : 'node-test: ok');
})().catch((e) => { console.log('node-test: FAIL ' + e.stack); });
