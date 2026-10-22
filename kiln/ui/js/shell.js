// The frame around every signed-in page: the rail on the left, the bordered
// column on the right.

import { del, get } from './api.js';
import { h } from './dom.js';
import { forget, isAdmin, projectPath, session, setProject } from './session.js';
import { button, dot } from './ui.js';

// Only pages backed by a kiln endpoint.
const NAV = [
  { group: 'project' },
  { id: 'overview', text: 'overview', href: '#/overview' },
  { id: 'instances', text: 'instances', href: '#/instances' },
  { id: 'images', text: 'images', href: '#/images' },
  { group: 'system' },
  { id: 'nodes', text: 'nodes', href: '#/nodes', admin: true },
  { id: 'quotas', text: 'quotas', href: '#/quotas' },
  { id: 'people', text: 'users & tokens', href: '#/people' },
];

export function shell(current, content) {
  const pending = h('span', { class: 'row', style: { gap: '6px', color: 'var(--cu)' } });
  if (isAdmin()) {
    get('/v1/nodes').then((nodes) => {
      const n = nodes.filter((x) => x.status === 'pending').length;
      if (n) pending.append(dot('pending blink'), String(n));
    }).catch(() => {});
  }
  const nav = h('nav', { class: 'nav', 'aria-label': 'primary' }, NAV.filter((n) => !n.admin || isAdmin()).map((n) => {
    if (n.group) return h('span', { class: 'group' }, n.group === 'project' ? (session.project ?? 'project') : n.group);
    return h('a', { href: n.href, 'aria-current': n.id === current ? 'page' : null },
      h('span', {}, n.text),
      n.id === 'nodes' ? pending : null);
  }));
  const rail = h('aside', { class: 'rail' },
    h('a', { class: 'brand', href: '#/overview' }, logo(), 'copper'),
    h('div', { class: 'actions' }, projectPicker(), button('new vm ›', { primary: true, wide: true, href: '#/new' })),
    nav,
    h('div', { class: 'foot' },
      h('span', { class: 'kiln-pill' }, dot('online'), 'kiln', h('span', { class: 'quiet', style: { textTransform: 'none' } }, location.host)),
      h('div', { class: 'whoami' },
        h('span', {}, `${session.me.name} · ${session.me.role}`),
        h('button', { class: 'linkish', type: 'button', onclick: signOut }, 'sign out'))));
  return h('div', { class: 'shell' }, rail,
    h('main', {}, h('div', { class: 'frame' }, content,
      h('div', { class: 'footer' }, h('span', {}, 'copper · kiln · ingot · cu'), h('span', {}, location.host)))));
}

export const logo = () => h('span', { class: 'logo', 'aria-hidden': 'true' }, h('span'), h('span'), h('span'));

function projectPicker() {
  const select = h('select', { class: 'input', 'aria-label': 'project', style: { height: '40px', fontSize: '12px' } },
    session.projects.map((p) => {
      const path = projectPath(p);
      return h('option', { value: path, selected: path === session.project }, path);
    }));
  select.addEventListener('change', () => {
    setProject(select.value);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  });
  return session.projects.length ? select : null;
}

async function signOut() {
  try { await del('/v1/session'); } catch { /* the cookie may already be gone */ }
  forget();
  location.hash = '#/signin';
}
