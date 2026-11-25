// ruxos: networking from Node.js on this host: TCP over loopback (HTTP), UDP, AF_UNIX HTTP,
// interfaces through netlink. Prints "net-test: ok".
const http = require('node:http');
const dgram = require('node:dgram');
const net = require('node:net');
const os = require('node:os');
const fs = require('node:fs');
const results = [];
const check = (n, ok) => { results.push(ok); console.log(`${ok ? 'ok' : 'FAIL'} ${n}`); };
(async () => {
  const ifs = os.networkInterfaces();
  check('interfaces', ifs.lo && ifs.lo[0].address === '127.0.0.1');
  const srv = http.createServer((q, r) => r.end('pong ' + q.url));
  await new Promise((r) => srv.listen(8702, '0.0.0.0', r));
  const t = await (await fetch('http://127.0.0.1:8702/x')).text();
  check('http-loopback', t === 'pong /x');
  // many requests in parallel (keep-alive and new connections)
  const many = await Promise.all(Array.from({ length: 50 }, (_, i) => fetch('http://127.0.0.1:8702/' + i).then((r) => r.text())));
  check('http-parallel', many.every((x, i) => x === 'pong /' + i));
  // a big body
  const big = http.createServer((q, r) => r.end(Buffer.alloc(4 << 20, 7)));
  await new Promise((r) => big.listen(8703, r));
  const buf = Buffer.from(await (await fetch('http://127.0.0.1:8703/')).arrayBuffer());
  check('http-4mb', buf.length === 4 << 20 && buf[12345] === 7);
  // UDP
  const a = dgram.createSocket('udp4');
  const got = new Promise((r) => a.on('message', (m, ri) => r(m.toString() + ' ' + (ri.address === '127.0.0.1'))));
  await new Promise((r) => a.bind(9999, '127.0.0.1', r));
  const b = dgram.createSocket('udp4');
  b.send('dgram', 9999, '127.0.0.1');
  check('udp', (await got) === 'dgram true');
  // HTTP over a Unix socket
  try { fs.unlinkSync('/tmp/h.sock'); } catch {}
  const us = http.createServer((q, r) => r.end('unix ok'));
  await new Promise((r) => us.listen('/tmp/h.sock', r));
  const ut = await new Promise((r) => http.get({ socketPath: '/tmp/h.sock', path: '/' }, (res) => { let d = ''; res.on('data', (c) => (d += c)); res.on('end', () => r(d)); }));
  check('http-unix', ut === 'unix ok');
  // refused connection
  const err = await new Promise((r) => { const s = net.connect(1, '127.0.0.1'); s.on('error', (e) => r(e.code)); s.on('connect', () => r('connected')); });
  check('econnrefused', err === 'ECONNREFUSED');
  console.log(results.every(Boolean) ? 'net-test: ok' : 'net-test: FAIL');
  process.exit(0);
})().catch((e) => { console.log('net-test: FAIL ' + e.stack + ' cause: ' + (e.cause && (e.cause.stack || e.cause))); process.exit(1); });
