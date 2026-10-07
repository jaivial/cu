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

// Two rounds of PARALLEL AGENTS: real headless mini-tui sessions driven by
// MiniMax-M3.1-Flash-Preview against the Sage chat of neural-dev. Each agent
// opens its own cu context (isolated cookie jar) in the ONE shared Chrome,
// signs in, opens a new conversation, sends a marker and waits for the answer
// by text (marker twice: echo + streamed reply). Model latency, one bash
// round-trip per step and a whole agent session are all inside these numbers.
// Box: 12 cores, 64 GB, ~8 GB of swap already consumed before the run.
// Sampled every 5 s; the 3 GB hard floor never came close. `avail` is
// MemAvailable MB, the minimum of the level. `f429` = 429 events / agents hit
// / fatal (round 2 only; round 1 counted them as failures).
// Round 1, 2026-10-06: levels 1-6 from a sweep whose stopwatch collected
// finished agents serially, so those latencies are floors; the 10 and 12 rows
// are the corrected repeat, one timing thread per agent.
export const parRound1 = [
  { c: 1, ok: 1, of: 1, rate: 100, p50: 117.6, p95: null, avail: 24700, chrome: 1.8, agents: 0.06 },
  { c: 2, ok: 2, of: 2, rate: 100, p50: 162.4, p95: 162.4, avail: 24494, chrome: 2.2, agents: 0.13 },
  { c: 4, ok: 4, of: 4, rate: 100, p50: 136.9, p95: 136.9, avail: 24137, chrome: 2.7, agents: 0.25 },
  { c: 6, ok: 6, of: 6, rate: 100, p50: 75.3, p95: 149.6, avail: 23729, chrome: 3.4, agents: 0.36 },
  { c: 8, ok: 8, of: 8, rate: 100, p50: 162.6, p95: 162.6, avail: 23311, chrome: 4.1, agents: 0.48 },
  { c: 10, ok: 9, of: 10, rate: 90, p50: 136.8, p95: 244.4, avail: 22712, chrome: 4.6, agents: 0.59 },
  { c: 12, ok: 11, of: 12, rate: 91.7, p50: 162.6, p95: 270.9, avail: 22280, chrome: 5.3, agents: 0.71 },
];

// Round 2, 2026-10-07: the model's token plan had reset, so 8-16 were re-run
// with a 429 counter. A reset cut the 429s at 8 and 10 to zero and lifted the
// already-measured levels, but the same 429 came back at 12b, 14 and 16. The
// hard stop rule (success < 80%) fired at 16.
export const parRound2 = [
  { c: 8, rep: false, ok: 8, of: 8, rate: 100, p50: 121.7, p95: 271.0, avail: 22516, chrome: 3.6, agents: 0.48, ev429: 0, hit429: 0, fatal429: 0 },
  { c: 10, rep: false, ok: 10, of: 10, rate: 100, p50: 76.3, p95: 261.9, avail: 23031, chrome: 4.2, agents: 0.6, ev429: 0, hit429: 0, fatal429: 0 },
  { c: 12, rep: false, ok: 12, of: 12, rate: 100, p50: 125.7, p95: 200.3, avail: 22747, chrome: 4.7, agents: 0.72, ev429: 0, hit429: 0, fatal429: 0 },
  { c: 12, rep: true, ok: 11, of: 12, rate: 91.7, p50: 125.4, p95: 202.5, avail: 20261, chrome: 4.7, agents: 0.71, ev429: 3, hit429: 2, fatal429: 1 },
  { c: 14, rep: false, ok: 12, of: 14, rate: 85.7, p50: 180.7, p95: 374.3, avail: 22063, chrome: 5.1, agents: 0.82, ev429: 8, hit429: 2, fatal429: 2 },
  { c: 16, rep: false, ok: 12, of: 16, rate: 75.0, p50: 180.7, p95: 368.5, avail: 21939, chrome: 5.6, agents: 0.94, ev429: 7, hit429: 3, fatal429: 3 },
];

export const parHeadline = [
  { k: '8-10', d: 'agentes en paralelo: seguro, 100 % dos veces' },
  { k: '12', d: 'el l\u00edmite: 100 % una vez, 91,7 % al repetir' },
  { k: '~0,3 GB', d: 'de RAM marginal por agente (Chrome + proceso)' },
  { k: '20,2 GB', d: 'de RAM libre m\u00ednima medida, suelo en 3 GB' },
];

// The 429, verbatim as Sage returned it inside the conversation.
export const par429 = 'Error code: 429 - rate_limit_error: All credentials for model claude-minimax-m2-5-highspeed are cooling down via provider claude';

// Per-agent marginal cost, measured from the level tables above.
export const parCost = [
  { what: 'contexto en el Chrome compartido', mem: '~0,25 GB', note: '~300 MB por contexto abierto, el consumidor dominante' },
  { what: 'proceso del agente (mini-tui headless)', mem: '~0,06 GB', note: 'plano: 15-60 MB, 16 agentes = 939 MB' },
  { what: 'total por agente', mem: '~0,3 GB', note: 'nada superlineal: 2x agentes = ~1,6x la RAM de Chrome' },
];

// Failure classification across both rounds. 429s are the app's gateway, not cu.
export const parFails = [
  { label: '429 del proveedor (gateway de Sage)', n: 9, d: 'todos con el mismo error, devuelto dentro de la conversaci\u00f3n' },
  { label: 'auto-interrupci\u00f3n del agente', n: 1, d: 'el agente se cort\u00f3 solo antes de su ventana de 180 s: decisi\u00f3n suya, no capacidad' },
  { label: 'UI, login, conversaci\u00f3n nueva, marcador', n: 0, d: 'funcionaron en todos los intentos, sin excepci\u00f3n' },
];
