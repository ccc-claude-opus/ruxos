// Request latency and throughput of a running ruwebos (any kernel), through the port forward.
//   node tools/bench.mjs [http://localhost:8700] [--label linux] [--out doc/bench.json]
// For each URL: 500 sequential requests (p50 / p99 latency), then 10 concurrent clients for 3 s
// (requests per second). The first 50 requests warm up and are not counted.
import { writeFileSync, readFileSync, existsSync } from 'node:fs';

const args = process.argv.slice(2);
const opt = (k, d) => (args.includes(k) ? args[args.indexOf(k) + 1] : d);
const edge = new URL(args.find((a) => a.startsWith('http')) || 'http://localhost:8700');
const label = opt('--label', 'unnamed');
const out = opt('--out', null);
const host = (app) => `${edge.protocol}//${app ? app + '.' : ''}localhost:${edge.port}`;
const targets = [
  ['home page', host('') + '/'],
  ['static file', host('calculator') + '/'],
  ['D1 query', host('notes') + '/'],
];

async function once(url) {
  const r = await fetch(url, { headers: { cookie: 'tz=UTC' } });
  await r.arrayBuffer();
  if (r.status >= 500) throw new Error(`${url}: ${r.status}`);
}

const pct = (xs, p) => xs[Math.min(xs.length - 1, Math.floor((xs.length * p) / 100))];
const results = {};
for (const [name, url] of targets) {
  for (let i = 0; i < 50; i++) await once(url);
  const times = [];
  for (let i = 0; i < 500; i++) {
    const t0 = performance.now();
    await once(url);
    times.push(performance.now() - t0);
  }
  times.sort((a, b) => a - b);
  let n = 0;
  const end = performance.now() + 3000;
  await Promise.all(Array.from({ length: 10 }, async () => {
    while (performance.now() < end) { await once(url); n++; }
  }));
  results[name] = { p50_ms: +pct(times, 50).toFixed(2), p99_ms: +pct(times, 99).toFixed(2), rps: Math.round(n / 3) };
  console.log(`${name.padEnd(12)} p50 ${results[name].p50_ms} ms  p99 ${results[name].p99_ms} ms  ${results[name].rps} req/s`);
}
if (out) {
  const all = existsSync(out) ? JSON.parse(readFileSync(out, 'utf8')) : {};
  all[label] = { date: new Date().toISOString().slice(0, 10), ...results };
  writeFileSync(out, JSON.stringify(all, null, 2) + '\n');
}
