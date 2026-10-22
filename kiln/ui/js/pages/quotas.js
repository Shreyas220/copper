import { enc, get, put } from '../api.js';
import { fill, h } from '../dom.js';
import { gb } from '../format.js';
import { isAdmin, session } from '../session.js';
import { act, button, empty, field, input, meter, notice, subtitle, table, title } from '../ui.js';

/** A cap per project. Refused up front, like capacity; lowering a cap never
 *  evicts anything. */
export default async function quotas() {
  const list = await get('/v1/quotas');
  const editor = h('div');
  const pathOf = (q) => (q.owner === session.me.name ? q.project : `${q.owner}:${q.project}`);
  const show = (q) => fill(editor, edit(q, pathOf(q)));
  const full = list.filter((q) => (q.caps.vcpus && q.used.vcpus >= q.caps.vcpus) || (q.caps.memory_mb && q.used.memory_mb >= q.caps.memory_mb));
  return [
    h('section', {}, h('div', { class: 'head' }, title('Quotas.', `${list.length} ${list.length === 1 ? 'project' : 'projects'}.`))),
    h('section', {}, list.length
      ? table([
        { title: 'project', cell: (q) => q.project },
        { title: 'owner', cell: (q) => q.owner },
        { title: 'vcpu', cell: (q) => usage(q.used.vcpus, q.caps.vcpus, String) },
        { title: 'memory', cell: (q) => usage(q.used.memory_mb, q.caps.memory_mb, gb) },
        { title: 'disk', cell: (q) => usage(q.used.disk_gb, q.caps.disk_gb, (v) => `${v} gb`) },
      ], list, isAdmin() ? show : null)
      : empty('no projects yet.'),
    full.map((q) => h('div', { class: 'pad' }, notice(`${q.project} is at its cap`, 'warm')))),
    isAdmin() ? h('section', {}, editor) : null,
  ];
}

function usage(used, cap, fmt) {
  return cap ? meter('', used, cap, `${fmt(used)} / ${fmt(cap)}`) : h('span', { class: 'quiet' }, `${fmt(used)} · no cap`);
}

function edit(q, path) {
  const box = (v) => input({ type: 'number', min: 0, value: v ?? '', placeholder: 'no cap' });
  const vcpus = box(q.caps.vcpus), mem = box(q.caps.memory_mb ? q.caps.memory_mb / 1024 : null), disk = box(q.caps.disk_gb);
  const save = button('save ›', { primary: true, type: 'submit' });
  const num = (el, scale = 1) => (el.value === '' ? null : Math.round(Number(el.value) * scale));
  const form = h('form', { class: 'pad stack' },
    subtitle('Edit.', `${q.project} · ${q.owner}`),
    h('div', { class: 'cells four', style: { background: 'none', gap: '16px' } },
      field('vcpu cap', vcpus, `${q.used.vcpus} in use`),
      field('memory cap, gb', mem, `${gb(q.used.memory_mb)} in use`),
      field('disk cap, gb', disk, `${q.used.disk_gb} gb in use`)),
    h('div', { class: 'row' }, save));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    await act(save, () => put(`/v1/projects/${enc(path)}/quota`, { vcpus: num(vcpus), memory_mb: num(mem, 1024), disk_gb: num(disk) }), 'saved');
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  });
  return form;
}
