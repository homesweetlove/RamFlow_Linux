// Drives a real Chrome through the DevTools protocol with trusted input events and measures
// "input → second animation frame" for two features of the dashboard. No dependencies.
//
// usage: node driver.mjs --port 9333 --stabilize 12 --warmup 5 --n 30 --seed 1 --think 150
import { pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const args = Object.fromEntries(process.argv.slice(2).reduce((a, x, i, all) => (x.startsWith('--') ? [...a, [x.slice(2), all[i + 1]]] : a), []));
const PORT = +args.port, STAB = +(args.stabilize ?? 12), WARM = +(args.warmup ?? 5), N = +(args.n ?? 30);
const THINK = +(args.think ?? 150);
let seed = +(args.seed ?? 1);
const rnd = () => ((seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) / 4294967296);
const sleep = ms => new Promise(r => setTimeout(r, ms));

// --- the same record model as the page, computed independently ---
const score = id => (Math.imul(id, 2654435761) >>> 0) % 100000;
const expectedFor = (g, dir) => {
  const ids = [];
  for (let id = g; id < 20000; id += 20) ids.push(id);
  if (dir !== 0) ids.sort((a, b) => (dir > 0 ? score(b) - score(a) : score(a) - score(b)) || a - b);
  return { rows: ids.length, first: ids[0], last: ids[ids.length - 1] };
};

// --- minimal CDP client ---
const targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = targets.find(t => t.type === 'page');
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
let nextId = 1; const pending = new Map(); let waiter = null;
ws.onmessage = ev => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  else if (m.method === 'Runtime.bindingCalled' && m.params.name === '__done' && waiter) { const w = waiter; waiter = null; w(JSON.parse(m.params.payload)); }
};
const send = (method, params = {}) => new Promise(res => { const id = nextId++; pending.set(id, res); ws.send(JSON.stringify({ id, method, params })); });
const evalJs = async expr => (await send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true })).result.result.value;

await send('Page.enable'); await send('Runtime.enable');
await send('Runtime.addBinding', { name: '__done' });
const url = pathToFileURL(join(dirname(fileURLToPath(import.meta.url)), 'dashboard.html')).href;
await send('Page.navigate', { url });
for (let i = 0; i < 100 && !(await evalJs('typeof window.__state === "function"')); i++) await sleep(100);

let dir = 0, group = -1, errors = [];
const hr = () => Number(process.hrtime.bigint()) / 1e6;
const nextDone = () => new Promise(res => { waiter = res; });

async function doSearch(timed) {
  let g; do { g = Math.floor(rnd() * 20); } while (g === group);
  await evalJs(`(() => { const q = document.getElementById('q'); q.focus(); q.value = ''; return 1 })()`);
  const done = nextDone();
  const t0 = hr();
  await send('Input.insertText', { text: 'g' + String(g).padStart(2, '0') });
  const inner = await done; const ms = hr() - t0;
  group = g; // search keeps the current sort direction
  const st = await evalJs('window.__state()');
  const exp = expectedFor(g, dir);
  if (st.rows !== exp.rows || st.first !== exp.first || st.last !== exp.last) errors.push({ op: 'search', g, st, exp });
  return timed ? { ms, inner: inner.inner } : null;
}
async function doSort(timed) {
  const r = await evalJs(`(() => { const b = document.getElementById('sort').getBoundingClientRect(); return { x: b.x + b.width / 2, y: b.y + b.height / 2 } })()`);
  const done = nextDone();
  const t0 = hr();
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', x: r.x, y: r.y, button: 'left', clickCount: 1 });
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: r.x, y: r.y, button: 'left', clickCount: 1 });
  const inner = await done; const ms = hr() - t0;
  dir = dir === 1 ? -1 : 1;
  const st = await evalJs('window.__state()');
  const exp = expectedFor(group < 0 ? 0 : group, dir);
  const full = group < 0 ? { rows: 0 } : exp;
  if (group >= 0 && (st.rows !== full.rows || st.first !== full.first || st.last !== full.last)) errors.push({ op: 'sort', st, exp: full });
  return timed ? { ms, inner: inner.inner } : null;
}

// Stabilization: the foreground is busy, like a user working, so a controller can react.
await doSearch(false);
const stabEnd = Date.now() + STAB * 1000;
// Like a person working: an action, a short pause, the next action (not back-to-back, which
// would saturate the browser by itself).
while (Date.now() < stabEnd) { await doSort(false); await sleep(THINK); await doSearch(false); await sleep(THINK); }
for (let i = 0; i < WARM; i++) { await doSearch(false); await doSort(false); await sleep(THINK); }

await sleep(1000);
const startMs = Date.now();
const search = [], sort = [];
for (let i = 0; i < N; i++) {
  search.push(await doSearch(true)); await sleep(THINK);
  sort.push(await doSort(true)); await sleep(THINK);
}
const endMs = Date.now();
console.log(JSON.stringify({ search, sort, errors, window: { startMs, endMs }, operations: 2 * N }));
ws.close();
process.exit(0);
