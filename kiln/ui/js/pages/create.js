import { enc, get, post } from '../api.js';
import { fill, h } from '../dom.js';
import { gb, plural } from '../format.js';
import { isAdmin, session } from '../session.js';
import { act, button, empty, field, input, kv, notice, subtitle, title } from '../ui.js';
import { firstProject, projectUrl } from './common.js';

const SHAPES = [
  { id: 'smol', vcpus: 1, memory_mb: 1024 },
  { id: 'regular', vcpus: 2, memory_mb: 2048 },
  { id: 'chonky', vcpus: 4, memory_mb: 8192 },
];
const KEY_STORE = 'copper.ssh-key';

/** New VM in five steps. Nothing is created until "create" on the last one;
 *  the review step asks kiln for a dry run first. */
export default async function create() {
  if (!session.project) return firstProject();
  const [images, nodes] = await Promise.all([get('/v1/images'), isAdmin() ? get('/v1/nodes') : Promise.resolve([])]);
  let savedKey = '';
  try { savedKey = localStorage.getItem(KEY_STORE) ?? ''; } catch { /* fine */ }
  const want = {
    name: '', ssh_key: savedKey, image: images[0]?.name ?? 'ubuntu-24.04', arch: 'x86_64',
    shape: 'regular', vcpus: 2, memory_mb: 2048, disk_gb: 20, node: '', spread: '',
  };
  let step = 0;
  const steps = h('nav', { class: 'steps', 'aria-label': 'steps' });
  const panel = h('div', { class: 'pad stack' });
  const STEPS = [
    { key: 'name', summary: () => want.name || '—', draw: nameStep },
    { key: 'image', summary: () => `${want.image} · ${want.arch}`, draw: imageStep },
    { key: 'shape', summary: () => `${want.vcpus} vcpu · ${gb(want.memory_mb)} · ${want.disk_gb} gb`, draw: shapeStep },
    isAdmin() ? { key: 'placement', summary: () => (want.node ? `pinned` : want.spread ? `spread ${want.spread}` : 'copper picks'), draw: placementStep } : null,
    { key: 'review', summary: () => 'dry run', draw: reviewStep },
  ].filter(Boolean);
  const go = (i) => { step = i; draw(); };
  const next = (label) => button(`next · ${label} ›`, { primary: true, onclick: () => go(step + 1) });

  function draw() {
    fill(steps, STEPS.map((s, i) => h('button', { type: 'button', class: 'step', 'aria-current': i === step ? 'step' : null, onclick: () => go(i) },
      h('span', { class: 'n' }, String(i + 1).padStart(2, '0')), h('span', { class: 'k' }, s.key), h('span', { class: 'v' }, s.summary()))));
    fill(panel, STEPS[step].draw());
  }

  function nameStep() {
    const name = input({ value: want.name, placeholder: 'game-server', pattern: '[a-z0-9]([a-z0-9-]*[a-z0-9])?', autofocus: true });
    name.addEventListener('input', () => (want.name = name.value.trim()));
    const key = h('textarea', { class: 'input', rows: 3, style: { height: 'auto', padding: '10px 12px', resize: 'vertical' }, placeholder: 'ssh-ed25519 AAAA… you@laptop' }, want.ssh_key);
    key.addEventListener('input', () => (want.ssh_key = key.value.trim()));
    return [
      subtitle('Name.'),
      field('name', name, 'a-z 0-9 -'),
      field('project', input({ value: session.project, readonly: true })),
      field('ssh public key', key),
      h('div', { class: 'row' }, next('image')),
    ];
  }

  function imageStep() {
    const arches = isAdmin() ? [...new Set(nodes.filter((n) => n.status !== 'retired').map((n) => n.arch))] : ['x86_64', 'aarch64'];
    const names = [...new Set(images.map((i) => i.name))];
    return [
      subtitle('Image.'),
      names.length
        ? h('div', { class: 'choices' }, names.map((n) => choice(n, plural(images.filter((i) => i.name === n).length, 'arch', 'arches'), want.image === n, () => { want.image = n; draw(); })))
        : notice('the image catalogue is empty.', 'warm'),
      field('or type a name', textInput('image')),
      h('span', { class: 'label' }, 'architecture'),
      h('div', { class: 'choices' }, ['x86_64', 'aarch64'].map((a) => {
        const has = arches.includes(a);
        const inCatalogue = images.some((i) => i.name === want.image && i.arch === a);
        return choice(a, !has ? 'no such machine yet' : inCatalogue ? 'in the catalogue' : 'node-local image', want.arch === a, () => { want.arch = a; draw(); }, !has && isAdmin());
      })),
      h('div', { class: 'row' }, next('shape')),
    ];
  }

  function shapeStep() {
    return [
      subtitle('Shape.'),
      h('div', { class: 'choices' },
        SHAPES.map((s) => choice(s.id, `${s.vcpus} vcpu · ${gb(s.memory_mb)}`, want.shape === s.id, () => { Object.assign(want, { shape: s.id, vcpus: s.vcpus, memory_mb: s.memory_mb }); draw(); })),
        choice('custom', 'your call', want.shape === 'custom', () => { want.shape = 'custom'; draw(); })),
      h('div', { class: 'cells four', style: { background: 'none', gap: '16px' } },
        field('vcpu', numberInput('vcpus', 1)),
        field('memory, mb', numberInput('memory_mb', 128, 256)),
        field('boot disk, gb', numberInput('disk_gb', 1))),
      h('div', { class: 'row' }, next(isAdmin() ? 'placement' : 'review')),
    ];
  }

  function placementStep() {
    const pin = h('select', { class: 'input' }, h('option', { value: '' }, 'copper picks · most room wins'),
      nodes.filter((n) => n.status === 'online' && n.arch === want.arch).map((n) => h('option', { value: n.id, selected: want.node === n.id }, n.hostname)));
    pin.addEventListener('change', () => (want.node = pin.value));
    return [
      subtitle('Placement.'),
      field('pin to a node', pin),
      field('spread group', textInput('spread', 'games')),
      h('div', { class: 'row' }, next('review')),
    ];
  }

  function reviewStep() {
    const verdict = h('div', {}, h('p', { class: 'hint' }, 'checking capacity…'));
    const make = button(`create ${want.name || 'vm'} ›`, { primary: true, disabled: true });
    const body = request();
    if (!want.name) fill(verdict, notice('give it a name first, in step 01.', 'bad'));
    else post(projectUrl('/instances/check'), body).then((fit) => {
      fill(verdict, fitReport(fit));
      make.disabled = !fit.fits;
    }).catch((e) => fill(verdict, notice(e.message, 'bad')));
    make.addEventListener('click', async () => {
      const made = await act(make, () => post(projectUrl('/instances'), body), `${want.name} is starting`);
      if (made) {
        try { localStorage.setItem(KEY_STORE, want.ssh_key); } catch { /* fine */ }
        location.hash = `#/instances/${enc(made.name)}`;
      }
    });
    return [
      subtitle('Review.'),
      kv([['name', want.name || '—'], ['image', `${want.image} · ${want.arch}`], ['shape', `${want.vcpus} vcpu · ${want.memory_mb} MB · ${want.disk_gb} GB`],
        ['project', session.project], isAdmin() ? ['node', want.node ? nodes.find((n) => n.id === want.node)?.hostname : 'copper picks'] : null]),
      verdict,
      h('div', { class: 'row' }, button('cancel', { href: '#/instances' }), make),
    ];
  }

  function fitReport(fit) {
    return h('div', { class: 'stack' },
      fit.fits ? notice('fits.', 'good') : notice(fit.reason, 'bad'),
      fit.nodes
        ? h('div', {}, h('span', { class: 'label' }, 'fit per node'), fit.nodes.length
          ? fit.nodes.map((n) => h('div', { class: 'event', style: { gridTemplateColumns: '160px 90px minmax(0, 1fr)' } },
            h('span', { class: 'mono' }, n.hostname), h('span', { class: `kind ${n.fits ? '' : 'failed'}` }, n.status !== 'online' ? n.status : n.fits ? 'fits' : 'short'),
            h('span', { class: 'what' }, `${n.free.vcpus} vcpu · ${gb(n.free.memory_mb)} · ${n.free.disk_gb} gb free`)))
          : empty('no machine of this architecture.'))
        : h('p', { class: 'hint' }, `largest free on one machine: ${fit.largest.vcpus} vcpu · ${gb(fit.largest.memory_mb)} · ${fit.largest.disk_gb} gb`));
  }

  function request() {
    return {
      name: want.name, vcpus: want.vcpus, memory_mb: want.memory_mb, arch: want.arch, image: want.image, disk_gb: want.disk_gb,
      ssh_keys: want.ssh_key ? [want.ssh_key] : [],
      node: want.node || undefined, spread: want.spread || undefined,
    };
  }

  function textInput(key, placeholder) {
    const el = input({ value: want[key], placeholder });
    el.addEventListener('input', () => (want[key] = el.value.trim()));
    return el;
  }

  function numberInput(key, min, step = 1) {
    const el = input({ type: 'number', min, step, value: want[key] });
    el.addEventListener('input', () => { want[key] = Number(el.value); want.shape = 'custom'; });
    return el;
  }

  draw();
  return [
    h('section', {}, h('div', { class: 'head' }, h('div', { class: 'label' }, `instances / new`), title('New VM.', 'Pick a shape.'))),
    h('section', {}, h('div', { class: 'cells split' }, steps, panel)),
  ];
}

function choice(name, small, pressed, onclick, disabled) {
  return h('button', { type: 'button', class: 'choice', 'aria-pressed': pressed ? 'true' : 'false', onclick, disabled }, name, h('small', {}, small));
}
