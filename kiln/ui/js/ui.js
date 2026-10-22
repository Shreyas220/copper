// Shared pieces of the v2 look: two-tone titles, figure labels, square
// buttons, state dots, meters, copyable commands, tables.

import { h, svg } from './dom.js';

/** `Overview.` in white, the rest in grey. */
export const title = (noun, rest) => h('h1', { class: 'title' }, h('b', {}, noun), rest ? ` ${rest}` : '');
export const subtitle = (noun, rest) => h('h2', { class: 'subtitle' }, h('b', {}, noun), rest ? ` ${rest}` : '');

export const label = (text, extra) => h('div', { class: 'label row spread' }, h('span', {}, text), extra);

/** `FIG.2 · needs a look` */
export const fig = (n, text, extra) => label(h('span', {}, `FIG.${n} `, h('span', { class: 'quiet' }, `· ${text}`)), extra);

export const tag = (text, kind = '') => h('span', { class: `tag ${kind}` }, text);

export function button(text, { primary, danger, small, wide, onclick, href, disabled, type } = {}) {
  const cls = ['btn', primary && 'primary', danger && 'danger', small && 'small', wide && 'wide'].filter(Boolean).join(' ');
  if (href) return h('a', { class: cls, href }, text);
  return h('button', { class: cls, type: type ?? 'button', onclick, disabled }, text);
}

export const dot = (state, blink) => h('span', { class: `dot ${state}${blink ? ' blink' : ''}`, 'aria-hidden': 'true' });

export function stateBadge(state) {
  const busy = state === 'starting' || state === 'stopping' || state === 'deleting';
  return h('span', { class: `state ${state}` }, dot(state, busy), state);
}

/** A labelled bar: `vcpu ▓▓▓░░ 16 / 20`. */
export function meter(name, used, total, text, kind = '') {
  const share = total > 0 ? Math.min(100, (used / total) * 100) : 0;
  const cls = used > total && total > 0 ? 'over' : kind;
  return h('div', { class: 'meter' },
    h('span', { class: 'label' }, name),
    h('span', { class: 'bar', 'aria-hidden': 'true' }, h('i', { class: cls, style: { width: `${share}%` } })),
    h('span', { class: 'val' }, text));
}

/** A command block with a copy button. Lines starting with `$ ` are commands. */
export function copyBlock(caption, lines) {
  const text = lines.map((l) => l.replace(/^\$ /, '')).join('\n');
  const btn = h('button', { type: 'button' }, 'copy');
  btn.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(text);
      btn.textContent = 'copied';
    } catch {
      btn.textContent = 'select and copy';
    }
    setTimeout(() => (btn.textContent = 'copy'), 1500);
  });
  return h('div', { class: 'copy' },
    h('div', { class: 'bar' }, h('span', { class: 'label' }, caption), btn),
    h('pre', {}, lines.map((l, i) => [
      i ? '\n' : '',
      l.startsWith('$ ') ? [h('span', { class: 'p' }, '$ '), l.slice(2)] : h('span', { class: 'quiet' }, l),
    ])));
}

/** columns: [{ title, cell(row) }]; onRow makes rows clickable. */
export function table(columns, rows, onRow) {
  return h('div', { class: 'table-wrap' }, h('table', { class: 'list' },
    h('thead', {}, h('tr', {}, columns.map((c) => h('th', { class: c.class }, c.title)))),
    h('tbody', {}, rows.map((row) => h('tr', {
      class: onRow ? 'link' : null,
      tabindex: onRow ? 0 : null,
      onclick: onRow ? () => onRow(row) : null,
      onkeydown: onRow ? (e) => e.key === 'Enter' && onRow(row) : null,
    }, columns.map((c) => h('td', { class: c.class }, c.cell(row))))))));
}

/** Definition list of [key, value] pairs. */
export const kv = (pairs) => h('dl', { class: 'kv' }, pairs.filter(Boolean).map(([k, v]) => [h('dt', {}, k), h('dd', {}, v)]));

export const empty = (...text) => h('div', { class: 'empty' }, ...text);

export const notice = (text, kind = '') => h('div', { class: `notice ${kind}`, role: kind === 'bad' ? 'alert' : null }, text);

export function field(name, input, hint) {
  return h('label', { class: 'field' }, h('span', {}, name), input, hint ? h('span', { class: 'hint' }, hint) : null);
}

export const input = (props) => h('input', { class: 'input', ...props });

let toastTimer;
export function toast(message, bad = false) {
  document.querySelector('.toast')?.remove();
  const el = h('div', { class: `toast${bad ? ' bad' : ''}`, role: 'status' }, message);
  document.body.append(el);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.remove(), 5000);
}

/** Run an API action from a button: disables it, reports errors. */
export async function act(btn, fn, done) {
  btn.disabled = true;
  try {
    const result = await fn();
    if (done) toast(done);
    return result;
  } catch (e) {
    toast(e.message, true);
    return undefined;
  } finally {
    btn.disabled = false;
  }
}

/** A sparkline of the last hour. `values` are 0..max. */
export function spark(values, max) {
  const w = 300, hgt = 72;
  if (values.length < 2) return h('div', { class: 'hint' }, 'collecting… a reading every 10 s');
  const step = w / (values.length - 1);
  const pts = values.map((v, i) => `${(i * step).toFixed(1)},${(hgt - (Math.min(v, max) / (max || 1)) * (hgt - 4) - 2).toFixed(1)}`);
  return svg('svg', { class: 'spark', viewBox: `0 0 ${w} ${hgt}`, preserveAspectRatio: 'none', role: 'img', 'aria-label': 'last hour' },
    svg('polygon', { class: 'area', points: `0,${hgt} ${pts.join(' ')} ${w},${hgt}` }),
    svg('polyline', { points: pts.join(' ') }));
}
