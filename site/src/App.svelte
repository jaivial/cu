<script>
  import CompareTable from './lib/CompareTable.svelte';
  import { tools, tasks, tokens, speed, actionCosts, contexts, resources, sweep, sweepHeadline,
    realLevels, realHeadline, realWaits, realBlock, realVsAb,
    parRound1, parRound2, parHeadline, par429, parCost, parFails } from './lib/data.js';

  let tab = $state('cli');

  const maxSpeed = Math.max(...speed.map((s) => s.before));
  const grp = (s) => s.replace(/\B(?=(\d{3})+(?!\d))/g, '\u202f');
  const n = (v) => grp(v.toLocaleString('es-ES'));
  const fmt = (v) => (v < 10 ? v.toLocaleString('es-ES', { maximumFractionDigits: 1 }) : grp(Math.round(v).toLocaleString('es-ES')));
  const secs = (v) => v.toFixed(2).replace('.', ',') + ' s';
  const dec = (v, d) => v.toLocaleString('es-ES', { minimumFractionDigits: d, maximumFractionDigits: d });
  const mb = (v) => String(v).replace(/\B(?=(\d{3})+(?!\d))/g, '\u202f') + ' MB';

  const snapshotLines = [
    ['- url: ', 'https://shop.test/'],
    ['- page: ', 'Catálogo'],
    ['- heading ', '"Productos"'],
    ['- ref=e1 ', 'searchbox "Buscar"'],
    ['- ref=e2 ', 'link "Ofertas"'],
    ['- ref=e3 ', 'button "Añadir: Teclado"'],
    ['- ref=e4 ', 'button "Añadir: Ratón"'],
  ];

  const features = [
    {
      k: '01',
      t: 'Un Chrome que no se apaga',
      d: 'Un Chromium persistente detrás de un servidor HTTP solo en loopback, un daemon por máquina. El agente lo encuentra caliente desde cualquier sesión o proyecto, con su perfil y sus cookies.',
    },
    {
      k: '02',
      t: 'Una pestaña por test, con lease',
      d: 'cu tab open URL --lease 300 abre una pestaña en ese Chrome compartido, y el lease la cierra si el test muere, se corta o el agente desaparece. cu context open NAME da un tarro de cookies aislado.',
    },
    {
      k: '03',
      t: 'Snapshots, no capturas',
      d: 'Una línea por elemento interactivo con un ref estable que sobrevive a las acciones. Los iframes y los shadow roots abiertos se recorren también: los componentes web no pintan páginas en blanco.',
    },
    {
      k: '04',
      t: 'Actuar por ref, en lote',
      d: 'click e3, type e2, o un formulario entero con cu act en una llamada que vuelve con la página nueva. cu batch junta muchos comandos en una sola ejecución.',
    },
    {
      k: '05',
      t: 'Texto cuando la respuesta son palabras',
      d: 'cu text devuelve lo que la página dice de verdad: prosa, respuestas de API, mensajes, dentro de frames y shadow roots. Tope de 16 000 caracteres con aviso de corte.',
    },
    {
      k: '06',
      t: 'Descargas vigiladas',
      d: 'Los archivos caen en <data>/downloads (con subcarpetas por contexto), cu downloads los lista y el click que los empezó los nombra en su resultado.',
    },
    {
      k: '07',
      t: 'Las contraseñas no pasan por el modelo',
      d: 'El usuario las escribe en un formulario local en /login; el daemon las teclea en el navegador. Nunca se guardan, registran ni devuelven.',
    },
    {
      k: '08',
      t: 'Skill y tool nativa',
      d: 'SKILL.md enseña a un agente todo el flujo: tests agénticos, user stories, informes de fallo. Y mini-tui expone cu como tool nativa junto a bash, sin servidor MCP por medio.',
    },
  ];

  const cli = [
    ['cu start', 'un daemon por máquina; responde en ~6 ms y el navegador sube en segundo plano'],
    ['cu status --short', 'daemon, navegador y cuántos leases hay abiertos'],
    ['cu tab open URL --lease 300 --label test-1', 'pestaña para este test; el lease la cierra si el test muere'],
    ['cu tab close ID · cu tab renew ID 600', 'cierra la pestaña o alarga su lease'],
    ['cu context open NAME --lease 300', 'cookies aisladas en el mismo Chrome; se usa con --context NAME'],
    ['cu lease', 'leases abiertos y segundos que les quedan'],
    ['cu navigate URL', 'espera a que la página sea usable (máx. 3 s) e informa settled_ms'],
    ['cu snapshot', 'la página para el LLM: url, título, headings y refs (frames y shadow roots)'],
    ['cu text', 'palabras visibles: prosa, respuestas de API, mensajes'],
    ['cu click e3', 'click real con test de impacto; si algo lo tapa, lo dice'],
    ['cu type e2 "Ada" --submit', 'Input.insertText y, si se pide, Enter'],
    ["cu act '[{\"do\":\"type\",\"ref\":\"e2\",\"text\":\"Ada\"},{\"do\":\"click\",\"ref\":\"e5\"}]'", 'lote en una conexión, termina con snapshot'],
    ["cu batch 'snapshot --tab t1' 'click e3 --tab t1'", 'muchos comandos, una llamada; una línea JSON por comando'],
    ['cu shot page.jpg · cu downloads', 'captura JPEG rápida; descargas listadas desde <data>/downloads'],
    ['cu login', 'imprime el enlace /login para que el humano entre'],
    ['cu session save NAME / load NAME', 'guarda o restaura el perfil con la sesión iniciada'],
  ];

  const http = [
    ['GET', '/v1/status', 'estado, navegador y leases'],
    ['POST', '/v1/navigate', '{"url": "..."} · espera asentada'],
    ['GET', '/v1/snapshot', 'página compacta con refs'],
    ['GET', '/v1/text', 'palabras visibles, con - frame: <url>'],
    ['GET', '/v1/screenshot', '?format=png para PNG'],
    ['POST', '/v1/click', '{"ref": "e3"}'],
    ['POST', '/v1/type', '{"ref": "e2", "text": "hi", "submit": true}'],
    ['POST', '/v1/act', '{"actions": [...], "snapshot": true}'],
    ['GET', '/v1/tabs', 'pestañas; el default, marcado'],
    ['DELETE', '/v1/tabs/ID', 'cierra una pestaña'],
    ['GET', '/v1/contexts', 'lista contextos aislados'],
    ['DELETE', '/v1/contexts/NAME', 'cierra un contexto'],
    ['GET', '/v1/downloads', 'archivos descargados, los terminados'],
    ['GET', '/v1/leases', 'leases abiertos y su tiempo restante'],
    ['POST', '/v1/lease', '{"key": "tab:ID", "seconds": 600}'],
    ['POST', '/v1/session/NAME', 'guarda el perfil vivo'],
    ['POST', '/v1/session/NAME/load', 'restaura una sesión guardada'],
    ['GET · POST', '/login', 'formulario humano, sin token'],
  ];

  const actions = [
    ['click', 'ref'],
    ['type', 'ref, text, clear = true, submit'],
    ['press', 'key: Enter, Tab, Escape, flechas…'],
    ['select', 'ref, value o etiqueta'],
    ['navigate', 'url'],
    ['wait', 'ms'],
  ];

  // Comparison tables. Every number comes from docs/BENCHMARKS.md; bars are
  // relative to the worst of each row and the best of each row is highlighted.
  const speedRows = tasks.map((t) => {
    const max = Math.max(...tools.map((x) => t.v[x.id]));
    const min = Math.min(...tools.map((x) => t.v[x.id]));
    return {
      label: t.name,
      sub: t.note,
      cells: tools.map((x) => ({
        text: fmt(t.v[x.id]),
        frac: t.v[x.id] / max,
        best: t.v[x.id] === min,
        color: x.color,
      })),
    };
  });

  const maxTokens = Math.max(...tokens.map((t) => t.tokens));
  const tokenRows = tokens.map((t) => ({
    label: t.tool,
    sub: t.cmd,
    cells: [
      { text: n(t.tokens), frac: t.tokens / maxTokens, best: t.best, color: t.color },
      { text: t.bytes ? n(t.bytes) : '—', frac: null },
      {
        text: t.best ? '1×' : (t.tokens / 350).toLocaleString('es-ES', { maximumFractionDigits: 1 }) + '×',
        frac: null,
      },
    ],
  }));

  const sweepRows = sweep.map((s) => ({
    label: `${s.c} vías`,
    sub: `${s.tests} tests${s.star ? ' *' : ''}`,
    cells: [
      { text: dec(s.tps, s.tps < 1 ? 2 : 1), frac: s.tps / 24.5, best: s.c === 50, color: 'var(--cu)' },
      { text: secs(s.p50), frac: s.p50 / 1.4, color: 'var(--ab)' },
      { text: secs(s.p95), frac: s.p95 / 1.5 },
      { text: dec(s.cpu, 2), frac: s.cpu / 0.35 },
      { text: `${dec(s.rss, 1)} GB`, frac: s.rss / 1.7 },
      { text: `${dec(s.rssCu, 1)} MB`, frac: null },
    ],
  }));

  // Real-site rows: bars are relative to the slowest value of each column of
  // the table; the recommended level (10 flows) is highlighted.
  const realRows = realLevels.map((l) => ({
    label: `${l.c} flujo${l.c === 1 ? '' : 's'}`,
    sub: l.c === 25 ? 'los que no terminaron no cuentan; por encima del precipicio' : l.c === 10 ? 'el nivel recomendado' : 'un flujo solo',
    cells: [
      { text: `${l.ok} / ${l.of}`, frac: l.ok / l.of, best: l.c === 10, color: 'var(--cu)' },
      { text: `${dec(l.p50, 1)} s`, frac: l.p50 / 67.4, color: 'var(--cu)' },
      { text: `${dec(l.p95, 1)} s`, frac: l.p95 / 80.1, color: 'var(--cu)' },
      { text: `${dec(l.ans, 1)} s`, frac: l.ans / 6.9, color: 'var(--ab)' },
      { text: `${l.frac} %`, frac: l.frac / 31, color: 'var(--ab)' },
      { text: dec(l.cores, 2), frac: l.cores / 0.28, color: 'var(--cu-2)' },
      { text: `${dec(l.rss, 1)} GB`, frac: null },
      { text: dec(l.cuCores, 2), frac: null },
    ],
  }));

  const realWaitRows = realWaits.map((w) => ({
    label: w.label,
    sub: w.sub,
    cells: [{ text: w.t, frac: w.frac, best: !!w.best, color: w.best ? 'var(--cu)' : 'var(--ab)' }],
  }));

  const realBlockRows = realBlock.map((b) => ({
    label: b.label,
    cells: [
      { text: b.ok, frac: null },
      { text: `${dec(b.t[0], 1)} s`, frac: b.t[0] / 67.4 },
      { text: `${dec(b.t[1], 1)} s`, frac: b.t[1] / 80.1 },
      { text: `${dec(b.t[2], 1)} s`, frac: b.t[2] / 6.5, color: 'var(--ab)' },
    ],
  }));

  const realVsAbRows = realVsAb.map((v) => ({
    label: v.tool,
    sub: v.mode,
    cells: [
      { text: v.ok, frac: null },
      { text: `${dec(v.p50, 1)} s`, frac: v.p50 / 67.4, color: 'var(--cu)' },
      { text: `${dec(v.p95, 1)} s`, frac: v.p95 / 80.1, color: 'var(--cu)' },
      { text: `${dec(v.open, 1)} s`, frac: v.open / 12.0, best: v.tool === 'cu', color: 'var(--cu-2)' },
      { text: `${dec(v.ans, 1)} s`, frac: v.ans / 9.2, best: v.tool === 'cu', color: 'var(--ab)' },
      { text: `${v.procs}`, frac: v.procs / 113, best: v.tool === 'cu' },
      { text: `${dec(v.rss, 1)} GB`, frac: v.rss / 12.8, best: v.tool === 'cu' },
    ],
  }));
  // Parallel-agent rows. Bars are relative to the worst value of each column of
  // the round. RAM columns are deliberately unscaled (plain values, no bar):
  // the point of the section is that available RAM never fell, and a bar would
  // imply a ceiling that was never reached.
  const parRate = (r) => r.ok / r.of;
  // "safe" = 8-10 in round 2 (100% twice, zero 429s). In round 1 the 10 level
  // was 90%: it worked, but it was already on the edge, so it is not flagged.
  const parRows = (list, with429, safeMax) => list.map((r) => ({
    label: `${r.c} agentes`,
    sub: r.rep ? 'la repetici\u00f3n del nivel, misma configuraci\u00f3n' : undefined,
    cells: [
      { text: `${r.ok} / ${r.of}`, frac: parRate(r), best: r.c <= safeMax, color: 'var(--cu)' },
      { text: `${dec(r.rate, 1)} %`, frac: parRate(r), best: r.c <= safeMax, color: 'var(--cu)' },
      { text: `${dec(r.p50, 1)} s`, frac: r.p50 / Math.max(...list.map((x) => x.p95 || x.p50)), color: 'var(--ab)' },
      { text: r.p95 ? `${dec(r.p95, 1)} s` : '\u2014', frac: r.p95 ? r.p95 / Math.max(...list.map((x) => x.p95 || x.p50)) : null },
      { text: mb(r.avail), frac: null },
      { text: `${dec(r.chrome, 1)} GB`, frac: null },
      { text: `${dec(r.agents, 2)} GB`, frac: null },
      ...(with429
        ? [{ text: `${r.ev429} / ${r.hit429} / ${r.fatal429}`, frac: r.fatal429 / 3, best: r.fatal429 === 0, color: r.fatal429 ? 'var(--pw)' : 'var(--cu)' }]
        : []),
    ],
  }));
  const parRows1 = parRows(parRound1, false, 8);
  const parRows2 = parRows(parRound2, true, 10);

  const parCostRows = parCost.map((c) => ({
    label: c.what,
    sub: c.note,
    cells: [{ text: c.mem, frac: null, best: c.what.startsWith('total'), color: 'var(--cu)' }],
  }));
</script>

<a class="skip" href="#main">Saltar al contenido</a>

<header class="top">
  <div class="wrap bar">
    <a class="logo" href="#top" aria-label="cu, inicio"><span>cu</span></a>
    <nav aria-label="Secciones">
      <a href="#que">Qué es</a>
      <a href="#api">API / CLI</a>
      <a href="#bench">Velocidad</a>
      <a href="#tokens">Tokens</a>
      <a href="#recursos">Recursos</a>
      <a href="#real">Sitio real</a>
      <a href="#par">Agentes</a>
    </nav>
    <span class="local">local · sin nube</span>
  </div>
</header>

<main id="main">
  <section class="hero wrap" id="top">
    <div class="hero-copy">
      <p class="eyebrow">computer use para agentes de IA</p>
      <h1>El navegador que tu agente <em>no tiene que esperar.</em></h1>
      <p class="lede">
        <code>cu</code> mantiene un Chromium vivo detrás de un servidor local y le da al modelo lo que necesita:
        una página en unos cientos de bytes, refs sobre los que actuar y lotes que devuelven el resultado en la
        misma llamada.
      </p>
      <dl class="stats">
        <div><dt>snapshot</dt><dd>3,7<small> ms</small></dd></div>
        <div><dt>10 clicks en lote</dt><dd>25<small> ms</small></dd></div>
        <div><dt>tokens por página</dt><dd>350<small> vs 1 314</small></dd></div>
        <div><dt>tests en paralelo</dt><dd>50<small> en 1 Chrome</small></dd></div>
      </dl>
    </div>

    <div class="term" role="img" aria-label="Ejemplo de sesión de terminal con cu">
      <div class="term-bar"><i></i><i></i><i></i><span>agente@local</span></div>
      <pre><span class="p">$</span> cu navigate https://shop.test
<span class="o">{"{"}"ok":true,"settled_ms":17{"}"}</span>
<span class="p">$</span> cu snapshot
{#each snapshotLines as [a, b]}<span class="k">{a}</span><span class="o">{b}</span>
{/each}<span class="p">$</span> cu click e3
<span class="o">{"{"}"ok":true,"ms":4{"}"}</span>
<span class="p">$</span> <span class="caret"></span></pre>
    </div>
  </section>

  <section class="wrap section" id="que">
    <header class="sec-head">
      <p class="num">01</p>
      <h2>Qué es y para qué sirve</h2>
      <p class="sub">
        Un agente que usa el navegador paga cada acción dos veces: en milisegundos y en tokens. <code>cu</code>
        recorta las dos cosas. Es un binario de Rust con CLI y SDK, un daemon en <code>127.0.0.1</code> con token
        bearer, y un perfil de Chromium que sobrevive entre tareas.
      </p>
    </header>
    <div class="features">
      {#each features as f}
        <article>
          <span class="fk">{f.k}</span>
          <h3>{f.t}</h3>
          <p>{f.d}</p>
        </article>
      {/each}
    </div>

    <div class="flow" aria-label="Ciclo de un agente con cu">
      <div><b>navigate</b><span>espera a que la página esté lista</span></div>
      <i aria-hidden="true">→</i>
      <div><b>snapshot</b><span>refs, sin píxeles</span></div>
      <i aria-hidden="true">→</i>
      <div><b>act</b><span>varias acciones, una llamada</span></div>
      <i aria-hidden="true">→</i>
      <div><b>snapshot</b><span>incluido en la respuesta</span></div>
    </div>
  </section>

  <section class="wrap section" id="api">
    <header class="sec-head">
      <p class="num">02</p>
      <h2>API y CLI</h2>
      <p class="sub">
        Lo mismo por tres caminos: la CLI que usa el agente, la API HTTP que hay debajo y el SDK de Rust.
      </p>
    </header>

    <div class="tabs" role="tablist" aria-label="Interfaz">
      <button role="tab" aria-selected={tab === 'cli'} onclick={() => (tab = 'cli')}>CLI</button>
      <button role="tab" aria-selected={tab === 'http'} onclick={() => (tab = 'http')}>HTTP</button>
      <button role="tab" aria-selected={tab === 'sdk'} onclick={() => (tab = 'sdk')}>SDK Rust</button>
    </div>

    <div class="api-grid">
      <div class="panel" role="tabpanel">
        {#if tab === 'cli'}
          <ul class="cmds">
            {#each cli as [c, d]}
              <li><code>{c}</code><span>{d}</span></li>
            {/each}
          </ul>
          <p class="foot">Los comandos de página aceptan <code>--tab ID</code> o <code>--context NAME</code>.
            <code>cu tabs</code> lista las pestañas (el default, marcado) y <code>cu tabs close ID</code> cierra una.</p>
        {:else if tab === 'http'}
          <ul class="routes">
            {#each http as [m, p, d]}
              <li><span class="m m-{m.split(' ')[0].toLowerCase()}">{m}</span><code>{p}</code><span>{d}</span></li>
            {/each}
          </ul>
          <p class="foot">Todas con <code>Authorization: Bearer</code> salvo <code>/login</code>. Añade
            <code>?tab=ID</code> o <code>?context=NAME</code> a navigate, snapshot, text, screenshot o act;
            pasar los dos a la vez es un error.</p>
        {:else}
          <pre class="code"><span class="k">let</span> cu = cu::Client::new(<span class="s">"127.0.0.1:8787"</span>, token);
println!(<span class="s">"{"{}"}"</span>, cu.status()?);
cu.navigate(<span class="s">"https://example.com"</span>)?;
let page = cu.snapshot()?;
let words = cu.text()?;
cu.click(<span class="s">"e3"</span>)?;
cu.act(<span class="s">r#"[{"{"}"do":"type","ref":"e2","text":"Ada"{"}"}]"#</span>)?;
cu.save_session(<span class="s">"example"</span>)?;</pre>
          <p class="foot">El token vive en <code>.cu/server.json</code>; mantenlo privado. Además:
            <code>tabs</code>, <code>close_tab</code>, <code>downloads</code> y <code>load_session</code>.</p>
        {/if}
      </div>

      <aside class="side">
        <div class="mini">
          <h3>Lotes con <code>cu act</code></h3>
          <pre class="code small">{`{"actions":[
  {"do":"type","ref":"e2","text":"Ada"},
  {"do":"select","ref":"e4","value":"Medium"},
  {"do":"click","ref":"e5"}
]}`}</pre>
          <table class="kv">
            <tbody>
              {#each actions as [a, b]}
                <tr><th><code>{a}</code></th><td>{b}</td></tr>
              {/each}
            </tbody>
          </table>
          <p class="foot">Si algo falla: <code>"ok":false</code>, el índice que falló y por qué. Un ref caducado
            pide un snapshot nuevo; un elemento tapado dice qué lo tapa.</p>
        </div>
      </aside>
    </div>

    <div class="trio">
      <article class="mini">
        <h3>Coste por acción</h3>
        <table class="kv">
          <tbody>
            {#each actionCosts as a}
              <tr><td>{a.what}</td><th class="r"><code>{a.ms}</code></th></tr>
            {/each}
          </tbody>
        </table>
      </article>
      <article class="mini">
        <h3>Contextos aislados</h3>
        <p class="txt">Cookies, almacenamiento y caché propios dentro del mismo Chromium.</p>
        <table class="kv">
          <thead><tr><td></td><th class="r">memoria</th><th class="r">listo en</th></tr></thead>
          <tbody>
            {#each contexts as c}
              <tr><td>{c.what}</td><th class="r"><code>{c.mem}</code></th><th class="r"><code>{c.ready}</code></th></tr>
            {/each}
          </tbody>
        </table>
      </article>
      <article class="mini">
        <h3>Sesiones y login seguro</h3>
        <p class="txt"><code>cu login</code> da un enlace a <code>/login</code>: el humano escribe, el daemon teclea
          en la página y envía. Después, <code>cu session save NAME</code> guarda el perfil con la sesión iniciada y
          <code>load</code> la recupera tras reiniciar.</p>
      </article>
    </div>
  </section>

  <section class="wrap section" id="bench">
    <header class="sec-head">
      <p class="num">03</p>
      <h2>Velocidad contra agent-browser y Playwright MCP</h2>
      <p class="sub">
        Mismo binario (Chrome for Testing headless shell 153), mismo sitio local y las mismas seis tareas de agente.
        <code>cu</code> y agent-browser por CLI, un proceso por acción; Playwright MCP por stdio JSON-RPC. Mediana
        de 10 rondas en orden rotatorio, en ms. Menos es mejor.
      </p>
    </header>

    <ul class="legend" aria-label="Herramientas comparadas">
      {#each tools as t}
        <li><i style="background:{t.color}"></i>{t.label}</li>
      {/each}
    </ul>

    <CompareTable
      caption="Tareas de agente · mediana de 10 rondas, en ms · menos es mejor · la mejor de cada fila, resaltada"
      first="tarea"
      columns={tools.map((t) => ({ label: t.label }))}
      rows={speedRows}
    />

    <ul class="fair">
      <li><code>cu</code> y Playwright MCP esperan la navegación que provoca una acción; agent-browser vuelve antes,
        así que su formulario y login incluyen el <code>wait --url</code> + <code>wait --load</code> que un agente
        tendría que añadir.</li>
      <li>Playwright MCP genera refs nuevos tras cada acción, así que sus “10 clicks” leen un snapshot antes de cada
        uno; los ~550 ms por click son su propio asentamiento. Los refs de <code>cu</code> sobreviven.</li>
      <li>El coste de lanzar un proceso CLI está incluido en las columnas de <code>cu</code> y agent-browser.</li>
    </ul>

    <div class="speed">
      <div class="speed-head">
        <h3><code>cu</code> contra sí mismo</h3>
        <p>Antes y después del trabajo de velocidad, misma máquina cargada, mediana de 20 ejecuciones.</p>
      </div>
      {#each speed as s}
        <div class="sp">
          <span class="sp-name"><code>{s.action}</code></span>
          <div class="sp-bars">
            <span class="b before" style="width:{(s.before / maxSpeed) * 100}%"><em>{n(s.before)} ms</em></span>
            <span class="b after" style="width:{(s.after / maxSpeed) * 100}%"><em>{n(s.after)} ms</em></span>
          </div>
          <span class="sp-note">{s.note}</span>
        </div>
      {/each}
      <div class="res">
        <table class="kv">
          <thead><tr><td>navegador, misma carga</td><th class="r">antes</th><th class="r">después</th></tr></thead>
          <tbody>
            {#each resources as r}
              <tr><td>{r.what}</td><td class="r dimc">{r.before}</td><th class="r"><code>{r.after}</code></th></tr>
            {/each}
          </tbody>
        </table>
      </div>
    </div>
  </section>

  <section class="wrap section" id="tokens">
    <header class="sec-head">
      <p class="num">04</p>
      <h2>Tokens del snapshot</h2>
      <p class="sub">
        La misma página de catálogo de 2,3 KB (12 enlaces, 10 productos con botón, buscador y pie), contada con
        tiktoken <code>o200k_base</code>. Es lo que paga el modelo cada vez que mira la página.
      </p>
    </header>

    <CompareTable
      caption="Snapshot de la misma página · menos es mejor · la mejor de la fila, resaltada"
      first="herramienta"
      columns={[{ label: 'tokens' }, { label: 'bytes' }, { label: 'vs cu' }]}
      rows={tokenRows}
    />

    <p class="big-quote">
      <span>3,8×</span> menos tokens que Playwright MCP por cada vistazo a la página: una línea por elemento
      interactivo, sin marcado de presentación ni nodos de texto, con tope de 200 nodos y 120 caracteres por nombre.
    </p>
  </section>

  <section class="wrap section" id="recursos">
    <header class="sec-head">
      <p class="num">05</p>
      <h2>Recursos y concurrencia</h2>
      <p class="sub">
        El modelo que un CI quiere: <strong>un Chrome siempre abierto</strong>, y cada test abre su propia pestaña
        con lease en ese Chrome y la cierra al terminar (si el test muere, el lease la cierra). ¿Cuánto cuesta
        cuando 50 tests corren a la vez, y dónde satura?
      </p>
    </header>

    <dl class="stats wide">
      {#each sweepHeadline as h}
        <div><dt>{h.d}</dt><dd>{h.k}</dd></div>
      {/each}
    </dl>

    <CompareTable
      caption="De 5 a 50 tests simultáneos, una pestaña con lease cada uno · menos es mejor salvo tests/s"
      first="concurrencia"
      columns={[
        { label: 'tests/s' },
        { label: 'flujo p50' },
        { label: 'flujo p95' },
        { label: 'CPU Chrome' },
        { label: 'RSS Chrome' },
        { label: 'RSS cu' },
      ]}
      rows={sweepRows}
      bestSr="el mayor caudal de la tabla"
      note="* La oleada de 5 vías se encontró una llamada colgada hasta el timeout de 60 s del harness (una flujo de menos registrado); el resto de niveles, 0 errores. Pico real de la corrida: 50/50 en verde, 2,1 s de pared, 1,84 GB de RSS de Chrome (11 procesos), 11 MB para el daemon de cu y 71 % de máquina ocupada."
    />

    <ul class="fair">
      <li><strong>Chrome no es el cuello de botella.</strong> Con 50 pestañas usa menos de 0,4 de 12 cores y
        1,8 GB de 64 GB: Chrome es el recurso que se comparte, no el límite.</li>
      <li><strong>El caudal se aplana hacia los 25-35 flujos simultáneos</strong> (tests/s: 12 → 19 → 23 → 24)
        mientras la latencia por flujo crece casi lineal con la concurrencia (p50: 0,29 s → 1,40 s de 10 a 50).
        Satura primero la parte serializada del camino — los viajes de ida y vuelta por loopback y el lanzamiento
        de un proceso <code>cu</code> por llamada —, no la CPU del renderer.</li>
      <li><strong>~18 MB por pestaña abierta</strong> para una página JS de este tamaño (0,8 GB con pocas →
        1,8 GB con 50). Nada frente a 64 GB.</li>
      <li><strong>Recomendado:</strong> 8 tests simultáneos por defecto, 12 como tope interactivo y hasta 50 en un
        lote sin supervisión (medido: 100/100 en verde). Los leases hacen que la pila se limpie sola.</li>
    </ul>

  </section>

  <section class="wrap section" id="real">
    <header class="sec-head">
      <p class="num">06</p>
      <h2>Contra una app real de producción</h2>
      <p class="sub">
        Todo lo anterior corre contra páginas locales. Esto corre los flujos que un agente de QA ejecuta de verdad
        contra <strong>una app web real de producción en build de desarrollo</strong> (servidor dev: una petición por
        módulo, el navegador admite ~6 sockets por origen), por la red, con sesión iniciada y una pestaña con lease
        por flujo en el Chrome compartido. Medianas p50/p95 de los flujos de cada nivel, sobre una caja compartida de
        12 cores con otros agentes corriendo.
      </p>
    </header>

    <p class="flow-desc">
      El flujo: abrir una pestaña en una ruta profunda → iniciar sesión si la app dice que no hay sesión → navegar a
      una sección y una subruta → abrir el chat del asistente, conversación nueva, enviar un marcador y esperar a que
      ese marcador aparezca <strong>dos veces</strong> en el texto (el eco del mensaje más la respuesta en streaming) →
      cerrar la pestaña. Sin esperas por reloj: cada <code>wait</code> es por texto, estado o URL.
    </p>

    <dl class="stats wide">
      {#each realHeadline as h}
        <div><dt>{h.d}</dt><dd>{h.k}</dd></div>
      {/each}
    </dl>

    <CompareTable
      caption="1, 10 y 25 flujos en paralelo, sesión compartida · menos es mejor salvo los que llevan · el nivel recomendado, resaltado"
      first="flujos"
      columns={[
        { label: 'ok' },
        { label: 'total p50' },
        { label: 'total p95' },
        { label: 'respuesta p50' },
        { label: '% del flujo' },
        { label: 'CPU Chrome' },
        { label: 'RSS Chrome' },
        { label: 'cores cu' },
      ]}
      rows={realRows}
      bestSr="mejor de la fila"
      note="Chrome usa 0,15-0,28 de los 12 cores; los ~6 GB de RSS son el navegador compartido entero (pestañas de otros agentes incluidas), ~18 MB por pestaña. El daemon de cu: 0,01-0,03 cores y ~2 MB."
    />

    <ul class="fair">
      <li><strong>Recomendado: hasta 10 flujos por Chrome compartido</strong> contra este despliegue — ~90 % de
        éxito, p50 ~67 s por flujo de los cuales ~6,5 s son la respuesta del asistente, y Chrome por debajo de un
        tercio de core. Cuenta con p95 ~80 s y un reintento cuando «la app no llegó a pintar».</li>
      <li><strong>A 25 flujos solo terminaron 3 de 25, y es límite de la app, no de cu.</strong> Los otros 22 se
        quedaron en el shell vacío sin renderizar durante 60 s, reproducido dos veces: un build de desarrollo arranca
        en frío con una petición por módulo, así que 25 arranques en frío se encolan en minutos. Las mismas 25
        pestañas con lease contra las páginas locales de arriba fueron 100/100 en verde. Para subir de ahí: escalonar
        la primera navegación (~1-2 s), un build de producción de la app, o repartir entre navegadores/orígenes.</li>
      <li><strong>Chrome no es el cuello de botella.</strong> El servidor de la app y su presupuesto de sockets por
        origen sí lo son.</li>
      <li><strong>El login es barato cuando el anti-bot coopera y durísimo cuando no.</strong> En frío: 0,75-1,5 s;
        con el captcha throttleado, los mismos 30-120 s, y los logins en paralelo se pisan (8 sesiones volvieron al
        login a mitad de flujo en la tanda de 25; agent-browser completó 0/20 en 120 s así). Escalona los logins
        reales; comparte sesión en las corridas de concurrencia.</li>
      <li><strong>La respuesta es la única fase que no se encola:</strong> ~5-7 s p50 con 1, 10 y 25 flujos alike
        (p95 ~20 s a 10). Su peso en el flujo baja (31 % con un flujo → ~12 % a 10-25) porque lo que crece es la
        contención de la página, no la respuesta. Timeouts de 60-120 s cubren el p95.</li>
    </ul>

    <h3 class="sub-h">Palanca 1 · esperar por texto, nunca por networkidle</h3>
    <p class="lever">
      La app mantiene dos streams de eventos abiertos toda su vida (una lista en vivo y un stream de conversaciones),
      así que <code>networkidle</code> no se alcanza nunca. Un flujo instrumentado:
    </p>
    <CompareTable
      caption="Qué espera el flujo y cuánto tarda · el texto gana, la red nunca se calma"
      first="espera"
      columns={[{ label: 'medido' }]}
      rows={realWaitRows}
      bestSr="la espera que funciona"
    />

    <h3 class="sub-h">Palanca 2 · bloquear imágenes, fuentes y media: aquí no es la palanca</h3>
    <CompareTable
      caption="Nivel 10, dos olas, contra el mismo baseline · el peso de esta app son módulos JS y la cola de sockets, no media"
      first="configuración"
      columns={[{ label: 'ok' }, { label: 'total p50' }, { label: 'total p95' }, { label: 'respuesta p50' }]}
      rows={realBlockRows}
    />

    <h3 class="sub-h">Los mismos flujos por agent-browser (10 flujos)</h3>
    <CompareTable
      caption="Mismos pasos, misma caja, un navegador entero por sesión de agent-browser · la forma de la comparación es arquitectónica"
      first="herramienta"
      columns={[
        { label: 'ok' },
        { label: 'total p50' },
        { label: 'total p95' },
        { label: 'apertura p50' },
        { label: 'respuesta p50' },
        { label: 'procesos' },
        { label: 'RSS máx' },
      ]}
      rows={realVsAbRows}
    />

    <ul class="fair">
      <li>Los totales son comparables porque a 10 flujos cada herramienta gasta el tiempo en otro sitio: <code>cu</code>
        hace cola en el presupuesto de sockets por origen de la app pero sus llamadas por paso siguen siendo rápidas
        (apertura 0,1 s), mientras diez sesiones de agent-browser arrancan la app en frío sin caché compartida
        (apertura 12 s, 30 s de boot cada una) pero tienen diez presupuestos de sockets independientes.</li>
      <li>Los dos esperan por texto; la respuesta son 6,5 s (<code>cu</code>, sondeo de texto) contra 9,2 s
        (agent-browser, sondeo de snapshot, más grueso). Dos flujos de cada corrida los perdió el mismo flake de
        arranque de la app.</li>
      <li><code>cu</code> mete todas las pestañas en un Chrome (27 procesos a 10 flujos); agent-browser paga un
        navegador completo por sesión (113 procesos, el doble de RAM). Con logins en frío — cada sesión entrando por su
        cuenta — agent-browser terminó <strong>0 de 20</strong> en 120 s; <code>cu</code> con una sesión compartida
        sostiene 18 de 20.</li>
    </ul>

    <h3 class="sub-h">Lo que estas corridas dicen de cu</h3>
    <ul class="fair">
      <li><strong>Un comando de página enviado a través de una navegación client-side puede colgarse para siempre.</strong>
        El daemon espera sin deadline y filtra un hilo y una conexión cada vez; el harness reintentó y terminó. Un
        deadline por comando CDP pertenece a <code>cu</code>.</li>
      <li><strong>Los refs mueren si la app re-renderiza entre el snapshot y el click</strong> («could not locate eN»):
        snapshot nuevo y reintento; escribir en el composer también mata el ref del botón de enviar, así que se envía
        con type + Enter.</li>
      <li><strong><code>cu act</code> sale con código 0 y cuerpo <code>{"{"}"ok":false{"}"}</code></strong> — hay que leer el
        cuerpo.</li>
    </ul>
  </section>

  <section class="wrap section" id="par">
    <header class="sec-head">
      <p class="num">07</p>
      <h2>Agentes de IA en paralelo</h2>
      <p class="sub">
        Todo lo anterior son flujos guionizados. Esto son <strong>agentes de IA de verdad</strong>: cada "agente" es una
        sesión headless de <code>mini-tui</code> con <strong>MiniMax-M3.1-Flash-Preview</strong>, que recibe los comandos
        exactos de <code>cu</code> y decide el flujo el mismo: abrir <strong>su propio contexto</strong>
        <code>cu</code> (tarro de cookies aislado dentro del <em>único</em> Chrome compartido), navegar, iniciar sesión si
        hace falta, abrir el chat del asistente, conversación nueva, enviar un marcador y esperar la respuesta
        <strong>por texto</strong> (el marcador aparece dos veces: el eco más la respuesta en streaming), validar y cerrar
        su contexto. Aquí se pagan la latencia del modelo, un viaje de ida y vuelta por bash por paso y una sesión
        completa de agente, encima del navegador. Objetivo: el <strong>chat Sage de neural-dev</strong>, una app real de
        producción con build estático detrás de nginx/Cloudflare. Caja de 12 cores y 64 GB, con 8 GB de swap ya ocupados
        antes de empezar (presión ajena a la prueba).
      </p>
    </header>

    <p class="flow-desc">
      Vigilancia en todas las corridas: <code>MemAvailable</code>, swap usado, RSS del Chrome compartido y RSS de los
      procesos agente, muestreados cada 5 s, con un <strong>suelo duro de 3 GB</strong> que habría matado por PID
      (SIGTERM y luego SIGKILL) sólo a los procesos lanzados por la prueba. El suelo no se acercó ni una vez.
    </p>

    <dl class="stats wide">
      {#each parHeadline as h}
        <div><dt>{h.d}</dt><dd>{h.k}</dd></div>
      {/each}
    </dl>

    <h3 class="sub-h">Ronda 1 &middot; de 1 a 12 agentes</h3>
    <CompareTable
      caption="Barrido de 1 a 12 agentes reales · p50/p95 por agente, cronómetro del harness · todo medido"
      first="agentes"
      columns={[
        { label: 'ok' },
        { label: 'tasa' },
        { label: 'p50' },
        { label: 'p95' },
        { label: 'RAM libre mín.' },
        { label: 'RSS Chrome' },
        { label: 'RSS agentes' },
      ]}
      rows={parRows1}
      bestSr="nivel sin 429 y 100 %"
      note="Los niveles de 1 a 6 vienen de un barrido cuyo cronómetro recogía los agentes terminados en serie, así que sus latencias son suelos, no medianas; el pass/fail y la RAM valen en todas las filas. Las filas de 10 y 12 son la repetición corregida, con un hilo por agente cronometrando su propia salida."
    />

    <h3 class="sub-h">Ronda 2 &middot; de 8 a 16 agentes, con contador de 429</h3>
    <p class="lever">
      Se repitió porque se había reiniciado el plan de tokens del modelo. Bajó los 429 a cero en 8 y 10 y subió el éxito
      en los niveles ya medidos, pero el mismo 429 volvió en la repetición de 12, en 14 y en 16. Reiniciar el plan ensancha
      el margen; no quita el techo.
    </p>
    <CompareTable
      caption="De 8 a 16 agentes reales, tras el reinicio de tokens · el mismo Chrome para todos · todo medido"
      first="agentes"
      columns={[
        { label: 'ok' },
        { label: 'tasa' },
        { label: 'p50' },
        { label: 'p95' },
        { label: 'RAM libre mín.' },
        { label: 'RSS Chrome' },
        { label: 'RSS agentes' },
        { label: '429 ev/afect/fat' },
      ]}
      rows={parRows2}
      bestSr="nivel sin 429 y 100 %"
      note="429 ev/afect/fat = eventos de 429 distintos / agentes que vieron al menos uno / fallos fatales. La regla dura de parada (tasa &lt; 80 %) se disparó en el nivel 16: 75,0 %; la condición de RAM ni se acercó. Torneos totales de la ronda: 18 eventos, 7 agentes afectados, 6 fallos fatales."
    />

    <h3 class="sub-h">El cuello de botella no fue la RAM</h3>
    <ul class="fair">
      <li>
        <strong>Todos los fallos menos uno fueron el mismo 429 del proveedor de la app</strong>, devuelto por Sage
        dentro de la conversación:
      </li>
    </ul>
    <pre class="quote429">{par429}</pre>
    <ul class="fair">
      <li>
        Ronda 1: 3 fallos, los 3 de 429. Ronda 2: 7 fallos, 6 de 429 y 1 agente que se interrumpió él mismo antes de
        cerrar su ventana de 180 s (el harness lo cuenta como fallo, pero es una decisión del agente, no capacidad).
        Clasificación de los diez fallos de las dos rondas: <strong>9 por 429 del proveedor, 1 auto-interrupción, 0 por
        la UI, el login, <code>cu</code> o el navegador</strong>. La navegación, el login, la conversación nueva y el
        marcador funcionaron todas las veces.
      </li>
      <li>
        <strong>Los 429 fatales crecen con el nivel: 0, 0, 0, 1, 2, 3</strong> en 8, 10, 12, 12b, 14 y 16. Lo que se
        agota son las credenciales LLM del gateway de la app por concentración, y eso no se arregla desde el
        navegador.
      </li>
      <li>
        <strong>La RAM nunca fue el límite.</strong> La memoria disponible nunca bajó de 20,2 GB (ronda 2) ni de
        21,9 GB (ronda 1): más de 6 veces el suelo de 3 GB. El delta de swap fue ~0 en todos los niveles.
      </li>
    </ul>

    <h3 class="sub-h">Coste marginal por agente</h3>
    <CompareTable
      caption="Medido sobre las tablas de arriba · el Chrome compartido es el consumidor dominante y es compartido"
      first="qué"
      columns={[{ label: 'RAM por agente' }]}
      rows={parCostRows}
      bestSr="el total"
    />

    <h3 class="sub-h">Límite seguro recomendado</h3>
    <ul class="fair">
      <li><strong>8-10 agentes en paralelo: seguro.</strong> 100 % dos veces en cada nivel, cero 429 en la ronda 2, p50 76-122 s, RAM de sobra.</li>
      <li><strong>12: el límite.</strong> 100 % en la primera repetición y 91,7 % en la segunda (1 429 fatal). Aceptable si se acepta reintentar los 429; p95 ~200 s, por debajo del umbral de 2x.</li>
      <li><strong>14 o más: no recomendado</strong> con este proveedor. 85,7 % y 75,0 %, 2-3 429 fatales cada nivel y p95 de 368-374 s (&gt;2x el p95 de 117,6 s con un solo agente).</li>
      <li><strong>Para pasar de ~12 simultáneos:</strong> escalonar los lanzamientos en oleadas de 8-10, o subir la concurrencia de credenciales/modelos del gateway LLM de la app. La RAM no es lo que frena.</li>
      <li><strong>Timeout por agente: 420 s</strong>, que cubre el p95 más alto observado (~374 s) con margen; 300 s deja de bastar a 14-16.</li>
    </ul>

    <h3 class="sub-h">Medido contra extrapolado</h3>
    <ul class="fair">
      <li>
        Los tres datos anteriores son <strong>medidos</strong>, nivel a nivel, en esas corridas. Una extrapolación lineal
        de ~0,3 GB por agente diría que 64 GB darían para muchas veces más: eso es aritmética, no evidencia, y esta
        página no lo afirma.
      </li>
      <li>
        <strong>La contención de CPU y del Chrome compartido nunca se aisló de los reintentos por 429.</strong> Un agente
        que recibe un 429 reintenta, y un agente reintentando no está ocioso, así que la columna de latencia mezcla
        backoff del proveedor con contención local. No se midió nada por encima de 16 agentes, y 16 ya falló la regla
        del 80 %.
      </li>
      <li>
        Con n &le; 16, el "p95" del harness es siempre el máximo (<code>int(0.95n) = n-1</code>), así que aquí "p95" se
        lee como "el agente más lento" y depende más de la suerte de un agente concreto que de la concurrencia. Por eso
        se registró como aviso y no como parada.
      </li>
      <li>
        El <code>seconds=</code> que reporta cada agente es su propia estimación del tiempo de Sage, inconsistente
        (3 s, 132 s y 27 s en el mismo nivel): se recoge, pero no se usa como métrica. La latencia de referencia es el
        reloj del harness.
      </li>
      <li>
        El gateway de MiniMax <strong>no devuelve uso</strong>: <code>prompt_tokens</code> y
        <code>completion_tokens</code> salen a 0 y <code>cost_usd</code> a 0,0 en todas las corridas, así que el coste en
        tokens <strong>no es medible con esta configuración</strong> y aquí no se inventa ninguna cifra. Lo que sí se mide:
        ~14-16 pasos de herramienta y ~15-16 llamadas al modelo por agente.
      </li>
    </ul>
  </section>
</main>

<footer class="wrap foot-site">
  <p><code>cu</code> corre solo en local, sin nube detrás. Datos de <code>docs/BENCHMARKS.md</code>; reproducibles con
    <code>python3 bench/compare.py --iters 10</code>. Código en
    <a href="https://github.com/jaivial/cu">github.com/jaivial/cu</a>.</p>
</footer>

<style>
  .wrap { width: min(var(--w), 100% - 2.5rem); margin-inline: auto; }

  /* Off-screen until focused, but kept inside the viewport box on purpose: the
     usual `left: -999px` parks it in negative overflow, which is what some
     browsers turn into a leftward-scrollable page. Clipping a 1x1 box at the
     origin hides it just as well and can never add scroll. */
  .skip {
    position: absolute; left: 0; top: 0; width: 1px; height: 1px;
    overflow: hidden; clip: rect(0 0 0 0); clip-path: inset(50%); white-space: nowrap;
  }
  .skip:focus {
    left: 1rem; top: 1rem; width: auto; height: auto; overflow: visible;
    clip: auto; clip-path: none; z-index: 10; background: var(--cu); color: #000; padding: 0.4rem 0.7rem;
  }

  /* header */
  .top {
    position: sticky; top: 0; z-index: 5;
    background: color-mix(in srgb, var(--bg) 82%, transparent);
    backdrop-filter: blur(10px);
    border-bottom: 1px solid var(--line);
  }
  .bar { display: flex; align-items: center; gap: 2rem; height: 58px; }
  .logo { text-decoration: none; }
  .logo span {
    display: inline-grid; place-items: center; width: 34px; height: 34px; border-radius: 8px;
    background: var(--cu); color: #0b0c0b; font-family: var(--mono); font-weight: 800; font-size: 0.95rem;
  }
  nav { display: flex; gap: 1.5rem; font-size: 0.9rem; }
  nav a { color: var(--dim); text-decoration: none; }
  nav a:hover { color: var(--fg); }
  .local {
    margin-left: auto; font-family: var(--mono); font-size: 0.72rem; color: var(--dim);
    border: 1px solid var(--line); padding: 0.2rem 0.55rem; border-radius: 99px;
  }

  /* hero */
  .hero {
    display: grid; grid-template-columns: 1.15fr 1fr; gap: 3.5rem; align-items: center;
    padding: 6rem 0 5rem;
  }
  .eyebrow {
    font-family: var(--mono); font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.14em;
    color: var(--cu); margin: 0 0 1.4rem;
  }
  h1 {
    font-size: clamp(2.6rem, 5.6vw, 4.6rem); line-height: 0.98; letter-spacing: -0.035em;
    font-weight: 700; margin: 0; text-wrap: balance;
  }
  h1 em {
    font-family: var(--serif); font-style: italic; font-weight: 400; letter-spacing: -0.01em;
    color: var(--cu);
  }
  .lede { color: var(--dim); font-size: 1.12rem; max-width: 34rem; margin: 1.6rem 0 2.2rem; }
  .lede code, .sub code { color: var(--fg); }
  .stats { display: flex; gap: 2.4rem; margin: 0; flex-wrap: wrap; }
  .stats div { border-left: 2px solid var(--cu); padding-left: 0.9rem; }
  .stats dt { font-size: 0.78rem; color: var(--dim); }
  .stats dd { margin: 0; font-family: var(--mono); font-size: 1.6rem; font-weight: 600; letter-spacing: -0.03em; }
  .stats small { font-size: 0.8rem; color: var(--dim); font-weight: 400; letter-spacing: 0; }
  .stats.wide { margin: 0 0 2.2rem; gap: 3rem; }
  .stats.wide dd { font-size: 1.9rem; }

  .term {
    background: #0e100e; border: 1px solid var(--line); border-radius: 14px; overflow: hidden;
    box-shadow: 0 40px 80px -40px rgba(198, 243, 107, 0.18), 0 0 0 1px rgba(255,255,255,0.02) inset;
  }
  .term-bar {
    display: flex; align-items: center; gap: 6px; padding: 0.7rem 0.9rem;
    border-bottom: 1px solid var(--line); background: var(--panel);
  }
  .term-bar i { width: 10px; height: 10px; border-radius: 50%; background: #2a2e2a; }
  .term-bar span { margin-left: auto; font-family: var(--mono); font-size: 0.7rem; color: var(--faint); }
  .term pre {
    margin: 0; padding: 1.2rem 1.3rem 1.4rem; font-size: 0.8rem; line-height: 1.7; overflow-x: auto;
    color: var(--fg);
  }
  .p { color: var(--cu); }
  .o { color: var(--dim); }
  .k { color: var(--cu-2); }
  .s { color: var(--ab); }
  .caret {
    display: inline-block; width: 8px; height: 1em; vertical-align: -2px; background: var(--cu);
    animation: blink 1.1s steps(1) infinite;
  }
  @keyframes blink { 50% { opacity: 0; } }

  /* sections */
  .section { padding: 5.5rem 0 1rem; border-top: 1px solid var(--line); }
  .sec-head {
    display: grid; grid-template-columns: 4rem 1fr; column-gap: 1rem; margin-bottom: 2.6rem;
  }
  .num { grid-row: span 2; margin: 0.55rem 0 0; font-family: var(--mono); color: var(--faint); font-size: 0.85rem; }
  h2 {
    margin: 0; font-size: clamp(1.9rem, 3.6vw, 2.8rem); letter-spacing: -0.03em; line-height: 1.05; font-weight: 650;
  }
  .sub { margin: 0.9rem 0 0; color: var(--dim); max-width: 46rem; }

  .features { display: grid; grid-template-columns: repeat(4, 1fr); gap: 1px; background: var(--line);
    border: 1px solid var(--line); border-radius: 14px; overflow: hidden; }
  .features article { background: var(--bg); padding: 1.6rem 1.4rem 1.8rem; }
  .fk { font-family: var(--mono); font-size: 0.75rem; color: var(--cu); }
  .features h3 { margin: 0.8rem 0 0.6rem; font-size: 1.12rem; letter-spacing: -0.015em; line-height: 1.2; }
  .features p { margin: 0; color: var(--dim); font-size: 0.92rem; }

  .flow {
    margin-top: 2rem; display: flex; align-items: stretch; gap: 0.6rem; flex-wrap: wrap;
  }
  .flow div {
    flex: 1 1 10rem; padding: 0.9rem 1rem; border: 1px solid var(--line); border-radius: 10px;
    background: var(--panel); display: flex; flex-direction: column; gap: 0.15rem;
  }
  .flow b { font-family: var(--mono); font-weight: 600; color: var(--cu); font-size: 0.92rem; }
  .flow span { font-size: 0.82rem; color: var(--dim); }
  .flow i { align-self: center; color: var(--faint); font-style: normal; }

  /* api */
  .tabs { display: inline-flex; gap: 0.25rem; padding: 0.25rem; border: 1px solid var(--line);
    border-radius: 10px; margin-bottom: 1rem; background: var(--panel); }
  .tabs button {
    font: inherit; font-size: 0.85rem; color: var(--dim); background: none; border: 0;
    padding: 0.4rem 0.9rem; border-radius: 7px; cursor: pointer;
  }
  .tabs button[aria-selected='true'] { background: var(--fg); color: var(--bg); }
  .api-grid { display: grid; grid-template-columns: minmax(0, 1.5fr) minmax(0, 1fr); gap: 1rem; }
  .panel, .mini {
    background: var(--panel); border: 1px solid var(--line); border-radius: 14px; padding: 1.3rem 1.4rem;
    min-width: 0;
  }
  .cmds, .routes { list-style: none; padding: 0; margin: 0; }
  .cmds li {
    display: grid; grid-template-columns: minmax(0, 1fr); gap: 0.15rem; padding: 0.7rem 0;
    border-bottom: 1px solid var(--line);
  }
  .cmds li:last-child, .routes li:last-child { border-bottom: 0; }
  .cmds code { color: var(--fg); overflow-wrap: anywhere; }
  .cmds code::before { content: '$ '; color: var(--cu); }
  .cmds span, .routes span:last-child { color: var(--dim); font-size: 0.86rem; }
  .routes li {
    display: grid; grid-template-columns: 5.2rem minmax(10rem, 13rem) 1fr; gap: 0.8rem; align-items: baseline;
    padding: 0.55rem 0; border-bottom: 1px solid var(--line);
  }
  .routes code { color: var(--fg); overflow-wrap: anywhere; }
  .m { font-family: var(--mono); font-size: 0.7rem; font-weight: 700; letter-spacing: 0.04em; }
  .m-get { color: var(--cu-2); }
  .m-post { color: var(--ab); }
  .m-delete { color: #f27b7b; }
  .foot { color: var(--dim); font-size: 0.84rem; margin: 1rem 0 0; }
  .code {
    margin: 0; font-size: 0.82rem; line-height: 1.7; background: #0e100e; border: 1px solid var(--line);
    border-radius: 10px; padding: 1rem 1.1rem; overflow-x: auto;
  }
  .code.small { font-size: 0.76rem; }
  .mini h3 { margin: 0 0 0.9rem; font-size: 1.02rem; letter-spacing: -0.01em; }
  .txt { color: var(--dim); font-size: 0.9rem; margin: 0 0 0.8rem; }
  .txt code { color: var(--fg); }
  .kv { width: 100%; border-collapse: collapse; font-size: 0.86rem; margin-top: 0.9rem; }
  .kv th, .kv td { text-align: left; padding: 0.45rem 0; border-bottom: 1px solid var(--line); vertical-align: top; }
  .kv tr:last-child th, .kv tr:last-child td { border-bottom: 0; }
  .kv td { color: var(--dim); }
  .kv th { font-weight: 500; padding-right: 0.8rem; }
  .kv thead th, .kv thead td { font-size: 0.72rem; color: var(--faint); font-weight: 400; text-transform: uppercase; letter-spacing: 0.08em; }
  .kv .r { text-align: right; padding-left: 0.8rem; padding-right: 0; }
  .kv th code { color: var(--cu); }
  .dimc { color: var(--faint) !important; }
  .trio { display: grid; grid-template-columns: repeat(3, 1fr); gap: 1rem; margin-top: 1rem; }

  /* benchmarks */
  .legend { list-style: none; padding: 0; margin: 0 0 1.2rem; display: flex; gap: 1.2rem; flex-wrap: wrap; font-size: 0.85rem; color: var(--dim); }
  .legend li { display: flex; align-items: center; gap: 0.45rem; }
  .legend i { width: 12px; height: 12px; border-radius: 3px; }

  .fair { margin: 1.6rem 0 0; padding: 0 0 0 1.1rem; color: var(--dim); font-size: 0.88rem; display: grid; gap: 0.5rem; max-width: 52rem; }
  .fair code { color: var(--fg); }
  .fair strong { color: var(--fg); font-weight: 600; }

  .speed { margin-top: 3rem; padding: 1.6rem; border: 1px solid var(--line); border-radius: 14px; background: var(--panel); }
  .speed-head h3 { margin: 0; font-size: 1.15rem; }
  .speed-head p { margin: 0.3rem 0 1.4rem; color: var(--dim); font-size: 0.88rem; }
  .sp { display: grid; grid-template-columns: 11rem 1fr 14rem; gap: 1rem; align-items: center; padding: 0.7rem 0; border-top: 1px solid var(--line); }
  .sp-name code { color: var(--fg); }
  .sp-bars { display: grid; gap: 4px; padding-right: 5.5rem; }
  .b { display: block; height: 18px; border-radius: 4px; position: relative; min-width: 2px; }
  .b em { position: absolute; left: calc(100% + 0.5rem); top: 50%; transform: translateY(-50%);
    font-style: normal; font-family: var(--mono); font-size: 0.72rem; white-space: nowrap; }
  .before { background: #343a33; }
  .before em { color: var(--faint); }
  .after { background: var(--cu); }
  .after em { color: var(--cu); }
  .sp-note { font-size: 0.8rem; color: var(--dim); text-align: right; }
  .res { margin-top: 1rem; max-width: 34rem; }

  .big-quote { margin: 3rem 0 4rem; font-size: clamp(1.15rem, 2.2vw, 1.5rem); line-height: 1.4; max-width: 50rem; color: var(--dim); letter-spacing: -0.01em; }
  .big-quote span { font-family: var(--serif); font-style: italic; color: var(--cu); font-size: 1.8em; line-height: 1; }

  .flow-desc { color: var(--dim); font-size: 0.92rem; max-width: 52rem; margin: -1.2rem 0 2rem; }
  .flow-desc code, .lever code { color: var(--fg); }
  .sub-h {
    margin: 3rem 0 1rem; font-size: 1.15rem; letter-spacing: -0.015em; font-weight: 650;
  }
  .lever { color: var(--dim); font-size: 0.9rem; max-width: 52rem; margin: 0 0 1rem; }

  /* parallel agents: the 429 the app returns, verbatim */
  .quote429 {
    margin: 0.9rem 0 1rem; padding: 0.9rem 1.1rem; max-width: 52rem;
    border-left: 2px solid var(--pw); border-radius: 0 10px 10px 0;
    background: var(--panel); color: var(--dim);
    font-size: 0.78rem; line-height: 1.6; white-space: pre-wrap; word-break: break-word;
  }

  .foot-site { border-top: 1px solid var(--line); padding: 2rem 0 3rem; color: var(--faint); font-size: 0.84rem; }
  .foot-site code { color: var(--dim); }
  .foot-site a { color: var(--dim); }

  /* responsive */
  @media (max-width: 980px) {
    .hero { grid-template-columns: 1fr; padding: 4rem 0 3.5rem; gap: 2.5rem; }
    .features { grid-template-columns: repeat(2, 1fr); }
    .api-grid, .trio { grid-template-columns: minmax(0, 1fr); }
    .sp { grid-template-columns: 9rem 1fr; }
    .sp-note { grid-column: 2; text-align: left; }
  }
  @media (max-width: 720px) {
    nav { display: none; }
    .sec-head { grid-template-columns: 1fr; }
    .num { grid-row: auto; margin: 0 0 0.4rem; }
    .features { grid-template-columns: 1fr; }
    .flow i { display: none; }
    .routes li { grid-template-columns: 4.6rem 1fr; }
    .routes li span:last-child { grid-column: 2; }
    .sp { grid-template-columns: 1fr; gap: 0.5rem; }
    .sp-note { grid-column: 1; }
    .stats { gap: 1.4rem; }
    .stats.wide { gap: 1.6rem; }
    .section { padding-top: 4rem; }
  }
</style>
