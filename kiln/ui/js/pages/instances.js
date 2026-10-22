import { fill, h } from '../dom.js';
import { plural } from '../format.js';
import { session } from '../session.js';
import { button, empty, title } from '../ui.js';
import { firstProject, instanceTable, listInstances } from './common.js';

export default async function instances(ctx) {
  if (!session.project) return firstProject();
  const list = h('div');
  const head = h('div', { class: 'head' });
  const draw = async () => {
    const all = await listInstances();
    fill(head, h('div', { class: 'row spread' }, title('Instances.', `${plural(all.length, 'vm')} in ${session.project}.`), button('new vm ›', { primary: true, href: '#/new' })));
    fill(list, all.length ? instanceTable(all) : empty('no vms yet.'));
  };
  await draw();
  ctx.every(5000, draw);
  return [h('section', {}, head), h('section', {}, list)];
}
