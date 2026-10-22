// Pieces several pages share: the instances table, project paths, and the
// "stop + start" reset for a failed VM.

import { enc, get, post } from '../api.js';
import { h } from '../dom.js';
import { shape, since } from '../format.js';
import { createProject, isAdmin, session } from '../session.js';
import { button, field, input, notice, stateBadge, table, toast } from '../ui.js';

export const projectUrl = (rest = '') => `/v1/projects/${enc(session.project)}${rest}`;
export const instanceUrl = (name, rest = '') => projectUrl(`/instances/${enc(name)}${rest}`);

export async function listInstances() {
  return session.project ? get(projectUrl('/instances')) : [];
}

/** Sorted by state, then name, like `cu ls`. With `onChange`, each row also
 *  gets the action its state allows. */
export function instanceTable(instances, onChange) {
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
    onChange ? { title: '', cell: (i) => rowAction(i, onChange) } : null,
  ].filter(Boolean), rows, (i) => (location.hash = `#/instances/${enc(i.name)}`));
}

/** The one thing kiln lets you do from here in each state. */
function rowAction(i, onChange) {
  const run = (text, fn) => {
    const btn = button(text, { small: true });
    btn.addEventListener('click', async (e) => {
      e.stopPropagation();
      btn.disabled = true;
      try {
        await fn((t) => (btn.textContent = t));
        onChange();
      } catch (err) {
        toast(err.message, true);
        btn.disabled = false;
      }
    });
    return btn;
  };
  switch (i.state) {
    case 'running':
    case 'starting':
      return run('stop', () => post(instanceUrl(i.name, '/stop')));
    case 'stopped':
      return run('start', () => post(instanceUrl(i.name, '/start')));
    case 'failed':
      return run('stop + start', (progress) => stopThenStart(i.name, progress));
    case 'unknown':
      return button('look ›', { small: true, href: `#/instances/${enc(i.name)}` });
    default:
      return null;
  }
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
