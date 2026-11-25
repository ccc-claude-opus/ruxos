// ruxos: the path a caged rucloud Worker takes to the binding API, under load: an HTTP server on a
// Unix socket, a forwarder on 127.0.0.1 that pipes each connection to it (as rucloud's host.mjs
// does), and keep-alive fetch clients. Every request must succeed. Prints "forward-test: ok".
//   node forward.js [seconds]
const http = require('node:http');
const net = require('node:net');
const fs = require('node:fs');
const secs = Number(process.argv[2] || 3);
(async () => {
  const sock = '/tmp/forward-test.sock';
  try { fs.unlinkSync(sock); } catch {}
  const srv = http.createServer((q, r) => {
    let n = 0;
    q.on('data', (d) => (n += d.length));
    q.on('end', () => r.end(JSON.stringify({ url: q.url, n })));
  });
  await new Promise((r) => srv.listen(sock, r));
  const fwd = net.createServer((c) => {
    const up = net.connect(sock);
    c.on('error', () => up.destroy());
    up.on('error', () => c.destroy());
    c.pipe(up).pipe(c);
  });
  await new Promise((r) => fwd.listen(0, '127.0.0.1', r));
  const base = `http://127.0.0.1:${fwd.address().port}`;
  let ok = 0, failed = 0, first = '';
  const end = Date.now() + secs * 1000;
  const worker = async (id) => {
    let i = 0;
    while (Date.now() < end) {
      try {
        const r = await fetch(`${base}/${id}/${i++}`, { method: 'POST', body: 'x'.repeat(100), headers: { 'content-type': 'text/plain' } });
        const j = await r.json();
        if (j.n === 100) ok++;
        else throw new Error('bad body ' + JSON.stringify(j));
      } catch (e) {
        failed++;
        if (!first) first = `${e.message}${e.cause ? ': ' + (e.cause.code || e.cause.message) : ''}`;
      }
    }
  };
  await Promise.all(Array.from({ length: 10 }, (_, i) => worker(i)));
  console.log(`${failed ? 'FAIL' : 'ok'} forward: ${ok} requests, ${failed} failed${first ? ' (' + first + ')' : ''}`);
  console.log(failed ? 'forward-test: FAIL' : 'forward-test: ok');
  process.exit(0);
})();
