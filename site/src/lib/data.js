// Every number below is copied from docs/BENCHMARKS.md.

export const tools = [
  { id: 'cu', label: 'cu', color: 'var(--cu)' },
  { id: 'batch', label: 'cu act (lote)', color: 'var(--cu-2)' },
  { id: 'ab', label: 'agent-browser', color: 'var(--ab)' },
  { id: 'pw', label: 'Playwright MCP', color: 'var(--pw)' },
];

// Median of 10 rounds, ms, same Chrome headless shell 153, same local site.
export const tasks = [
  { name: 'Arranque en frío + primera página', key: 'cold', v: { cu: 206, batch: 220, ab: 321, pw: 2437 } },
  { name: 'Abrir página', key: 'open', v: { cu: 14, batch: 12, ab: 20, pw: 53 } },
  { name: 'Snapshot', key: 'snap', v: { cu: 3.7, batch: 3.8, ab: 11, pw: 12 } },
  { name: 'Click', key: 'click', v: { cu: 5.5, batch: 6.7, ab: 8.1, pw: 567 } },
  { name: 'Formulario', note: 'abrir, leer, 2 campos, select, enviar, leer', key: 'form', v: { cu: 83, batch: 73, ab: 162, pw: 727 } },
  { name: 'Login', note: 'abrir, leer, usuario, contraseña + Enter, leer', key: 'login', v: { cu: 81, batch: 81, ab: 157, pw: 670 } },
  { name: '10 clicks seguidos', key: 'ten', v: { cu: 55, batch: 25, ab: 155, pw: 5491 } },
];

// Same 2.3 KB catalogue page, tiktoken o200k_base.
export const tokens = [
  { tool: 'cu', cmd: 'snapshot', tokens: 350, bytes: 931, color: 'var(--cu)', best: true },
  { tool: 'agent-browser', cmd: 'snapshot -i', tokens: 480, bytes: 1148, color: 'var(--ab)' },
  { tool: 'agent-browser', cmd: 'snapshot', tokens: 953, bytes: null, color: 'var(--ab)' },
  { tool: 'Playwright MCP', cmd: 'browser_snapshot', tokens: 1314, bytes: 3684, color: 'var(--pw)' },
];

// cu before/after the speed work, same loaded machine.
export const speed = [
  { action: 'cu start', before: 461.7, after: 48.8, note: '9,5x · hasta 71x en máquina tranquila' },
  { action: 'shot (jpeg)', before: 48.1, after: 43.1, note: '1,1x' },
  { action: 'shot x8 en paralelo', before: 86.6, after: 73.0, note: '1,2x' },
];

export const actionCosts = [
  { what: 'click sin navegación, dentro de un lote', ms: '~1 ms' },
  { what: 'click sin navegación, una llamada HTTP', ms: '3-5 ms' },
  { what: 'click que navega (hasta DOM nuevo listo)', ms: '15-35 ms' },
];

export const contexts = [
  { what: 'un segundo navegador', mem: '~176 MB', ready: '0,5-1 s' },
  { what: 'un contexto en el navegador vivo', mem: '~19 MB', ready: '~35 ms' },
];

export const resources = [
  { what: 'procesos', before: '11', after: '10' },
  { what: 'PSS', before: '~337 MB', after: '~316 MB' },
  { what: 'tiempo de CPU', before: '~1,65 s', after: '~1,15 s (−30%)' },
  { what: 'navegador huérfano al parar', before: 'todo', after: 'nada' },
];

// 50 parallel tests, one Chrome, one leased tab per test. One run,
// 2026-10-06, machine load ~2.5-3 of 12 cores ("cargado").
export const sweep = [
  { c: 5, tests: 9, star: true, tps: 0.15, p50: 0.15, p95: 0.17, cpu: 0.05, rss: 0.8, rssCu: 2.1, busy: 32 },
  { c: 10, tests: 20, tps: 12.4, p50: 0.29, p95: 0.30, cpu: 0.12, rss: 1.0, rssCu: 2.8, busy: 38 },
  { c: 20, tests: 40, tps: 18.9, p50: 0.52, p95: 0.56, cpu: 0.18, rss: 1.3, rssCu: 4.3, busy: 55 },
  { c: 35, tests: 70, tps: 22.7, p50: 0.96, p95: 1.02, cpu: 0.33, rss: 1.5, rssCu: 8.1, busy: 63 },
  { c: 50, tests: 100, tps: 24.5, p50: 1.40, p95: 1.50, cpu: 0.35, rss: 1.7, rssCu: 10.6, busy: 69 },
];

// The 50-way headline, same run.
export const sweepHeadline = [
  { k: '50/50', d: 'tests en verde a 50 vías' },
  { k: '2,1 s', d: 'de pared para una oleada de 50' },
  { k: '~18 MB', d: 'por pestaña abierta en Chrome' },
  { k: '0,35', d: 'de 12 cores de CPU de Chrome (máx.)' },
];

// Real production site in a dev build (Vite dev server, one request per module,
// ~6 sockets per origin), over the network. Signed-in session shared by the
// flows of a level, one leased tab per flow in the one shared Chrome. p50/p95
// over the flows of each level; shared 12-core box (load1 5-11). Same run for
// every table below. "answer" is the assistant's streamed chat answer.
export const realLevels = [
  { c: 1, ok: 4, of: 5, p50: 22.1, p95: 25.1, ans: 6.9, frac: 31, cores: 0.15, rss: 5.7, cuCores: 0.01 },
  { c: 10, ok: 18, of: 20, p50: 67.4, p95: 80.1, ans: 6.5, frac: 12, cores: 0.25, rss: 6.3, cuCores: 0.03 },
  { c: 25, ok: 3, of: 25, p50: 48.2, p95: 74.0, ans: 5.2, frac: 11, cores: 0.28, rss: 6.5, cuCores: 0.03 },
];

// The 10-flow level, same run: the recommendation for this deployment.
export const realHeadline = [
  { k: '18 / 20', d: 'flujos en verde a 10 en paralelo' },
  { k: '67,4 s', d: 'p50 por flujo, ~6,5 s de respuesta' },
  { k: '0,25', d: 'de 12 cores de CPU de Chrome (media)' },
  { k: '3 / 25', d: 'a 25 en paralelo: límite de la app, no de cu' },
];

// Waiting by text vs networkidle, one instrumented flow (CDP Network events).
export const realWaits = [
  { label: 'navigate devuelve', sub: 'asentamiento inteligente del DOM', t: '2,2 s', frac: 2.2 / 11.7 },
  { label: 'networkidle tras esa navegación', sub: 'dos streams de eventos siempre abiertos', t: 'nunca', frac: null },
  { label: 'respuesta visible por texto, tras enviar', sub: 'el marcador aparece dos veces', t: '11,7 s', frac: 1, best: true },
  { label: 'networkidle tras el envío', sub: 'los streams de la app nunca cierran', t: 'nunca', frac: null },
];

// Blocking images/fonts/media over CDP (cu has no switch yet), level 10.
export const realBlock = [
  { label: 'baseline', t: [67.4, 80.1, 6.5], ok: '18 / 20' },
  { label: 'imágenes/fuentes/media bloqueadas', t: [65.5, 76.2, 4.7], ok: '19 / 20' },
];

// The same 10 flows through the agent-browser CLI, one browser per session.
export const realVsAb = [
  { tool: 'cu', mode: 'un Chrome compartido', ok: '18 / 20', p50: 67.4, p95: 80.1, open: 0.1, ans: 6.5, procs: 27, rss: 6.3 },
  { tool: 'agent-browser', mode: 'un navegador por sesión', ok: '18 / 20', p50: 58.6, p95: 69.7, open: 12.0, ans: 9.2, procs: 113, rss: 12.8 },
];
