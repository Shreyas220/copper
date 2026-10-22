// Shared pieces of the v2 look: two-tone titles, figure labels, square
// buttons, state marks, meters, copyable commands, tables.

import { h, svg } from './dom.js';

/** `Overview.` in white, the rest in grey. */
export const title = (noun, rest) => h('h1', { class: 'title' }, h('b', {}, noun), rest ? ` ${rest}` : '');
export const subtitle = (noun, rest) => h('h2', { class: 'subtitle' }, h('b', {}, noun), rest ? ` ${rest}` : '');

/** `instances / web` */
export function crumb(...parts) {
  return h('nav', { class: 'crumb', 'aria-label': 'breadcrumb' }, parts.map((p, i) => [
    i ? h('span', {}, '/') : null,
    i === parts.length - 1 ? h('b', {}, p.text) : h('a', { href: p.href }, p.text),
  ]));
}

export const label = (text, extra) => h('div', { class: 'label row spread' }, h('span', {}, text), extra);

/** `FIG.2 · needs a look` */
export const fig = (n, text, extra) => label(h('span', {}, `FIG.${n} `, h('span', { class: 'quiet' }, `· ${text}`)), extra);

export const tag = (text) => h('span', { class: 'tag' }, text);

export function button(text, { primary, warm, danger, small, wide, onclick, href, disabled, type } = {}) {
  const cls = ['btn', primary && 'primary', warm && 'warm', danger && 'danger', small && 'small', wide && 'wide'].filter(Boolean).join(' ');
  if (href) return h('a', { class: cls, href }, text);
  return h('button', { class: cls, type: type ?? 'button', onclick, disabled }, text);
}

export const dot = (state, blink) => h('span', { class: `dot ${state}${blink ? ' blink' : ''}`, 'aria-hidden': 'true' });

/** A state as the tables show it: coloured, lowercase, with its mark. */
export const st = (state, text = state) => h('span', { class: `st ${state}` }, text);

/** A state as the instance header shows it: in a bordered pill. */
export const pill = (state) => h('span', { class: `pill ${state}` }, state);

/** A labelled bar. `extra` is an overcommitted stretch drawn hatched. */
export function meter(name, used, total, text, { kind = '', extra = 0 } = {}) {
  const scale = Math.max(total, used, 1);
  const cls = used > total && total > 0 ? 'over' : kind;
  return h('div', { class: 'meter' },
    h('span', { class: 'label' }, name),
    h('span', { class: 'bar', 'aria-hidden': 'true' },
      h('i', { class: cls, style: { width: `${(used / scale) * 100}%` } }),
      extra > 0 ? h('i', { class: 'oc', style: { width: `${(extra / scale) * 100}%`, marginLeft: 'auto' } }) : null),
    h('span', { class: 'val' }, text));
}

/** A command block with a copy button. Each line is a string (`$ ` marks a
 *  command) or an array of strings and `[text, class]` pairs. */
export function copyBlock(caption, lines) {
  const plain = (l) => (Array.isArray(l) ? l.map((p) => (Array.isArray(p) ? p[0] : p)).join('') : l);
  const text = lines.map(plain).filter((l) => l.startsWith('$ ')).map((l) => l.slice(2)).join('\n') || lines.map(plain).join('\n');
  const btn = h('button', { type: 'button' }, 'copy');
  btn.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(text);
      btn.textContent = 'copied';
    } catch {
      btn.textContent = 'select it';
    }
    setTimeout(() => (btn.textContent = 'copy'), 1500);
  });
  const render = (l) => {
    if (Array.isArray(l)) return l.map((p) => (Array.isArray(p) ? h('span', { class: p[1] }, p[0]) : p));
    return l.startsWith('$ ') ? [h('span', { class: 'p' }, '$ '), l.slice(2)] : h('span', { class: 'quiet' }, l);
  };
  return h('div', { class: 'copy' },
    h('div', { class: 'bar' }, h('span', { class: 'label' }, caption), btn),
    h('pre', {}, lines.map((l, i) => [i ? '\n' : '', render(l)])));
}

/** columns: [{ title, cell(row), class }]; onRow makes rows clickable. */
export function table(columns, rows, onRow) {
  return h('div', { class: 'table-wrap' }, h('table', { class: 'list' },
    h('thead', {}, h('tr', {}, columns.map((c) => h('th', { scope: 'col', class: c.class }, c.title)))),
    h('tbody', {}, rows.map((row) => h('tr', {
      class: onRow ? 'link' : null,
      onclick: onRow ? (e) => !e.target.closest('a, button') && onRow(row) : null,
    }, columns.map((c) => h('td', { class: c.class }, c.cell(row))))))));
}

/** A grid of labelled values: [[label, value], …]. */
export const props = (pairs) => h('div', { class: 'props' }, pairs.filter(Boolean).map(([k, v]) => h('div', {}, h('span', {}, k), h('b', {}, v))));
export const kv = (pairs) => h('div', { class: 'kv' }, pairs.filter(Boolean).map(([k, v]) => h('div', {}, h('span', {}, k), h('b', {}, v))));

export const empty = (...text) => h('div', { class: 'empty' }, ...text);

export const notice = (text, kind = '') => h('div', { class: `notice ${kind}`, role: kind === 'bad' ? 'alert' : null }, text);

export function field(name, input, hint, hintClass = '') {
  return h('label', { class: 'field' }, h('span', {}, name), input, hint ? h('span', { class: `hint ${hintClass}` }, hint) : null);
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
export function spark(values, max, { kind = '', mini = false } = {}) {
  const w = 300, hgt = mini ? 28 : 64;
  if (values.length < 2) return h('div', { class: 'hint' }, 'no readings yet');
  const step = w / (values.length - 1);
  const pts = values.map((v, i) => `${(i * step).toFixed(1)},${(hgt - (Math.min(v, max) / (max || 1)) * (hgt - 4) - 2).toFixed(1)}`);
  return svg('svg', { class: `spark ${kind}${mini ? ' mini' : ''}`, viewBox: `0 0 ${w} ${hgt}`, preserveAspectRatio: 'none', role: 'img', 'aria-label': 'last hour' },
    svg('polygon', { class: 'area', points: `0,${hgt} ${pts.join(' ')} ${w},${hgt}` }),
    svg('polyline', { points: pts.join(' ') }));
}
