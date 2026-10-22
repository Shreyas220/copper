import { enc, get, post } from '../api.js';
import { fill, h } from '../dom.js';
import { gb, plural } from '../format.js';
import { isAdmin, projectPath, session } from '../session.js';
import { act, button, copyBlock, crumb, dot, fig, input, kv, notice, st, title } from '../ui.js';
import { firstProject, instanceHref } from './common.js';

const SHAPES = [
  { id: 'smol', vcpus: 1, memory_gb: 1 },
  { id: 'regular', vcpus: 2, memory_gb: 2 },
  { id: 'chonky', vcpus: 4, memory_gb: 8 },
];
const NAME = /^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/;
const KEY_STORE = 'copper.ssh-key';

/** New VM, as one page of steps. Every change asks kiln for a fresh dry run;
 *  nothing is created until the button at the bottom. */
export default async function create(ctx) {
  if (!session.project) return firstProject();
  const admin = isAdmin();
  const [images, nodes, inUse] = await Promise.all([get('/v1/images'), admin ? get('/v1/nodes') : Promise.resolve([]), imageUse()]);
  let savedKey = '';
  try { savedKey = localStorage.getItem(KEY_STORE) ?? ''; } catch { /* no storage: type it */ }
  // Start from the image most VMs here already use.
  const usual = [...images].sort((a, b) => (inUse[b.name] ?? 0) - (inUse[a.name] ?? 0))[0];
  const want = {
    name: '', ssh_key: savedKey, project: session.project, image: usual?.name ?? 'ubuntu-24.04', arch: usual?.arch ?? 'x86_64',
    shape: 'regular', vcpus: 2, memory_gb: 2, disk_gb: 20, placement: 'pick', node: '', spread: '',
  };
  let fit = null;
  const keys = ['name', 'image', 'shape', admin ? 'placement' : null, 'review'].filter(Boolean);
  let current = keys[0];
  const blocks = {};
  const goTo = (key) => blocks[key].scrollIntoView({ behavior: 'smooth', block: 'start' });
  const next = (key) => button(`next · ${keys[keys.indexOf(key) + 1]} ›`, { onclick: () => goTo(keys[keys.indexOf(key) + 1]) });
  const changed = () => { recheck(); update(); };

  // ---- 01 name ----
  const name = input({ placeholder: 'game-server', autofocus: true });
  name.addEventListener('input', () => {
    want.name = name.value.trim();
    name.setAttribute('aria-invalid', want.name && !NAME.test(want.name) ? 'true' : 'false');
    changed();
  });
  const project = h('select', { class: 'input' }, session.projects.map((p) => h('option', { value: projectPath(p), selected: projectPath(p) === want.project }, projectPath(p))));
  project.addEventListener('change', () => { want.project = project.value; changed(); });
  const key = input({ value: want.ssh_key, placeholder: 'ssh-ed25519 AAAA… you@laptop', spellcheck: 'false' });
  key.addEventListener('input', () => (want.ssh_key = key.value.trim()));
  blocks.name = h('div', { class: 'step' },
    h('div', { class: 'cells three flat' }, field('name', name, h('span', { class: 'hint' }, 'a-z 0-9 -')), field('project', project), field('ssh key', key)),
    h('div', { class: 'row' }, next('name')));

  // ---- 02 image ----
  const names = [...new Set(images.map((i) => i.name))];
  const imageList = h('div', { class: 'radios', role: 'radiogroup', 'aria-label': 'image' });
  const drawImages = () => fill(imageList, names.map((n) => radio(n, null, h('span', { class: 'aside' }, plural(inUse[n] ?? 0, 'vm')), want.image === n,
    () => { want.image = n; drawImages(); changed(); })));
  drawImages();
  const online = new Set(nodes.filter((n) => n.status === 'online').map((n) => n.arch));
  const waitingArm = nodes.find((n) => n.status === 'pending' && n.arch === 'aarch64');
  const archList = h('div', { class: 'choices' });
  const drawArches = () => fill(archList,
    ['x86_64', 'aarch64'].map((a) => h('button', { type: 'button', class: 'choice', 'aria-pressed': want.arch === a ? 'true' : 'false', disabled: admin && !online.has(a) && want.arch !== a,
      onclick: () => { want.arch = a; drawArches(); changed(); } }, a)),
    waitingArm && !online.has('aarch64') ? h('a', { href: '#/nodes', class: 'hint warm-link' }, dot('pending'), ` ${waitingArm.hostname} waiting ›`) : null);
  drawArches();
  blocks.image = h('div', { class: 'step' },
    h('span', { class: 'label' }, 'image'),
    names.length ? imageList : notice('the image catalogue is empty.', 'warm'),
    h('span', { class: 'label' }, 'architecture'),
    archList,
    h('div', { class: 'row' }, next('image')));

  // ---- 03 shape ----
  const num = (k) => {
    const el = input({ type: 'number', min: 1, value: want[k] });
    el.addEventListener('input', () => { want[k] = Number(el.value); want.shape = 'custom'; drawShapes(); changed(); });
    return el;
  };
  const sizes = { vcpus: num('vcpus'), memory_gb: num('memory_gb'), disk_gb: num('disk_gb') };
  const hints = { vcpus: h('span', { class: 'hint' }), memory_gb: h('span', { class: 'hint' }), disk_gb: h('span', { class: 'hint' }) };
  const shapeList = h('div', { class: 'cards-pick' });
  const pick = (s) => {
    Object.assign(want, { shape: s.id, vcpus: s.vcpus, memory_gb: s.memory_gb });
    sizes.vcpus.value = s.vcpus;
    sizes.memory_gb.value = s.memory_gb;
    drawShapes();
    changed();
  };
  const drawShapes = () => fill(shapeList,
    SHAPES.map((s) => card(s.id, `${s.vcpus} vcpu · ${s.memory_gb} gb`, want.shape === s.id, () => pick(s))),
    card('custom', 'your call', want.shape === 'custom', () => { want.shape = 'custom'; drawShapes(); sizes.vcpus.focus(); }));
  drawShapes();
  const fitBox = h('div');
  const refusal = h('div');
  blocks.shape = h('div', { class: 'step' },
    shapeList,
    h('div', { class: 'cells three flat' },
      field('vcpu', sizes.vcpus, hints.vcpus), field('memory, gb', sizes.memory_gb, hints.memory_gb), field('boot disk, gb', sizes.disk_gb, hints.disk_gb)),
    fitBox, refusal,
    h('div', { class: 'row' }, next('shape')));

  // ---- 04 placement, admins only: kiln ignores hints from anyone else ----
  if (admin) {
    const pin = h('select', { class: 'input small' }, nodes.filter((n) => n.status === 'online').map((n) => h('option', { value: n.id }, n.hostname)));
    const group = input({ class: 'input small', placeholder: 'games', style: { width: '120px' } });
    const choose = (p) => () => {
      want.placement = p;
      if (p === 'pin') want.node = pin.value;
      for (const [k, el] of Object.entries(options)) el.setAttribute('aria-checked', k === p ? 'true' : 'false');
      changed();
    };
    // Built once and only re-marked, so typing a group name keeps focus.
    const options = {
      pick: radio('copper picks', 'most room wins', null, true, choose('pick')),
      pin: radio('pin to a node', 'refuses if it won’t fit there', h('span', { class: 'aside' }, pin, ' --node'), false, choose('pin')),
      spread: radio('spread group', 'apart from others in the group', h('span', { class: 'aside' }, group, ' --spread'), false, choose('spread')),
    };
    pin.addEventListener('change', choose('pin'));
    group.addEventListener('input', () => { want.spread = group.value.trim(); choose('spread')(); });
    blocks.placement = h('div', { class: 'step' },
      h('span', { class: 'label' }, 'placement'),
      h('div', { class: 'radios', role: 'radiogroup', 'aria-label': 'placement' }, Object.values(options)),
      h('div', { class: 'row' }, next('placement')));
  }

  // ---- 05 review ----
  const dryRun = h('div');
  const summary = h('div');
  blocks.review = h('div', { class: 'step' }, dryRun, summary);

  // ---- the parts that follow kiln's answer ----
  const steps = h('nav', { class: 'options steps', 'aria-label': 'steps' });
  const bar = h('div', { class: 'actionbar' });
  const make = button('create vm ›', { primary: true, disabled: true });
  make.addEventListener('click', async () => {
    const made = await act(make, () => post(`/v1/projects/${enc(want.project)}/instances`, request(false)), `${want.name} is starting`);
    if (made) {
      try { localStorage.setItem(KEY_STORE, want.ssh_key); } catch { /* fine */ }
      location.hash = instanceHref(made.name);
    }
  });
  const named = () => want.name && NAME.test(want.name);
  const shapeText = () => `${want.vcpus} vcpu · ${want.memory_gb} gb · ${want.disk_gb} gb`;
  const nodeText = () => (want.placement === 'pin' ? nodes.find((n) => n.id === want.node)?.hostname ?? '—' : want.placement === 'spread' ? `spread · ${want.spread || '—'}` : 'copper picks');
  const SUMMARY = {
    name: () => `${want.name || '—'} · vm`,
    image: () => `${want.image} · ${want.arch}`,
    shape: () => (fit && !fit.fits ? st('failed', shapeText()) : shapeText()),
    placement: nodeText,
    review: () => 'dry run',
  };

  function update() {
    fill(steps, keys.map((k, i) => h('button', { type: 'button', class: 'option', 'aria-current': k === current ? 'true' : 'false', onclick: () => goTo(k) },
      h('span', { class: 'n' }, String(i + 1).padStart(2, '0')), h('span', { class: 'k' }, k), h('span', { class: 'v' }, SUMMARY[k]()))));
    const L = fit?.largest;
    const over = { vcpus: L && want.vcpus > L.vcpus, memory_gb: L && want.memory_gb * 1024 > L.memory_mb, disk_gb: L && want.disk_gb > L.disk_gb };
    for (const k of Object.keys(sizes)) sizes[k].setAttribute('aria-invalid', over[k] ? 'true' : 'false');
    fill(hints.vcpus, L ? `${L.vcpus} max` : '');
    fill(hints.memory_gb, L ? `${gb(L.memory_mb).replace(' gb', '')} free` : '');
    fill(hints.disk_gb, L ? `${L.disk_gb} free` : '');
    for (const k of Object.keys(hints)) hints[k].className = `hint${over[k] ? ' bad' : ''}`;
    fill(fitBox, admin && fit?.nodes ? fitFigure() : null);
    fill(refusal, fit && !fit.fits ? wontFit(over) : null);
    fill(dryRun, copyBlock('dry run', dryRunLines()));
    fill(summary, kv([['name', want.name || '—'], ['image', want.image], ['arch', want.arch], ['shape', shapeText()], ['project', want.project], admin ? ['node', nodeText()] : null]));
    make.textContent = `create ${want.name || 'vm'} ›`;
    make.disabled = !(named() && fit?.fits);
    const verdict = !want.name ? st('stopped', 'name it first') : !named() ? st('failed', 'name: a-z 0-9 -') : !fit ? st('starting', 'checking…')
      : fit.fits ? st('running', 'fits') : st('failed', over.vcpus ? `won't fit · ${want.vcpus} > ${L.vcpus} vcpu` : "won't fit");
    fill(bar, verdict, button('cancel', { href: '#/instances' }), make);
  }

  /** FIG.1: each machine's vcpus, held and free, against this shape. */
  function fitFigure() {
    return h('div', { class: 'stack', style: { gap: '12px' } },
      fig(1, 'fit', h('span', { class: 'hint' }, '□ = 1 vcpu')),
      h('div', { class: 'fit' }, fit.nodes.map((f) => {
        const n = nodes.find((x) => x.id === f.node);
        const total = n?.capacity ? Math.floor(n.capacity.vcpus * n.overcommit.cpu) : f.free.vcpus;
        const held = Math.max(0, total - f.free.vcpus);
        const short = Math.max(0, want.vcpus - f.free.vcpus);
        const up = f.status === 'online';
        return h('div', { class: 'row-fit' },
          h('span', { class: 'row', style: { gap: '8px' } }, dot(f.status), f.hostname),
          up ? h('span', { class: 'cubes', 'aria-hidden': 'true' },
            Array.from({ length: Math.min(held, 64) }, () => h('i')),
            Array.from({ length: Math.min(f.free.vcpus, 64) }, () => h('i', { class: 'free' })),
            Array.from({ length: Math.min(short, 32) }, () => h('i', { class: 'want' }))) : h('span', { class: 'hint' }, f.status === 'pending' ? 'waiting' : f.status),
          up ? (f.fits ? st('running', `${f.free.vcpus} free · fits`) : h('span', { class: 'hint' }, `${f.free.vcpus} free · `, h('span', { style: { color: 'var(--ox)' } }, `short ${short}`))) : h('span'));
      })),
      h('p', { class: 'hint', style: { margin: 0 } }, `members see: ${fit.fits ? 'fits' : `won't fit · ${fit.largest?.vcpus ?? 0} vcpu max`}`));
  }

  function wontFit(over) {
    const L = fit.largest;
    return h('div', { class: 'stack', style: { gap: '18px' } },
      h('div', { class: 'node-top' },
        h('h3', { class: 'node-name' }, h('span', { class: 'mono' }, '03'), "won't fit"),
        h('ol', { class: 'facts' }, h('li', {}, h('span', { class: 'mono' }, '3.1'), fit.reason ?? 'refused, not queued'))),
      over.vcpus && L.vcpus > 0 ? h('div', { class: 'row' }, button(`use ${L.vcpus} vcpu ›`, { warm: true, onclick: () => {
        want.vcpus = L.vcpus;
        sizes.vcpus.value = L.vcpus;
        want.shape = 'custom';
        drawShapes();
        changed();
      } })) : null);
  }

  function dryRunLines() {
    const lines = [`$ cu create ${want.name || 'NAME'} --vcpus ${want.vcpus} --memory ${want.memory_gb * 1024} --image ${want.image} --disk ${want.disk_gb}${want.arch === 'x86_64' ? '' : ` --arch ${want.arch}`}${admin && want.placement === 'pin' ? ` --node ${want.node}` : ''}${admin && want.placement === 'spread' && want.spread ? ` --spread ${want.spread}` : ''}`];
    const L = fit?.largest;
    if (!fit) return lines;
    lines.push('checking capacity…');
    if (L) {
      const row = (k, shown, ok, note) => [`${k.padEnd(8)} ${shown.padEnd(7)} `, ok ? ['[ok]', 'ok'] : ['[no]', 'no'], ['  ' + note, 'quiet']];
      lines.push(row('vcpu', String(want.vcpus), want.vcpus <= L.vcpus, `max ${L.vcpus}`),
        row('memory', `${want.memory_gb} GB`, want.memory_gb * 1024 <= L.memory_mb, `${gb(L.memory_mb).replace(' gb', '')} free`),
        row('disk', `${want.disk_gb} GB`, want.disk_gb <= L.disk_gb, `${L.disk_gb} free`));
    }
    lines.push([fit.fits ? ['fits.', 'ok'] : [`won't fit.${L ? '' : ` ${fit.reason ?? ''}`}`, 'no']]);
    return lines;
  }

  // Ask kiln whether this would fit, a moment after the last change. The
  // name doesn't change the answer, so a stand-in is used until there is one.
  let timer;
  function recheck() {
    clearTimeout(timer);
    timer = setTimeout(async () => {
      try { fit = await post(`/v1/projects/${enc(want.project)}/instances/check`, request(true)); } catch (e) { fit = { fits: false, reason: e.message, largest: null }; }
      update();
    }, 250);
  }

  function request(check) {
    return {
      name: named() ? want.name : check ? 'vm' : want.name,
      vcpus: want.vcpus, memory_mb: want.memory_gb * 1024, arch: want.arch, image: want.image, disk_gb: want.disk_gb,
      ssh_keys: want.ssh_key ? [want.ssh_key] : [],
      node: admin && want.placement === 'pin' && want.node ? want.node : undefined,
      spread: admin && want.placement === 'spread' && want.spread ? want.spread : undefined,
    };
  }

  // The step in view is the one marked in the list.
  const watch = new IntersectionObserver((seen) => {
    const hit = seen.find((e) => e.isIntersecting);
    if (!hit) return;
    current = keys.find((k) => blocks[k] === hit.target);
    update();
  }, { rootMargin: '-35% 0px -60% 0px' });
  keys.forEach((k) => watch.observe(blocks[k]));
  ctx.onLeave(() => { watch.disconnect(); clearTimeout(timer); });

  update();
  recheck();
  return [
    h('section', {}, h('div', { class: 'head' }, crumb({ text: 'instances', href: '#/instances' }, { text: 'new' }), title('New VM.', 'Pick a shape.'))),
    h('section', {}, h('div', { class: 'cells split' }, h('div', {}, steps), h('div', { class: 'wizard' }, keys.map((k) => blocks[k]), bar))),
  ];
}

/** How many of the visible instances use each image, for the image list. */
async function imageUse() {
  const projects = isAdmin() ? session.projects : session.projects.filter((p) => p.owner === session.me.name);
  const lists = await Promise.all(projects.map((p) => get(`/v1/projects/${enc(projectPath(p))}/instances`).catch(() => [])));
  const count = {};
  for (const i of lists.flat()) count[i.image] = (count[i.image] ?? 0) + 1;
  return count;
}

function field(name, control, hint) {
  return h('label', { class: 'field' }, h('span', {}, name), control, hint ?? null);
}

function radio(name, sub, aside, checked, onclick) {
  return h('div', { class: 'radio', role: 'radio', tabindex: 0, 'aria-checked': checked ? 'true' : 'false',
    onclick: (e) => !e.target.closest('select, input') && onclick(),
    onkeydown: (e) => e.target === e.currentTarget && (e.key === ' ' || e.key === 'Enter') && (e.preventDefault(), onclick()) },
    h('span', { class: 'knob', 'aria-hidden': 'true' }),
    h('span', { class: 'what' }, name, sub ? h('small', {}, sub) : null),
    aside);
}

function card(name, small, pressed, onclick) {
  return h('button', { type: 'button', class: 'card-pick', 'aria-pressed': pressed ? 'true' : 'false', onclick }, name, h('small', {}, small));
}
