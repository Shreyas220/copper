import { enc, get } from '../api.js';
import { h } from '../dom.js';
import { bytes } from '../format.js';
import { isAdmin, projectPath, session } from '../session.js';
import { empty, table, title } from '../ui.js';

/** Kiln's image catalogue, and which of the instances you can see use each. */
export default async function images() {
  const [catalogue, instances] = await Promise.all([get('/v1/images'), visibleInstances()]);
  const usedBy = (img) => instances.filter((i) => i.image === img.name && i.arch === img.arch);
  return [
    h('section', {}, h('div', { class: 'head' }, title('Images.', `${catalogue.length} in the catalogue.`))),
    h('section', {}, h('div', { style: { paddingTop: '40px', paddingBottom: '28px' } }, catalogue.length
      ? table([
        { title: 'name', cell: (i) => i.name, class: 'name' },
        { title: 'arch', cell: (i) => i.arch },
        { title: 'size', cell: (i) => bytes(i.size_bytes) },
        { title: 'sha256', cell: (i) => h('span', { class: 'quiet', title: i.sha256 }, `${i.sha256.slice(0, 4)}…${i.sha256.slice(-4)}`), class: 'hide-sm' },
        { title: 'used by', cell: (i) => users(usedBy(i)) },
      ], catalogue)
      : empty('the catalogue is empty.'))),
  ];
}

function users(list) {
  if (!list.length) return h('span', { class: 'quiet' }, 'none yet');
  return h('span', { class: 'links' }, list.map((i, n) => [n ? ', ' : '',
    i.project === session.project ? h('a', { href: `#/instances/${enc(i.name)}` }, i.name) : `${i.project}/${i.name}`]));
}

/** Admins see every project's instances; others only their own. */
async function visibleInstances() {
  const projects = isAdmin() ? session.projects : session.projects.filter((p) => p.owner === session.me.name);
  const lists = await Promise.all(projects.map(async (p) => {
    const path = projectPath(p);
    const found = await get(`/v1/projects/${enc(path)}/instances`).catch(() => []);
    return found.map((i) => ({ ...i, project: path }));
  }));
  return lists.flat();
}
