import { enc, get, post } from '../api.js';
import { byRole, nodeNote, role, slab } from '../art.js';
import { fill, h } from '../dom.js';
import { ago, gb, percent, plural } from '../format.js';
import { isAdmin, session } from '../session.js';
import { act, button, copyBlock, dot, empty, field, fig, input, meter, notice, spark, st, title } from '../ui.js';

/** Machines as kiln knows them. Admin only: kiln returns 403 to others. */
export default async function nodes(ctx) {
  if (!isAdmin()) return h('section', {}, h('div', { class: 'pad' }, notice('admins only.')));
  const root = h('div', { style: { display: 'contents' } });
  const draw = async () => {
    // Don't redraw under someone typing a ratio or a name to retire.
    if (root.contains(document.activeElement) && document.activeElement.tagName === 'INPUT') return;
    const live = (await get('/v1/nodes')).filter((n) => n.status !== 'retired').sort(byRole);
    const samples = await Promise.all(live.map((n) => (n.status === 'online' ? get(`/v1/nodes/${enc(n.id)}/metrics`).catch(() => []) : [])));
    fill(root, ...build(live, samples, draw));
  };
  await draw();
  ctx.every(10000, draw);
  return root;
}

function build(live, samples, redraw) {
  const count = (s) => live.filter((n) => n.status === s).length;
  const pending = live.filter((n) => n.status === 'pending');
  return [
    h('section', {}, h('div', { class: 'head' },
      title('Nodes.', `${plural(live.length, 'machine')}${pending.length ? `, ${pending.length} waiting` : ''}.`),
      h('p', { class: 'hint', style: { margin: 0 } }, `${count('online')} online · ${count('offline')} offline · ${count('pending')} waiting`),
      pending.map((n) => banner(n, redraw)))),
    live.length
      ? h('section', {}, h('div', { class: 'cells two cards' },
        live.map((n, i) => card(n, i + 1, samples[i], redraw)),
        live.length % 2 ? h('div', { 'aria-hidden': 'true' }) : null))
      : h('section', {}, empty('no machines have registered yet.')),
  ];
}

/** A node that registered and is waiting. Let it in only after checking the
 *  key matches what ingot printed on that machine. */
function banner(n, redraw) {
  return h('div', { class: 'banner' },
    h('span', { class: 'who' }, dot('pending', true), `${n.hostname} wants in`),
    h('span', { class: 'meta' }, h('span', {}, `${n.arch} · node ${n.id.slice(0, 8)}`), h('span', {}, 'key ', h('b', {}, n.fingerprint ?? '—'))),
    h('span', { class: 'acts' }, turnAway(n, redraw), letIn(n, redraw)));
}

function letIn(n, redraw) {
  const go = button('let it in ›', { primary: true });
  go.addEventListener('click', async () => {
    await act(go, () => post(`/v1/nodes/${enc(n.id)}/approve`), `${n.hostname} can take work now`);
    redraw();
  });
  return go;
}

// Turning a node away retires its identity: it can never come back as this node.
function turnAway(n, redraw) {
  const away = button('turn away');
  away.addEventListener('click', async () => {
    if (!window.confirm(`turn ${n.hostname} (${n.id.slice(0, 8)}) away? it can never join as this node.`)) return;
    await act(away, () => post(`/v1/nodes/${enc(n.id)}/retire`), `${n.hostname} turned away`);
    redraw();
  });
  return away;
}

function card(n, index, samples, redraw) {
  const state = { online: 'online', offline: 'offline', pending: 'waiting' }[n.status];
  return h('div', { class: 'node' },
    h('div', { class: 'pad' }, fig(index, n.hostname, h('span', { class: `st ${n.status}` }, state, n.status === 'pending' ? null : h('span', { class: 'quiet' }, ` gen ${n.generation}`)))),
    h('figure', { class: 'scene' }, slab(n)),
    h('div', { class: 'pad stack', style: { gap: '28px' } },
      h('div', { class: 'node-top' },
        h('div', { class: 'stack', style: { gap: '10px' } },
          h('h3', { class: 'node-name' }, h('span', { class: 'mono' }, String(index).padStart(2, '0')), n.hostname),
          h('p', { class: 'node-sub' }, `${n.arch} · node ${n.id.slice(0, 8)}`)),
        h('ol', { class: 'facts' }, facts(n).map((f, i) => h('li', {}, h('span', { class: 'mono' }, `${index}.${i + 1}`), f)))),
      n.status === 'pending' ? waiting(n, redraw) : null,
      n.status === 'pending' ? null : held(n),
      n.status === 'online' ? network(n) : null,
      n.status === 'online' ? overcommit(n, redraw) : null,
      n.status === 'online' ? lives(samples) : null,
      n.status === 'offline' ? retire(n, redraw) : null));
}

function facts(n) {
  if (n.status === 'pending') return ['check the key'];
  const vms = n.instances.length ? vmNames(n) : 'no vms';
  if (n.status === 'offline') return [nodeNote(n), n.instances.length ? `${vms} frozen` : vms];
  return [role(n) === 'kiln' ? 'runs kiln' : null, vms].filter(Boolean);
}

/** The node's copper subnet, whether its host side (copper-net) is in place,
 *  and how it reaches each other node in the mesh. Nodes that don't run
 *  copper networks (Tart, for now) don't report any of it. */
function network(n) {
  if (!n.subnet || n.network_ready === undefined) return null;
  return h('div', { class: 'ocbox' },
    h('div', { class: 'vals' },
      h('span', { class: 'label' }, 'network'), h('b', {}, n.subnet),
      n.network_ready ? st('running', 'vms reach the internet') : st('failed', 'vms have no network')),
    n.network_ready ? null : copyBlock("copper-net.service isn't running here", ['$ sudo systemctl start copper-net']),
    ...(n.peers ?? []).map(peerPath));
}

/** One line per other node: the path of its last handshake, or none yet. */
function peerPath(p) {
  const path = { lan: 'direct on the lan', tailnet: 'over the tailnet' }[p.via];
  const handshake = p.handshake_age_s === undefined ? '' : ` · handshake ${ago(Math.floor(Date.now() / 1000) - p.handshake_age_s)} ago`;
  return h('div', { class: 'vals' },
    h('span', { class: 'label' }, `to ${p.hostname}`),
    path ? st('running', path + handshake) : st('stopped', 'no handshake yet'));
}

/** Kiln names them project/name; the current project's go without it. */
function vmNames(n) {
  const here = `${session.project}/`;
  return n.instances.map((i) => (i.startsWith(here) ? i.slice(here.length) : i)).join(', ');
}

function waiting(n, redraw) {
  return h('div', { class: 'stack', style: { gap: '22px' } },
    h('span', { class: 'hint' }, 'key ', h('b', { style: { color: 'var(--fg)', fontWeight: 500 } }, n.fingerprint ?? '—')),
    h('div', { class: 'row', style: { gap: '12px' } }, turnAway(n, redraw), letIn(n, redraw)));
}

/** What placement has handed out, against capacity × ratio. The hatched end
 *  is the stretch an overcommit ratio adds beyond the real hardware. */
function held(n) {
  const cap = n.capacity ?? { vcpus: 0, memory_mb: 0, disk_gb: 0 };
  const used = n.allocated ?? { vcpus: 0, memory_mb: 0, disk_gb: 0 };
  const oc = n.overcommit;
  const allow = { vcpus: Math.floor(cap.vcpus * oc.cpu), memory_mb: Math.floor(cap.memory_mb * oc.memory), disk_gb: Math.floor(cap.disk_gb * oc.disk) };
  const over = (k, real) => Math.max(0, allow[k] - real);
  const free = (k) => Math.max(0, allow[k] - used[k]);
  const anyOver = oc.cpu > 1 || oc.memory > 1 || oc.disk > 1;
  return h('div', { class: 'stack', style: { gap: '12px' } },
    meter('vcpu', used.vcpus, allow.vcpus, `${used.vcpus} used · ${free('vcpus')} free`, { extra: over('vcpus', cap.vcpus) }),
    meter('mem', used.memory_mb, allow.memory_mb, `${gb(used.memory_mb).replace(' gb', '')} used · ${gb(free('memory_mb')).replace(' gb', '')} free`, { extra: over('memory_mb', cap.memory_mb) }),
    meter('disk', used.disk_gb, allow.disk_gb, `${used.disk_gb} used · ${free('disk_gb')} free`, { extra: over('disk_gb', cap.disk_gb) }),
    h('div', { class: 'keyrow' },
      h('span', {}, h('i', { style: { background: 'var(--cu)' } }), 'used'),
      anyOver ? h('span', {}, h('i', { class: 'oc' }), 'overcommit') : null,
      h('span', {}, h('i', { style: { background: 'var(--l2)' } }), 'free')));
}

/** The ratios, and a form to change them behind "edit". */
function overcommit(n, redraw) {
  const oc = n.overcommit;
  const val = (k, v) => [h('span', {}, `${k} `, h('b', { class: v > 1 ? 'up' : '' }, `${v}×`))];
  const box = h('div', { class: 'ocbox' });
  const show = () => {
    const edit = button('edit', { small: true });
    edit.addEventListener('click', () => fill(box, form()));
    fill(box, h('div', { class: 'vals' }, h('span', { class: 'label' }, 'overcommit'), val('cpu', oc.cpu), val('mem', oc.memory), val('disk', oc.disk), h('span', { style: { marginLeft: 'auto' } }, edit)));
  };
  const form = () => {
    const cpu = input({ type: 'number', step: 0.25, min: 0.25, max: 64, value: oc.cpu });
    const mem = input({ type: 'number', step: 0.05, min: 0.25, max: 64, value: oc.memory });
    const disk = input({ type: 'number', step: 0.25, min: 0.25, max: 64, value: oc.disk });
    const save = button('save', { small: true, primary: true, type: 'submit' });
    const f = h('form', { class: 'stack', style: { gap: '14px' } },
      h('span', { class: 'label' }, 'overcommit'),
      h('div', { class: 'cells three flat' }, field('cpu ×', cpu), field('mem ×', mem), field('disk ×', disk)),
      h('div', { class: 'row', style: { gap: '8px' } }, button('cancel', { small: true, onclick: show }), save));
    f.addEventListener('submit', async (e) => {
      e.preventDefault();
      const ok = await act(save, () => post(`/v1/nodes/${enc(n.id)}/policy`, { cpu_ratio: Number(cpu.value), memory_ratio: Number(mem.value), disk_ratio: Number(disk.value) }), 'saved');
      if (ok !== undefined) {
        save.blur();
        redraw();
      }
    });
    return f;
  };
  show();
  return box;
}

/** The host as ingot last measured it, with the last hour drawn small. */
function lives(samples) {
  if (!samples.length) return null;
  const last = samples[samples.length - 1].usage;
  const frac = (used, total) => (total ? Math.round((used / total) * 1000) : 0);
  const cell = (name, now, values, kind) => h('div', { class: 'stack', style: { gap: '8px' } },
    h('span', { class: 'label' }, h('span', {}, name), h('span', { style: { color: 'var(--fg2)' } }, now)),
    spark(values, 1000, { kind, mini: true }));
  return h('div', { class: 'stack', style: { gap: '12px' } },
    h('span', { class: 'label' }, 'live'),
    h('div', { class: 'lives' },
      cell('cpu', percent(last.cpu_permille), samples.map((s) => s.usage.cpu_permille)),
      cell('mem', percent(frac(last.memory_used_bytes, last.memory_total_bytes)), samples.map((s) => frac(s.usage.memory_used_bytes, s.usage.memory_total_bytes)), 'pat'),
      cell('disk', percent(frac(last.disk_used_bytes, last.disk_total_bytes)), samples.map((s) => frac(s.usage.disk_used_bytes, s.usage.disk_total_bytes)))));
}

/** Retiring marks its vms failed and never moves them; type the name to confirm. */
function retire(n, redraw) {
  const box = h('div', { class: 'row', style: { gap: '14px' } });
  const ask = button('retire', { danger: true });
  ask.addEventListener('click', () => {
    const name = input({ placeholder: n.hostname, 'aria-label': `type ${n.hostname} to retire`, style: { width: '200px' } });
    const go = button(`retire ${n.hostname}`, { danger: true, disabled: true });
    name.addEventListener('input', () => (go.disabled = name.value !== n.hostname));
    go.addEventListener('click', async () => {
      await act(go, () => post(`/v1/nodes/${enc(n.id)}/retire`), `${n.hostname} retired`);
      redraw();
    });
    fill(box, name, go);
    name.focus();
  });
  fill(box, ask, n.instances.length ? h('span', { class: 'hint' }, `${vmNames(n)} → failed`) : null);
  return box;
}
