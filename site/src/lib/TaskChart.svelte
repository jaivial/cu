<script>
  import { tools } from './data.js';

  let { task, scale = 'linear' } = $props();

  const max = $derived(Math.max(...tools.map((t) => task.v[t.id])));
  const min = $derived(Math.min(...tools.map((t) => task.v[t.id])));
  const best = $derived(tools.find((t) => task.v[t.id] === min).id);

  function width(v) {
    if (scale === 'log') {
      // log over 1 ms .. 10 s so every card shares one axis
      return Math.max(2, (Math.log10(Math.max(v, 1)) / 4) * 100);
    }
    return Math.max(1.5, (v / max) * 100);
  }

  const fmt = (v) => (v < 10 ? v.toLocaleString('es-ES', { maximumFractionDigits: 1 }) : Math.round(v).toLocaleString('es-ES'));
  const ratio = $derived(task.v.pw / Math.min(task.v.cu, task.v.batch));
</script>

<figure class="card">
  <figcaption>
    <h3>{task.name}</h3>
    {#if task.note}<p class="note">{task.note}</p>{/if}
  </figcaption>
  <ul>
    {#each tools as t (t.id)}
      <li class:best={t.id === best}>
        <span class="who">{t.label}</span>
        <span class="track">
          <span class="bar" style="width:{width(task.v[t.id])}%; --c:{t.color}"></span>
        </span>
        <span class="val">{fmt(task.v[t.id])}<small> ms</small></span>
      </li>
    {/each}
  </ul>
  <p class="gain"><b>{ratio.toLocaleString('es-ES', { maximumFractionDigits: ratio < 10 ? 1 : 0 })}×</b> más rápido que Playwright MCP</p>
</figure>

<style>
  .card {
    margin: 0;
    padding: 1.25rem 1.25rem 1rem;
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: 14px;
    display: flex;
    flex-direction: column;
    gap: 0.9rem;
  }
  h3 {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .note {
    margin: 0.2rem 0 0;
    color: var(--dim);
    font-size: 0.8rem;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 0.45rem;
  }
  li {
    display: grid;
    grid-template-columns: 7.5rem 1fr 4.6rem;
    align-items: center;
    gap: 0.6rem;
    font-size: 0.8rem;
  }
  .who {
    color: var(--dim);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .best .who { color: var(--fg); }
  .track {
    height: 10px;
    background: var(--track);
    border-radius: 99px;
    overflow: hidden;
  }
  .bar {
    display: block;
    height: 100%;
    background: var(--c);
    border-radius: 99px;
    transition: width 0.6s cubic-bezier(0.2, 0.8, 0.2, 1);
  }
  .val {
    font-family: var(--mono);
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .val small { color: var(--dim); }
  .best .val { color: var(--cu); font-weight: 600; }
  .gain {
    margin: auto 0 0;
    padding-top: 0.7rem;
    border-top: 1px dashed var(--line);
    font-size: 0.78rem;
    color: var(--dim);
  }
  .gain b { color: var(--fg); font-family: var(--mono); }
  @media (max-width: 420px) {
    li { grid-template-columns: 6.2rem 1fr 4.2rem; }
  }
</style>
