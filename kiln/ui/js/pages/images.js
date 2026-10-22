import { enc, get } from '../api.js';
import { h } from '../dom.js';
import { bytes, plural } from '../format.js';
import { isAdmin, projectPath, session } from '../session.js';
import { empty, table, title } from '../ui.js';

/** Kiln's image catalogue, and which of the instances you can see use each. */
export default async function images() {
  const [catalogue, instances] = await Promise.all([get('/v1/images'), visibleInstances()]);
  const usedBy = (img) => instances.filter((i) => i.image === img.name && i.arch === img.arch).map((i) => i.name);
  return [
    h('section', {}, h('div', { class: 'head' }, title('Images.', `${plural(catalogue.length, 'entry', 'entries')} in the catalogue.`))),
    h('section', {}, catalogue.length
      ? table([
        { title: 'name', cell: (i) => i.name },
        { title: 'arch', cell: (i) => i.arch },
        { title: 'size', cell: (i) => bytes(i.size_bytes) },
        { title: 'sha256', cell: (i) => h('span', { title: i.sha256 }, `${i.sha256.slice(0, 4)}…${i.sha256.slice(-4)}`), class: 'hide-sm' },
        { title: 'used by', cell: (i) => usedBy(i).join(', ') || '—' },
      ], catalogue)
      : empty('the catalogue is empty.')),
  ];
}

/** Admins see every project's instances; others only their own. */
async function visibleInstances() {
  const projects = isAdmin() ? session.projects : session.projects.filter((p) => p.owner === session.me.name);
  const lists = await Promise.all(projects.map((p) => get(`/v1/projects/${enc(projectPath(p))}/instances`).catch(() => [])));
  return lists.flat();
}
