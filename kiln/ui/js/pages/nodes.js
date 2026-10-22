import { enc, get, post } from '../api.js';
import { nodeNote, role, slab } from '../art.js';
import { fill, h } from '../dom.js';
import { bytes, gb, percent, plural } from '../format.js';
import { isAdmin } from '../session.js';
import { act, button, dot, empty, field, fig, input, meter, notice, tag, title } from '../ui.js';

/** Machines as kiln knows them. Admin only: kiln returns 403 to others. */
export default async function nodes(ctx) {
  if (!isAdmin()) return h('section', {}, h('div', { class: 'pad' }, notice('admins only.')));
  const root = h('div', { style: { display: 'contents' } });
  const draw = async () => {
    if (root.contains(document.activeElement) && document.activeElement.tagName === 'INPUT') return;
    fill(root, ...build(await get('/v1/nodes'), draw));
  };
  await draw();
  ctx.every(10000, draw);
  return root;
}

function build(all, redraw) {
  const live = all.filter((n) => n.status !== 'retired');
  const count = (s) => live.filter((n) => n.status === s).length;
  const pending = live.filter((n) => n.status === 'pending');
  const working = live.filter((n) => n.status !== 'pending');
  return [
    h('section', {}, h('div', { class: 'head' },
      title('Nodes.', `${plural(live.length, 'machine')}${pending.length ? `, ${pending.length} waiting` : ''}.`),
      h('p', { class: 'hint' }, `${count('online')} online · ${count('offline')} offline · ${count('pending')} waiting`))),
    pending.map((n) => h('section', {}, wantsIn(n, redraw))),
    working.length ? h('section', {}, h('div', { class: 'cells two cards' },
      working.map((n, i) => card(n, i + 1, redraw)),
      working.length % 2 ? h('div', { 'aria-hidden': 'true' }) : null)) : null,
    live.length ? null : h('section', {}, empty('no machines have registered yet.')),
  ];
}

/** Approve only after checking the key matches what ingot printed. */
function wantsIn(n, redraw) {
  const go = button('let it in ›', { primary: true });
  go.addEventListener('click', async () => {
    await act(go, () => post(`/v1/nodes/${enc(n.id)}/approve`), `${n.hostname} can take work now`);
    redraw();
  });
  // Turning a node away retires its identity; it can never come back as this node.
  const away = button('turn away', { danger: true });
  away.addEventListener('click', async () => {
    if (!window.confirm(`turn ${n.hostname} (${n.id.slice(0, 8)}) away? it can never join as this node.`)) return;
    await act(away, () => post(`/v1/nodes/${enc(n.id)}/retire`), `${n.hostname} turned away`);
    redraw();
  });
  return h('div', { class: 'cells two' },
    h('div', { class: 'pad stack' },
      h('h2', { class: 'subtitle' }, h('b', {}, n.hostname), ' wants in.'),
      h('div', { class: 'row' }, dot('pending', true), tag(n.arch)),
      h('div', { class: 'row' }, h('span', { class: 'label' }, 'key'), h('span', { class: 'mono', style: { fontSize: '16px', letterSpacing: '.08em' } }, n.fingerprint ?? '—')),
      h('p', { class: 'hint', style: { margin: 0 } }, `node ${n.id}`),
      h('div', { class: 'row' }, go, away)),
    h('figure', { class: 'scene' }, slab(n)));
}

function card(n, index, redraw) {
  const cap = n.capacity ?? { vcpus: 0, memory_mb: 0, disk_gb: 0 };
  const used = n.allocated ?? { vcpus: 0, memory_mb: 0, disk_gb: 0 };
  const oc = n.overcommit;
  const allow = { vcpus: Math.floor(cap.vcpus * oc.cpu), memory_mb: Math.floor(cap.memory_mb * oc.memory), disk_gb: Math.floor(cap.disk_gb * oc.disk) };
  const status = n.status === 'online' ? `online · gen ${n.generation}` : nodeNote(n);
  const facts = [role(n) === 'kiln' ? 'runs kiln' : null, n.instances.length ? n.instances.join(', ') : 'no vms'].filter(Boolean);
  return h('div', { class: 'pad stack', style: { gap: '24px' } },
    fig(index, n.hostname, h('span', { class: `row state ${n.status}` }, dot(n.status), status)),
    h('figure', { class: 'scene bleed' }, slab(n)),
    h('div', { class: 'stack', style: { gap: '8px' } },
      h('h3', { class: 'node-name' }, h('span', { class: 'mono quiet' }, String(index).padStart(2, '0')), n.hostname),
      h('p', { class: 'quiet', style: { margin: 0 } }, `${n.arch} · node ${n.id.slice(0, 8)} · key ${n.fingerprint ?? '—'}`)),
    h('ol', { class: 'facts' }, facts.map((f, i) => h('li', {}, h('span', { class: 'mono' }, `${index}.${i + 1}`), f))),
    h('div', { class: 'stack', style: { gap: '10px' } },
      h('span', { class: 'label' }, 'held'),
      meter('vcpu', used.vcpus, allow.vcpus, `${used.vcpus} held · ${Math.max(0, allow.vcpus - used.vcpus)} free`),
      meter('mem', used.memory_mb, allow.memory_mb, `${gb(used.memory_mb)} held · ${gb(Math.max(0, allow.memory_mb - used.memory_mb))} free`),
      meter('disk', used.disk_gb, allow.disk_gb, `${used.disk_gb} gb held · ${Math.max(0, allow.disk_gb - used.disk_gb)} gb free`)),
    n.usage ? h('div', { class: 'stack', style: { gap: '10px' } },
      h('span', { class: 'label' }, 'live'),
      meter('cpu', n.usage.cpu_permille, 1000, percent(n.usage.cpu_permille), 'pat'),
      meter('mem', n.usage.memory_used_bytes, n.usage.memory_total_bytes, `${bytes(n.usage.memory_used_bytes)} / ${bytes(n.usage.memory_total_bytes)}`, 'pat'),
      meter('disk', n.usage.disk_used_bytes, n.usage.disk_total_bytes, `${bytes(n.usage.disk_used_bytes)} / ${bytes(n.usage.disk_total_bytes)}`, 'pat')) : null,
    ratios(n, redraw),
    n.status === 'offline' ? retire(n, redraw) : null);
}

function ratios(n, redraw) {
  const cpu = input({ type: 'number', step: 0.25, min: 0.25, max: 64, value: n.overcommit.cpu });
  const mem = input({ type: 'number', step: 0.05, min: 0.25, max: 64, value: n.overcommit.memory });
  const disk = input({ type: 'number', step: 0.25, min: 0.25, max: 64, value: n.overcommit.disk });
  const save = button('save', { small: true, type: 'submit' });
  const form = h('form', { class: 'stack', style: { gap: '10px' } },
    h('span', { class: 'label' }, 'overcommit'),
    h('div', { class: 'cells three', style: { background: 'none', gap: '10px' } }, field('cpu ×', cpu), field('mem ×', mem), field('disk ×', disk)),
    h('div', { class: 'row' }, save));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    await act(save, () => post(`/v1/nodes/${enc(n.id)}/policy`, { cpu_ratio: Number(cpu.value), memory_ratio: Number(mem.value), disk_ratio: Number(disk.value) }), 'saved');
    save.blur();
    redraw();
  });
  return form;
}

/** Retiring marks its vms failed and never moves them; type the name to confirm. */
function retire(n, redraw) {
  const confirm = input({ placeholder: n.hostname });
  const go = button('retire', { danger: true, small: true, disabled: true });
  confirm.addEventListener('input', () => (go.disabled = confirm.value !== n.hostname));
  go.addEventListener('click', async () => {
    await act(go, () => post(`/v1/nodes/${enc(n.id)}/retire`), `${n.hostname} retired`);
    redraw();
  });
  return h('div', { class: 'stack notice warm' },
    `${nodeNote(n)}. retiring marks ${plural(n.instances.length, 'vm')} failed.`,
    h('div', { class: 'row' }, h('div', { style: { flex: '1' } }, field(`type ${n.hostname} to retire`, confirm)), go));
}
