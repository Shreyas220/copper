import { enc, get, put } from '../api.js';
import { fill, h } from '../dom.js';
import { gb } from '../format.js';
import { isAdmin, session } from '../session.js';
import { act, button, empty, field, input, subtitle, table, title } from '../ui.js';

const KINDS = [
  { key: 'vcpus', used: 'vcpus', name: 'vcpu', fmt: String },
  { key: 'memory_mb', used: 'memory_mb', name: 'memory', fmt: (mb) => gb(mb).replace(' gb', '') },
  { key: 'disk_gb', used: 'disk_gb', name: 'disk', fmt: String },
];

/** A cap per project. Refused up front, like capacity; lowering a cap never
 *  evicts anything. */
export default async function quotas() {
  const list = await get('/v1/quotas');
  const editor = h('div');
  const pathOf = (q) => (q.owner === session.me.name ? q.project : `${q.owner}:${q.project}`);
  const close = () => fill(editor);
  const open = (q) => {
    fill(editor, h('section', {}, edit(q, pathOf(q), close)));
    editor.scrollIntoView({ behavior: 'smooth', block: 'start' });
  };
  const capped = list.flatMap((q) => KINDS.filter((k) => q.caps[k.key] && q.used[k.used] >= q.caps[k.key]).map((k) => `${q.project} is at its ${k.name} cap`));
  return [
    h('section', {}, h('div', { class: 'head' }, title('Quotas.', 'A cap per project.'))),
    h('section', {}, h('div', { style: { paddingTop: '40px' } }, list.length
      ? table([
        { title: 'project', cell: (q) => q.project, class: 'name' },
        { title: 'owner', cell: (q) => q.owner },
        ...KINDS.map((k) => ({ title: k.name, cell: (q) => usage(q.used[k.used], q.caps[k.key], k) })),
        isAdmin() ? { title: '', cell: (q) => button('edit', { small: true, onclick: () => open(q) }), class: 'act' } : null,
      ].filter(Boolean), list)
      : empty('no projects yet.')),
    h('div', { class: 'tail' }, h('span', {}, capped.length ? `${capped.join(' · ')} · new vms there are refused up front` : ''))),
    editor,
  ];
}

function usage(used, cap, kind) {
  const unit = kind.key === 'vcpus' ? '' : ' gb';
  if (!cap) return h('span', { class: 'quiet' }, `${kind.fmt(used)}${unit} · no cap`);
  const full = used >= cap;
  return h('span', { class: 'qm' },
    h('span', { class: 'bar', 'aria-hidden': 'true' }, h('i', { class: full ? 'over' : '', style: { width: `${Math.min(100, (used / cap) * 100)}%` } })),
    `${kind.fmt(used)} / ${kind.fmt(cap)}${unit}`);
}

function edit(q, path, close) {
  const box = (v) => input({ type: 'number', min: 0, value: v ?? '', placeholder: 'no cap' });
  const vcpus = box(q.caps.vcpus), mem = box(q.caps.memory_mb ? q.caps.memory_mb / 1024 : null), disk = box(q.caps.disk_gb);
  const save = button('save ›', { primary: true, type: 'submit' });
  const num = (el, scale = 1) => (el.value === '' ? null : Math.round(Number(el.value) * scale));
  const form = h('form', { class: 'panel' },
    h('div', { class: 'cells three flat' },
      field('vcpu cap', vcpus, `${q.used.vcpus} in use`),
      field('memory cap, gb', mem, `${gb(q.used.memory_mb).replace(' gb', '')} in use`),
      field('disk cap, gb', disk, `${q.used.disk_gb} in use`)),
    h('div', { class: 'row', style: { gap: '8px' } }, button('cancel', { onclick: close }), save));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const ok = await act(save, () => put(`/v1/projects/${enc(path)}/quota`, { vcpus: num(vcpus), memory_mb: num(mem, 1024), disk_gb: num(disk) }), 'saved');
    if (ok !== undefined) window.dispatchEvent(new HashChangeEvent('hashchange'));
  });
  return h('div', {},
    h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('Edit.', path)),
    h('div', { class: 'pad', style: { paddingTop: 0 } }, form));
}
