import { enc, get } from '../api.js';
import { stack, nodeNote } from '../art.js';
import { fill, h } from '../dom.js';
import { ago, BUSY, gb, NEEDS_LOOK, plural } from '../format.js';
import { currentProject, isAdmin, session } from '../session.js';
import { button, dot, empty, fig, meter, subtitle, title } from '../ui.js';
import { firstProject, instanceTable, listInstances, stopThenStart } from './common.js';

/** Overview: the machines, what's running, what needs a look, and every
 *  instance in the project. Machines are admin-only, as kiln sends them. */
export default async function overview(ctx) {
  if (!session.project) return firstProject();
  const root = h('div', { style: { display: 'contents' } });
  const draw = async () => fill(root, ...(await sections()));
  await draw();
  ctx.every(5000, draw);
  return root;
}

async function sections() {
  const [instances, quotas, events, nodes] = await Promise.all([
    listInstances(),
    get('/v1/quotas'),
    get(`/v1/events?project=${enc(session.project)}&limit=40`),
    isAdmin() ? get('/v1/nodes') : Promise.resolve(null),
  ]);
  const count = (pred) => instances.filter((i) => pred(i.state)).length;
  const running = count((s) => s === 'running');
  const look = instances.filter((i) => NEEDS_LOOK.has(i.state));
  const live = (nodes ?? []).filter((n) => n.status !== 'retired');
  const pending = live.filter((n) => n.status === 'pending');
  const attention = look.length + pending.length;
  const headline = instances.length || live.length
    ? `${running} running${attention ? `, ${attention} need${attention === 1 ? 's' : ''} a look` : ''}.`
    : 'Nothing here yet.';
  const here = currentProject();
  const quota = quotas.find((q) => q.owner === here.owner && q.project === here.name);
  return [
    h('section', {}, h('div', { class: 'head' }, title('Overview.', headline))),
    live.length ? h('section', {}, h('div', { class: 'scene-wrap' }, stack(live), h('span', { class: 'label fig0' }, 'FIG.0'))) : null,
    h('section', {}, h('div', { class: 'cells four' },
      stat('running', running, 'running'),
      stat('busy', count((s) => BUSY.has(s)), 'starting', true),
      stat('need a look', attention, 'failed'),
      quotaCell(quota))),
    h('section', {}, h('div', { class: 'cells two' },
      nodes ? h('div', { class: 'pad stack' }, fig(1, 'your cloud'), wiring(live)) : null,
      h('div', { class: 'pad stack' }, fig(nodes ? 2 : 1, 'needs a look'), needsLook(look, pending, events)))),
    h('section', {},
      h('div', { class: 'head' }, h('div', { class: 'row spread' },
        subtitle('Instances.', `${plural(instances.length, 'vm')} in ${session.project}.`),
        button('new vm ›', { primary: true, href: '#/new' }))),
      instances.length ? instanceTable(instances) : empty('no vms in this project.')),
  ];
}

function stat(name, n, state, blink) {
  return h('div', { class: 'stat' }, h('span', { class: 'label row' }, dot(state, blink && n > 0), name), h('span', { class: 'num' }, String(n)));
}

function quotaCell(q) {
  const box = h('div', { class: 'stat' }, h('a', { class: 'label', href: '#/quotas', style: { textDecoration: 'none' } }, `quota · ${session.project} ›`));
  if (!q || (!q.caps.vcpus && !q.caps.memory_mb && !q.caps.disk_gb)) {
    box.append(h('span', { class: 'hint' }, 'no caps on this project'));
    return box;
  }
  box.append(h('div', { class: 'stack', style: { gap: '10px' } },
    q.caps.vcpus ? meter('vcpu', q.used.vcpus, q.caps.vcpus, `${q.used.vcpus} / ${q.caps.vcpus}`) : null,
    q.caps.memory_mb ? meter('mem', q.used.memory_mb, q.caps.memory_mb, `${gb(q.used.memory_mb)} / ${gb(q.caps.memory_mb)}`) : null,
    q.caps.disk_gb ? meter('disk', q.used.disk_gb, q.caps.disk_gb, `${q.used.disk_gb} / ${q.caps.disk_gb} gb`) : null));
  return box;
}

/** FIG.1: kiln at the address this page came from, and an ingot per node. */
function wiring(nodes) {
  if (!nodes.length) return empty('no machines have registered yet.');
  const cols = Math.min(nodes.length, 4);
  return h('div', { class: 'wire' },
    h('span', { class: 'label' }, location.host),
    h('div', { class: 'kiln' }, h('span', { class: 'box' }, dot('starting'), 'kiln', h('small', {}, session.kiln?.hostname ?? ''))),
    h('div', { class: 'trunk', 'aria-hidden': 'true' }, h('span', { class: 'packet' })),
    h('div', { class: 'ingots', style: { gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`, '--edge': `calc(${50 / cols}% - 5px)` } },
      nodes.map((n) => h('a', { class: `ingot ${n.status}`, href: '#/nodes' },
        h('div', {}, h('span', { class: 'row', style: { gap: '7px' } }, dot(n.status, n.status === 'pending'), 'ingot'),
          h('small', {}, n.hostname, h('br'), nodeNote(n)))))));
}

/** Failed and unknown instances and waiting nodes, with what to do next. */
function needsLook(look, pending, events) {
  const rows = [];
  for (const n of pending) {
    rows.push(item('—', n.hostname, `wants in · key ${n.fingerprint ?? '—'}`, button('let it in ›', { small: true, href: '#/nodes' })));
  }
  for (const i of look) {
    const why = events.find((e) => e.instance === i.name && e.kind === 'failed');
    if (i.state === 'failed') {
      const btn = button('stop + start', { small: true });
      btn.addEventListener('click', async () => {
        btn.disabled = true;
        try { await stopThenStart(i.name, (t) => (btn.textContent = t)); } finally { btn.disabled = false; }
      });
      rows.push(item(why ? ago(why.at) : '—', i.name, `failed: ${i.error ?? why?.detail ?? 'no reason given'}`, btn));
    } else {
      rows.push(item('—', i.name, 'unknown · its node is not reporting', button('look ›', { small: true, href: `#/instances/${enc(i.name)}` })));
    }
  }
  return rows.length ? h('div', {}, rows) : empty('nothing needs a look.');
}

function item(when, name, what, action) {
  return h('div', { class: 'event', style: { gridTemplateColumns: '48px minmax(0, 1fr) auto', alignItems: 'center' } },
    h('span', { class: 'when' }, when),
    h('span', {}, h('span', { class: 'mono' }, name), h('br'), h('span', { class: 'what' }, what)),
    action);
}
