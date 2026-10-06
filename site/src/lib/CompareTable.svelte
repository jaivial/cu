<script>
  // Comparison table with a relative bar per cell and the best of each row
  // highlighted. Cells: { text, frac (0..1 relative bar), best, color }.
  let { caption, first, columns, rows, note = '', bestSr = 'mejor de la fila' } = $props();
</script>

<div class="ct-wrap" role="region" aria-label={caption} tabindex="0">
  <table class="ct">
    <caption>{caption}</caption>
    <thead>
      <tr>
        <th scope="col" class="first">{first}</th>
        {#each columns as col}
          <th scope="col" class="r">{col.label}</th>
        {/each}
      </tr>
    </thead>
    <tbody>
      {#each rows as row}
        <tr>
          <th scope="row" class="rowlabel">
            {row.label}
            {#if row.sub}<span class="sub">{row.sub}</span>{/if}
          </th>
          {#each row.cells as cell}
            <td class="r" class:best={cell.best}>
              <span class="val">
                {cell.text}{#if cell.best}<span class="sr"> — {bestSr}</span>{/if}
              </span>
              {#if cell.frac != null}
                <span class="bar" aria-hidden="true">
                  <i style="width:{Math.max(2, cell.frac * 100)}%;background:{cell.color || 'var(--bar)'}"></i>
                </span>
              {/if}
            </td>
          {/each}
        </tr>
      {/each}
    </tbody>
  </table>
  {#if note}<p class="note">{note}</p>{/if}
</div>

<style>
  .ct-wrap { overflow-x: auto; }
  .ct { width: 100%; border-collapse: collapse; min-width: 640px; }
  .ct caption {
    caption-side: top; text-align: left; padding: 0 0 0.9rem;
    font-family: var(--mono); font-size: 0.74rem; text-transform: uppercase;
    letter-spacing: 0.09em; color: var(--faint);
  }
  .ct th, .ct td { padding: 0.75rem 0; border-bottom: 1px solid var(--line); vertical-align: bottom; }
  .ct thead th {
    font-weight: 500; font-size: 0.72rem; color: var(--faint);
    text-transform: uppercase; letter-spacing: 0.06em; text-align: left;
  }
  .ct th.first { min-width: 12rem; }
  .ct .rowlabel {
    font-weight: 500; color: var(--fg); text-align: left; padding-right: 1.2rem;
    vertical-align: middle;
  }
  .ct .rowlabel .sub { display: block; font-size: 0.76rem; color: var(--faint); font-weight: 400; }
  .ct td.r { text-align: right; padding-left: 1.1rem; min-width: 7.2rem; }
  .ct .val {
    display: block; font-family: var(--mono); font-variant-numeric: tabular-nums;
    font-size: 0.95rem; color: var(--dim); white-space: nowrap;
  }
  .ct td.best .val { color: var(--cu); font-weight: 600; }
  .bar { display: block; height: 7px; margin-top: 0.45rem; border-radius: 4px; background: var(--track); overflow: hidden; }
  .bar i { display: block; height: 100%; border-radius: 4px; opacity: 0.55; }
  td.best .bar i { opacity: 1; box-shadow: 0 0 14px -2px var(--cu); }
  .note { margin: 0.9rem 0 0; color: var(--dim); font-size: 0.82rem; max-width: 52rem; }
  .sr {
    position: absolute; width: 1px; height: 1px; overflow: hidden;
    clip: rect(0 0 0 0); white-space: nowrap;
  }
  tr:last-child th, tr:last-child td { border-bottom: 0; }
</style>
