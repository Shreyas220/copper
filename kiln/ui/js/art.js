// The design's halftone artwork, picked from what kiln reports. A drawing
// only appears when it is true of the machines it stands for.

import { h } from './dom.js';
import { ago, plural } from './format.js';
import { session } from './session.js';
import { dot } from './ui.js';

/** How a node is drawn: the kiln machine in copper, other online nodes in
 *  bronze, offline ones grey, waiting ones as a dotted outline. */
export function role(node) {
  if (node.status === 'online' && node.hostname === session.kiln?.hostname) return 'kiln';
  return { online: 'online', offline: 'offline', pending: 'waiting' }[node.status] ?? null;
}

const ALT = {
  kiln: 'copper slab: the machine running kiln',
  online: 'bronze slab: an online node',
  offline: 'grey slab with its light off: a node that stopped reporting',
  waiting: 'dotted outline: a node waiting to be let in',
};

export const slab = (node) => h('img', { class: 'art', src: `/ui/art/slab-${role(node)}.png`, alt: `${node.hostname}, ${ALT[role(node)]}` });

/** The text on a node's callout: what kiln says about it. */
export function nodeNote(n) {
  if (n.status === 'pending') return 'waiting';
  if (n.status === 'offline') return n.last_seen ? `quiet ${ago(n.last_seen)}` : 'never reported';
  return `${role(n) === 'kiln' ? 'kiln · ' : ''}${plural(n.instances.length, 'vm')}`;
}

// Stack drawings from the design, each true for exactly these roles in this
// order. Positions are percentages of the drawing.
const SCENES = [
  { roles: ['kiln'], src: 'stack-1', at: [{ left: 69.47, top: 44.6 }] },
  { roles: ['kiln', 'online'], src: 'stack-2', at: [{ left: 69.05, top: 34.4 }, { left: 69.05, top: 54.4 }] },
  { roles: ['kiln', 'online', 'waiting'], src: 'stack-3', at: [{ left: 68.63, top: 27.0 }, { left: 68.63, top: 44.2 }, { left: 68.63, top: 61.4 }], light: { left: 65.4, top: 61.24 } },
  { roles: ['kiln', 'online', 'offline', 'waiting'], src: 'stack-4', at: [{ right: 76.13, top: 36.4 }, { right: 76.13, top: 53.6 }, { left: 76.13, top: 36.4 }, { left: 76.13, top: 53.6 }], light: { left: 73.97, top: 53.44 } },
];
const ORDER = ['kiln', 'online', 'offline', 'waiting'];

/** Kiln's machine first, then online, offline and waiting ones, by name. */
export const byRole = (a, b) => ORDER.indexOf(role(a)) - ORDER.indexOf(role(b)) || a.hostname.localeCompare(b.hostname);

/** FIG.0: the machines as one drawing, or one slab each when no drawing
 *  matches them. */
export function stack(nodes) {
  const drawn = nodes.filter((n) => role(n)).sort(byRole);
  const roles = drawn.map(role).join(' ');
  const scene = SCENES.find((s) => s.roles.join(' ') === roles);
  const legend = h('nav', { class: 'legend', 'aria-label': 'machines' }, drawn.map((n, i) =>
    h('a', { href: '#/nodes' }, dot(n.status, n.status === 'pending'), `${String(i + 1).padStart(2, '0')} ${n.hostname}`, h('span', {}, nodeNote(n)))));
  if (!scene) {
    return h('div', {},
      h('div', { class: 'slab-grid', style: { '--n': Math.min(drawn.length, 4) } }, drawn.map((n, i) => h('figure', { class: 'scene' }, slab(n), callout(n, i, { left: 50, top: 78, center: true })))),
      legend);
  }
  return h('div', {},
    h('figure', { class: 'scene' },
      h('img', { class: 'art', src: `/ui/art/${scene.src}.png`, alt: `halftone drawing of ${plural(drawn.length, 'machine')}: ${drawn.map((n) => `${n.hostname} ${role(n)}`).join(', ')}` }),
      scene.light ? h('span', { class: 'light blink', 'aria-hidden': 'true', style: { left: `${scene.light.left}%`, top: `${scene.light.top}%` } }) : null,
      drawn.map((n, i) => callout(n, i, scene.at[i]))),
    legend);
}

/** A label pinned to a point of a drawing by a short leader line. */
export function callout(n, i, at) {
  const pos = { top: `${at.top}%` };
  if (at.right != null) pos.right = `${at.right}%`;
  else pos.left = `${at.left}%`;
  return h('a', { class: `callout${at.right != null ? ' flip' : ''}${at.center ? ' center' : ''}`, href: '#/nodes', style: pos },
    h('span', { class: 'leader', 'aria-hidden': 'true' }), h('span', { class: 'pin', 'aria-hidden': 'true' }),
    h('span', { class: 'tagbox' },
      h('span', {}, dot(n.status, n.status === 'pending'), `${String(i + 1).padStart(2, '0')} ${n.hostname}`),
      h('small', {}, nodeNote(n))));
}
