import { get } from '../api.js';
import { fill, h } from '../dom.js';
import { BUSY, gb, NEEDS_LOOK } from '../format.js';
import { currentProject, session } from '../session.js';
import { button, empty, input, title } from '../ui.js';
import { firstProject, instanceTable, listInstances } from './common.js';

const FILTERS = [
  { id: 'all', dot: null, test: () => true },
  { id: 'running', dot: 'running', test: (s) => s === 'running' },
  { id: 'busy', dot: 'starting', test: (s) => BUSY.has(s) },
  { id: 'need a look', dot: 'failed', test: (s) => NEEDS_LOOK.has(s) },
  { id: 'stopped', dot: 'stopped', test: (s) => s === 'stopped' },
];

/** Every instance in the project, filtered by state and name. */
export default async function instances(ctx) {
  if (!session.project) return firstProject();
  let filter = 'all';
  let all = [];
  let quota = null;
  const heading = h('div');
  const chips = h('div', { class: 'chips', role: 'group', 'aria-label': 'filter by state' });
  const search = input({ type: 'search', placeholder: 'find a vm', 'aria-label': 'find a vm', style: { width: '220px' } });
  const list = h('div');
  const tail = h('div', { class: 'tail' });

  const show = () => {
    const query = search.value.trim();
    const test = FILTERS.find((f) => f.id === filter).test;
    fill(chips, FILTERS.map((f) => h('button', {
      type: 'button', class: 'chip', 'aria-pressed': f.id === filter ? 'true' : 'false',
      onclick: () => { filter = f.id; show(); },
    }, f.dot ? h('span', { class: `dot ${f.dot}` }) : null, f.id, h('span', { class: 'n' }, String(all.filter((i) => f.test(i.state)).length)))));
    const rows = all.filter((i) => test(i.state) && i.name.includes(query));
    fill(list, rows.length ? instanceTable(rows, draw) : empty(all.length ? 'no vm matches.' : 'no vms in this project.'));
  };
  async function draw() {
    const here = currentProject();
    const [found, quotas] = await Promise.all([listInstances(), get('/v1/quotas')]);
    all = found;
    quota = quotas.find((q) => q.owner === here.owner && q.project === here.name);
    fill(heading, title('Instances.', `${all.length} in ${session.project}.`));
    fill(tail, held(quota), h('span'));
    show();
  }
  search.addEventListener('input', show);
  await draw();
  ctx.every(5000, draw);
  return [
    h('section', {}, h('div', { class: 'head row spread', style: { flexDirection: 'row', paddingBottom: '40px' } },
      heading, h('div', { class: 'row', style: { gap: '8px' } }, search, button('new vm ›', { primary: true, href: '#/new' })))),
    h('section', {}, h('div', { class: 'pad', style: { paddingTop: '40px' } }, chips), list, tail),
  ];
}

/** What the project holds, stopped instances included, against its caps. */
function held(q) {
  if (!q) return null;
  const caps = [q.caps.vcpus, q.caps.memory_mb && gb(q.caps.memory_mb).replace(' gb', ''), q.caps.disk_gb].map((c) => c || '—');
  const capped = q.caps.vcpus || q.caps.memory_mb || q.caps.disk_gb;
  return h('span', {},
    `${q.used.vcpus} vcpu · ${gb(q.used.memory_mb)} · ${q.used.disk_gb} gb held`,
    capped ? [' · ', h('a', { href: '#/quotas' }, `quota ${caps.join(' · ')} ›`)] : null);
}
