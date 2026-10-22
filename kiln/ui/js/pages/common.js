// Pieces several pages share: the instances table, project paths, and the
// "stop + start" reset for a failed VM.

import { enc, get, post } from '../api.js';
import { h } from '../dom.js';
import { since, shape } from '../format.js';
import { createProject, isAdmin, session } from '../session.js';
import { button, field, input, notice, stateBadge, table, toast } from '../ui.js';

export const projectUrl = (rest = '') => `/v1/projects/${enc(session.project)}${rest}`;
export const instanceUrl = (name, rest = '') => projectUrl(`/instances/${enc(name)}${rest}`);

export async function listInstances() {
  return session.project ? get(projectUrl('/instances')) : [];
}

/** Sorted by state, then name, like `cu ls`. */
export function instanceTable(instances) {
  const order = ['failed', 'unknown', 'starting', 'stopping', 'deleting', 'running', 'stopped'];
  const rows = [...instances].sort((a, b) => order.indexOf(a.state) - order.indexOf(b.state) || a.name.localeCompare(b.name));
  return table([
    { title: 'name', cell: (i) => h('a', { href: `#/instances/${enc(i.name)}` }, i.name) },
    { title: 'you see', cell: (i) => stateBadge(i.state) },
    isAdmin() ? { title: 'node', cell: (i) => i.node_name ?? '—', class: 'hide-sm' } : null,
    { title: 'shape', cell: (i) => shape(i), class: 'hide-sm' },
    { title: 'image', cell: (i) => i.image, class: 'hide-sm' },
    { title: 'address', cell: (i) => i.addresses[0] ?? '—' },
    { title: 'up', cell: (i) => (i.running_since ? since(i.running_since) : '—') },
  ].filter(Boolean), rows, (i) => (location.hash = `#/instances/${enc(i.name)}`));
}

/** A failed start stops retrying after five minutes. Stopping, then starting,
 *  resets that budget, which is the only retry copper has. */
export async function stopThenStart(name, onProgress = () => {}) {
  onProgress('stopping…');
  await post(instanceUrl(name, '/stop'));
  for (let i = 0; i < 30; i++) {
    const inst = await get(instanceUrl(name));
    if (inst.state === 'stopped') break;
    await new Promise((r) => setTimeout(r, 2000));
  }
  onProgress('starting…');
  await post(instanceUrl(name, '/start'));
  toast(`${name} is starting again`);
}

/** Shown when the signed-in person has no project yet. */
export function firstProject() {
  const name = input({ value: 'default', required: true });
  const form = h('form', { class: 'stack pad', style: { maxWidth: '420px' } },
    notice('you have no project yet.'),
    field('project name', name),
    button('make project ›', { primary: true, type: 'submit' }));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    try {
      await createProject(name.value.trim());
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    } catch (err) {
      toast(err.message, true);
    }
  });
  return h('section', {}, form);
}
