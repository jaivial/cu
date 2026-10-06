<script>
  import TaskChart from './lib/TaskChart.svelte';
  import { tools, tasks, tokens, speed, actionCosts, contexts, resources } from './lib/data.js';

  let scale = $state('linear');
  let tab = $state('cli');

  const maxTokens = Math.max(...tokens.map((t) => t.tokens));
  const maxSpeed = Math.max(...speed.map((s) => s.before));
  const n = (v) => v.toLocaleString('es-ES');

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
      t: 'Un navegador que no se apaga',
      d: 'Un Chromium persistente detrás de un servidor HTTP solo en loopback. El agente no lanza un navegador por acción: lo encuentra caliente, con su perfil y sus cookies.',
    },
    {
      k: '02',
      t: 'Snapshots, no capturas',
      d: 'Una línea por elemento interactivo con un ref estable. Unos cientos de bytes en un único viaje CDP, en lugar de volcar el DOM o leer píxeles.',
    },
    {
      k: '03',
      t: 'Actuar por ref, en lote',
      d: 'click e3, type e2, o un formulario entero en una llamada que vuelve con la página nueva. Los refs sobreviven a acciones y snapshots.',
    },
    {
      k: '04',
      t: 'Las contraseñas no pasan por el modelo',
      d: 'El usuario las escribe en un formulario local en /login; el daemon las teclea en el navegador. Nunca se guardan, registran ni devuelven.',
    },
  ];

  const cli = [
    ['cu start --data .cu', 'arranca el daemon; responde en ~6 ms y el navegador sube en segundo plano'],
    ['cu status', 'estado del daemon y del navegador'],
    ['cu navigate https://example.com', 'espera a que la página sea usable (máx. 3 s) e informa settled_ms'],
    ['cu snapshot', 'la página para el LLM: url, título, headings y refs'],
    ['cu click e3', 'click real con test de impacto; si algo lo tapa, lo dice'],
    ['cu type e2 "Ada" --submit', 'Input.insertText y, si se pide, Enter'],
    ["cu act '[{\"do\":\"type\",\"ref\":\"e2\",\"text\":\"Ada\"},{\"do\":\"click\",\"ref\":\"e5\"}]'", 'lote en una conexión, termina con snapshot'],
    ['cu shot page.jpg', 'captura JPEG rápida; PNG si los píxeles importan'],
    ['cu login', 'imprime el enlace /login para que el humano entre'],
    ['cu session save NAME / load NAME', 'guarda o restaura el perfil con la sesión iniciada'],
  ];

  const http = [
    ['GET', '/v1/status', 'estado'],
    ['POST', '/v1/navigate', '{"url": "..."}'],
    ['GET', '/v1/snapshot', 'página compacta con refs'],
    ['GET', '/v1/screenshot', '?format=png para PNG'],
    ['POST', '/v1/click', '{"ref": "e3"}'],
    ['POST', '/v1/type', '{"ref": "e2", "text": "hi", "submit": true}'],
    ['POST', '/v1/act', '{"actions": [...], "snapshot": true}'],
    ['GET', '/v1/contexts', 'lista contextos aislados'],
    ['DELETE', '/v1/contexts/NAME', 'cierra un contexto'],
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
</script>

<a class="skip" href="#main">Saltar al contenido</a>

<header class="top">
  <div class="wrap bar">
    <a class="logo" href="#top" aria-label="cu, inicio"><span>cu</span></a>
    <nav aria-label="Secciones">
      <a href="#que">Qué es</a>
      <a href="#api">API / CLI</a>
      <a href="#bench">Benchmarks</a>
      <a href="#tokens">Tokens</a>
    </nav>
    <span class="local">local · sin publicar</span>
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
        {:else if tab === 'http'}
          <ul class="routes">
            {#each http as [m, p, d]}
              <li><span class="m m-{m.split(' ')[0].toLowerCase()}">{m}</span><code>{p}</code><span>{d}</span></li>
            {/each}
          </ul>
          <p class="foot">Todas con <code>Authorization: Bearer</code> salvo <code>/login</code>. Añade
            <code>?context=NAME</code> a navigate, snapshot, screenshot o act.</p>
        {:else}
          <pre class="code"><span class="k">let</span> cu = cu::Client::new(<span class="s">"127.0.0.1:8787"</span>, token);
println!(<span class="s">"{"{}"}"</span>, cu.status()?);
cu.navigate(<span class="s">"https://example.com"</span>)?;
let page = cu.snapshot()?;
cu.click(<span class="s">"e3"</span>)?;
cu.act(<span class="s">r#"[{"{"}"do":"type","ref":"e2","text":"Ada"{"}"}]"#</span>)?;
cu.save_session(<span class="s">"example"</span>)?;</pre>
          <p class="foot">El token vive en <code>.cu/server.json</code>; mantenlo privado.</p>
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
      <h2>Benchmarks reales</h2>
      <p class="sub">
        Mismo binario (Chrome for Testing headless shell 153), mismo sitio local y las mismas seis tareas de agente.
        <code>cu</code> y agent-browser por CLI, un proceso por acción; Playwright MCP por stdio JSON-RPC. Mediana
        de 10 rondas en orden rotatorio, en ms. Menos es mejor.
      </p>
    </header>

    <div class="controls">
      <ul class="legend">
        {#each tools as t}
          <li><i style="background:{t.color}"></i>{t.label}</li>
        {/each}
      </ul>
      <div class="seg" role="group" aria-label="Escala de las barras">
        <button aria-pressed={scale === 'linear'} onclick={() => (scale = 'linear')}>Lineal por tarea</button>
        <button aria-pressed={scale === 'log'} onclick={() => (scale = 'log')}>Logarítmica común</button>
      </div>
    </div>

    <div class="charts">
      {#each tasks as task (task.key)}
        <TaskChart {task} {scale} />
      {/each}
    </div>

    <details class="raw">
      <summary>Ver la tabla completa</summary>
      <div class="scroll">
        <table class="data">
          <thead>
            <tr><th>tarea</th>{#each tools as t}<th class="r">{t.label}</th>{/each}</tr>
          </thead>
          <tbody>
            {#each tasks as task}
              {@const m = Math.min(...tools.map((t) => task.v[t.id]))}
              <tr>
                <td>{task.name}</td>
                {#each tools as t}
                  <td class="r" class:win={task.v[t.id] === m}>{n(task.v[t.id])}</td>
                {/each}
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </details>

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

    <div class="tok">
      <div class="tok-chart">
        {#each tokens as t}
          <div class="tk" class:best={t.best}>
            <div class="tk-label">
              <span>{t.tool}</span><code>{t.cmd}</code>
            </div>
            <div class="tk-track">
              <span class="tk-bar" style="width:{(t.tokens / maxTokens) * 100}%; --c:{t.color}"></span>
            </div>
            <span class="tk-val">{n(t.tokens)}</span>
          </div>
        {/each}
      </div>

      <div class="scroll">
        <table class="data">
          <thead>
            <tr><th>herramienta</th><th class="r">tokens</th><th class="r">bytes</th><th class="r">vs cu</th></tr>
          </thead>
          <tbody>
            {#each tokens as t}
              <tr class:winrow={t.best}>
                <td>{t.tool} <code>{t.cmd}</code></td>
                <td class="r">{n(t.tokens)}</td>
                <td class="r">{t.bytes ? n(t.bytes) : '—'}</td>
                <td class="r">{t.best ? '1×' : (t.tokens / 350).toLocaleString('es-ES', { maximumFractionDigits: 1 }) + '×'}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </div>

    <p class="big-quote">
      <span>3,8×</span> menos tokens que Playwright MCP por cada vistazo a la página: una línea por elemento
      interactivo, sin marcado de presentación ni nodos de texto, con tope de 200 nodos y 120 caracteres por nombre.
    </p>
  </section>
</main>

<footer class="wrap foot-site">
  <p><code>cu</code> · solo local, nada publicado. Datos de <code>docs/BENCHMARKS.md</code>; reproducibles con
    <code>python3 bench/compare.py --iters 10</code>.</p>
</footer>

<style>
  .wrap { width: min(var(--w), 100% - 2.5rem); margin-inline: auto; }

  .skip { position: absolute; left: -999px; }
  .skip:focus { left: 1rem; top: 1rem; z-index: 10; background: var(--cu); color: #000; padding: 0.4rem 0.7rem; }

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
  .tabs button, .seg button {
    font: inherit; font-size: 0.85rem; color: var(--dim); background: none; border: 0;
    padding: 0.4rem 0.9rem; border-radius: 7px; cursor: pointer;
  }
  .tabs button[aria-selected='true'], .seg button[aria-pressed='true'] { background: var(--fg); color: var(--bg); }
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
  .kv .r, .data .r { text-align: right; padding-left: 0.8rem; padding-right: 0; }
  .kv th code { color: var(--cu); }
  .dimc { color: var(--faint) !important; }
  .trio { display: grid; grid-template-columns: repeat(3, 1fr); gap: 1rem; margin-top: 1rem; }

  /* benchmarks */
  .controls { display: flex; justify-content: space-between; align-items: center; gap: 1rem; flex-wrap: wrap; margin-bottom: 1.2rem; }
  .legend { list-style: none; padding: 0; margin: 0; display: flex; gap: 1.2rem; flex-wrap: wrap; font-size: 0.85rem; color: var(--dim); }
  .legend li { display: flex; align-items: center; gap: 0.45rem; }
  .legend i { width: 12px; height: 12px; border-radius: 3px; }
  .seg { display: inline-flex; padding: 0.25rem; border: 1px solid var(--line); border-radius: 10px; background: var(--panel); }
  .charts { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(330px, 100%), 1fr)); gap: 1rem; }

  .raw { margin-top: 1.4rem; border: 1px solid var(--line); border-radius: 12px; padding: 0.2rem 1.2rem; }
  .raw summary { cursor: pointer; padding: 0.8rem 0; color: var(--dim); font-size: 0.9rem; }
  .raw[open] summary { color: var(--fg); }
  .scroll { overflow-x: auto; }
  .data { width: 100%; border-collapse: collapse; font-size: 0.88rem; margin-bottom: 0.8rem; }
  .data th { font-weight: 500; font-size: 0.75rem; color: var(--faint); text-transform: uppercase; letter-spacing: 0.06em; text-align: left; }
  .data th, .data td { padding: 0.6rem 0; border-bottom: 1px solid var(--line); white-space: nowrap; }
  .data td.r { font-family: var(--mono); font-variant-numeric: tabular-nums; }
  .data .win { color: var(--cu); font-weight: 600; }
  .winrow td { color: var(--cu); }
  .winrow td code { color: var(--cu); }

  .fair { margin: 1.6rem 0 0; padding: 0 0 0 1.1rem; color: var(--dim); font-size: 0.88rem; display: grid; gap: 0.5rem; max-width: 52rem; }
  .fair code { color: var(--fg); }

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

  /* tokens */
  .tok { display: grid; grid-template-columns: 1.3fr 1fr; gap: 2rem; align-items: center; }
  .tok-chart { display: grid; gap: 1.1rem; }
  .tk { display: grid; grid-template-columns: 1fr 4rem; gap: 0.35rem 1rem; align-items: center; }
  .tk-label { grid-column: 1 / -1; display: flex; gap: 0.6rem; align-items: baseline; font-size: 0.9rem; }
  .tk-label code { color: var(--dim); font-size: 0.78rem; }
  .tk-track { height: 28px; background: var(--track); border-radius: 6px; overflow: hidden; }
  .tk-bar { display: block; height: 100%; background: var(--c); border-radius: 6px; opacity: 0.85; }
  .tk.best .tk-bar { opacity: 1; box-shadow: 0 0 24px -4px var(--cu); }
  .tk-val { font-family: var(--mono); font-size: 1.05rem; text-align: right; font-variant-numeric: tabular-nums; }
  .tk.best .tk-val, .tk.best .tk-label span { color: var(--cu); font-weight: 600; }

  .big-quote { margin: 3rem 0 4rem; font-size: clamp(1.15rem, 2.2vw, 1.5rem); line-height: 1.4; max-width: 50rem; color: var(--dim); letter-spacing: -0.01em; }
  .big-quote span { font-family: var(--serif); font-style: italic; color: var(--cu); font-size: 1.8em; line-height: 1; }

  .foot-site { border-top: 1px solid var(--line); padding: 2rem 0 3rem; color: var(--faint); font-size: 0.84rem; }
  .foot-site code { color: var(--dim); }

  /* responsive */
  @media (max-width: 980px) {
    .hero { grid-template-columns: 1fr; padding: 4rem 0 3.5rem; gap: 2.5rem; }
    .features { grid-template-columns: repeat(2, 1fr); }
    .api-grid, .trio, .tok { grid-template-columns: minmax(0, 1fr); }
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
    .charts { grid-template-columns: 1fr; }
    .sp { grid-template-columns: 1fr; gap: 0.5rem; }
    .sp-note { grid-column: 1; }
    .stats { gap: 1.4rem; }
    .section { padding-top: 4rem; }
  }
</style>
