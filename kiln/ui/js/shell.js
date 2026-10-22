// The frame around every signed-in page: the rail on the left, the bordered
// column on the right, the element tile at the bottom.

import { del, get } from './api.js';
import { h } from './dom.js';
import { icon } from './icons.js';
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
  const waiting = h('span', { class: 'count' });
  if (isAdmin()) {
    get('/v1/nodes').then((nodes) => {
      const n = nodes.filter((x) => x.status === 'pending').length;
      if (n) waiting.append(dot('pending', true), String(n));
    }).catch(() => {});
  }
  const nav = h('nav', { class: 'nav', 'aria-label': 'primary' }, NAV.filter((n) => !n.admin || isAdmin()).map((n) => {
    if (n.group) return h('span', { class: 'group' }, n.group === 'project' ? (session.project ?? 'project') : n.group);
    return h('a', { href: n.href, 'aria-current': n.id === current ? 'page' : null },
      icon(n.id), h('span', {}, n.text), n.id === 'nodes' ? waiting : null);
  }));
  const port = location.port ? ` · :${location.port}` : '';
  const rail = h('aside', { class: 'rail' },
    h('a', { class: 'brand', href: '#/overview' }, logo(), 'copper'),
    h('div', { class: 'actions' }, projectPicker(), button('new vm ›', { primary: true, wide: true, href: '#/new' })),
    nav,
    h('div', { class: 'foot' },
      h('span', { class: 'kiln-pill' }, dot('online'), 'kiln', h('small', {}, `${session.kiln?.hostname ?? location.hostname}${port}`)),
      h('div', { class: 'whoami' },
        h('span', {}, `${session.me.name} · ${session.me.role}`),
        h('button', { class: 'linkish', type: 'button', onclick: signOut }, 'sign out'))));
  return h('div', { class: 'shell' }, rail,
    h('main', {}, h('div', { class: 'frame' }, content, footer())));
}

export const logo = () => h('span', { class: 'logo', 'aria-hidden': 'true' }, h('span'), h('span'), h('span'));

function footer() {
  return h('footer', { class: 'footer' },
    h('div', { class: 'element', 'aria-hidden': 'true' }, h('span', {}, '29'), h('b', {}, 'Cu'), h('span', {}, '63.546')),
    h('p', {}, `copper v${session.kiln?.version ?? ''}`),
    h('p', {}, 'kiln · ingot · cu'));
}

function projectPicker() {
  if (!session.projects.length) return null;
  const select = h('select', { 'aria-label': 'project' }, session.projects.map((p) => {
    const path = projectPath(p);
    return h('option', { value: path, selected: path === session.project }, path);
  }));
  select.addEventListener('change', () => {
    setProject(select.value);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  });
  return h('label', { class: 'picker' }, h('span', {}, 'project'), select);
}

async function signOut() {
  try { await del('/v1/session'); } catch { /* the cookie may already be gone */ }
  forget();
  location.hash = '#/signin';
}
