// Routes and the page lifecycle. A page is `async (ctx, ...params) => Node`.
// `ctx.every(ms, fn)` runs fn on a timer until the user navigates away.

import { h } from './dom.js';
import { loadSession, session } from './session.js';
import { shell } from './shell.js';
import { notice } from './ui.js';

import create from './pages/create.js';
import images from './pages/images.js';
import instance from './pages/instance.js';
import instances from './pages/instances.js';
import invite from './pages/invite.js';
import nodes from './pages/nodes.js';
import overview from './pages/overview.js';
import people from './pages/people.js';
import quotas from './pages/quotas.js';
import signin from './pages/signin.js';

const ROUTES = [
  { path: /^\/signin$/, page: signin, bare: true },
  { path: /^\/invite\/([0-9a-f]{32})$/, page: invite, bare: true },
  { path: /^\/(?:overview)?$/, page: overview, nav: 'overview' },
  { path: /^\/instances$/, page: instances, nav: 'instances' },
  { path: /^\/instances\/([a-z0-9-]+)$/, page: instance, nav: 'instances' },
  { path: /^\/new$/, page: create, nav: 'instances' },
  { path: /^\/images$/, page: images, nav: 'images' },
  { path: /^\/nodes$/, page: nodes, nav: 'nodes' },
  { path: /^\/quotas$/, page: quotas, nav: 'quotas' },
  { path: /^\/people$/, page: people, nav: 'people' },
];

const app = document.getElementById('app');
let cleanups = [];
let rendering = 0;

async function render() {
  const mine = ++rendering;
  cleanups.forEach((fn) => fn());
  cleanups = [];
  const path = location.hash.replace(/^#/, '') || '/';
  const found = ROUTES.map((r) => [r, path.match(r.path)]).find(([, m]) => m);
  if (!found) {
    location.hash = '#/overview';
    return;
  }
  const [route, match] = found;
  const ctx = {
    every(ms, fn) {
      const timer = setInterval(fn, ms);
      cleanups.push(() => clearInterval(timer));
    },
    onLeave(fn) {
      cleanups.push(fn);
    },
  };
  try {
    if (!route.bare && !session.me) await loadSession();
    const content = await route.page(ctx, ...match.slice(1).map(decodeURIComponent));
    if (mine !== rendering) return; // the user already moved on
    app.replaceChildren(route.bare ? content : shell(route.nav, content));
  } catch (e) {
    if (e.status === 401) return; // api.js already sent us to sign in
    app.replaceChildren(h('div', { class: 'door' }, notice(e.message, 'bad')));
  }
  window.scrollTo(0, 0);
}

window.addEventListener('hashchange', render);
render();
