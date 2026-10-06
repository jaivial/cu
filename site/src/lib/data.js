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
