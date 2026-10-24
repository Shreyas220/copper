import { del, enc, get, post } from '../api.js';
import { role, slab } from '../art.js';
import { fill, h } from '../dom.js';
import { ago, bytes, percent, shape, since } from '../format.js';
import { icon } from '../icons.js';
import { isAdmin, session } from '../session.js';
import { act, button, copyBlock, crumb, dot, empty, field, input, notice, pill, props, spark, st, subtitle, title, toast } from '../ui.js';
import { instanceUrl, projectUrl, stopThenStart } from './common.js';

const TABS = ['overview', 'history', 'console', 'metrics'];

export default async function instance(ctx, name) {
  let inst = await get(instanceUrl(name));
  // Admins get the node record too, to draw the machine the VM is on.
  let nodes = isAdmin() ? await get('/v1/nodes') : [];
  // Your saved keys name the VM's keys, and are what a recreate puts in.
  const mine = await get('/v1/keys');
  // Recreating deletes the VM for a moment; the refresh must not take that
  // for the VM being gone.
  let paused = false;
  const page = { mine, pause: (p) => (paused = p) };
  let tab = 'overview';
  let leaveTab = () => {};
  const header = h('div', { class: 'split-head' });
  const tabs = h('div', { class: 'tabs', role: 'tablist' });
  const body = h('div', { class: 'tabbody' });

  const drawHeader = () => fill(header, heading(inst, refresh), machine(inst, nodes.find((n) => n.id === inst.node)));
  const drawTabs = () => fill(tabs, TABS.map((t) => h('button', {
    type: 'button', role: 'tab', 'aria-selected': t === tab ? 'true' : 'false', onclick: () => open(t),
  }, t)));
  const open = (t) => {
    leaveTab();
    leaveTab = () => {};
    tab = t;
    drawTabs();
    const shown = { overview, history, console: serialConsole, metrics }[t](inst, refresh, page);
    fill(body, shown.node);
    leaveTab = shown.leave ?? (() => {});
  };
  async function refresh() {
    if (paused) return;
    try {
      inst = await get(instanceUrl(name));
      if (isAdmin()) nodes = await get('/v1/nodes');
    } catch (e) {
      if (e.status === 404) {
        toast(`${name} is gone`);
        location.hash = '#/instances';
      }
      return;
    }
    drawHeader();
    // Don't redraw a form under someone's fingers.
    if (tab === 'overview' && !body.contains(document.activeElement)) open('overview');
  }

  drawHeader();
  open('overview');
  ctx.every(5000, refresh);
  ctx.onLeave(() => leaveTab());
  return [h('section', {}, header), h('section', {}, tabs, body)];
}

// ---- header ------------------------------------------------------------------

const LINES = {
  running: (where) => `Running${where ? ` on ${where}` : ''}.`,
  starting: () => 'Starting.',
  stopping: () => 'Stopping.',
  stopped: () => 'Stopped.',
  failed: () => 'Failed.',
  deleting: () => 'Deleting.',
  unknown: () => 'Its node is not reporting.',
};

function heading(inst, refresh) {
  const toggle = inst.state === 'stopped' || inst.state === 'failed'
    ? button(inst.state === 'failed' ? 'stop + start' : 'start', { warm: inst.state === 'failed', onclick: (e) => change(e.currentTarget, inst, inst.state === 'failed' ? 'reset' : 'start', refresh) })
    : button('stop', { onclick: (e) => change(e.currentTarget, inst, 'stop', refresh), disabled: inst.state !== 'running' && inst.state !== 'starting' });
  return h('div', { class: 'head', style: { gap: '22px' } },
    crumb({ text: 'instances', href: '#/instances' }, { text: inst.name }),
    title(`${inst.name}.`, LINES[inst.state](inst.node_name)),
    h('div', { class: 'row', style: { gap: '10px 14px' } }, pill(inst.state),
      h('span', { class: 'hint', style: { fontSize: '12px' } }, [inst.id.slice(0, 8), inst.image, inst.arch, inst.addresses[0]].filter(Boolean).join(' · '))),
    h('span', { class: 'hint', style: { fontSize: '12px' } }, `made ${ago(inst.created_at)} ago by ${inst.owner}`),
    inst.error && inst.state !== 'running' ? notice(inst.error, inst.state === 'failed' ? 'bad' : 'warm') : null,
    h('div', { class: 'row', style: { gap: '8px', marginTop: '6px' } },
      // Connect only exists while it runs; see connect() below.
      inst.state === 'running' ? button(inst.ssh_keys.length ? 'connect ›' : 'fix ssh ›', { primary: true, onclick: () => document.getElementById('connect')?.scrollIntoView({ behavior: 'smooth' }) }) : null,
      toggle,
      button('delete', { danger: true, onclick: () => document.getElementById('delete')?.scrollIntoView({ behavior: 'smooth' }) })));
}

/** FIG.0: the node this VM runs on, with the VM's callout. Admin only. */
function machine(inst, node) {
  if (!node || !role(node)) return null;
  return h('figure', { class: 'scene machine' },
    slab(node),
    h('span', { class: 'drop', 'aria-hidden': 'true' }),
    h('span', { class: 'tagbox drop-box' }, dot(inst.state), inst.name, h('small', {}, shape(inst))),
    h('figcaption', { class: 'label' }, 'FIG.0', h('a', { href: '#/nodes' }, node.hostname)));
}

async function change(btn, inst, what, refresh) {
  await act(btn, async () => {
    if (what === 'reset') await stopThenStart(inst.name, (t) => (btn.textContent = t));
    else await post(instanceUrl(inst.name, `/${what}`));
  });
  refresh();
}

// ---- overview tab ----------------------------------------------------------------

function overview(inst, refresh, page) {
  return {
    node: h('div', {},
      inst.ssh_keys.length ? connect(inst) : noKey(inst, refresh, page),
      h('section', {}, resize(inst, refresh)),
      h('section', {}, h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('Properties.')), properties(inst, page.mine)),
      h('section', {}, remove(inst))),
  };
}

/** One of the VM's keys, by the name you saved it under when it's yours. */
const keyName = (k, mine) => mine.find((m) => m.fingerprint === k.fingerprint)?.name ?? k.comment ?? `${k.fingerprint.slice(0, 15)}…`;

/** A VM made without an SSH key: nobody can log in, and keys are only
 *  written at first boot, so the fix is to make it again with your keys. */
function noKey(inst, refresh, page) {
  const { mine } = page;
  const fix = h('div');
  const start = button(`recreate ${inst.name} ›`, { warm: true, disabled: !mine.length });
  start.addEventListener('click', () => {
    const confirm = input({ placeholder: inst.name, autocomplete: 'off', 'aria-label': `type ${inst.name} to recreate it` });
    const go = button('recreate for good', { danger: true, type: 'submit', disabled: true });
    confirm.addEventListener('input', () => (go.disabled = confirm.value !== inst.name));
    const form = h('form', { class: 'row', style: { alignItems: 'flex-end', maxWidth: '640px' } },
      h('div', { style: { flex: '1' } }, field(`its ${inst.disk_gb} gb disk is erased · type ${inst.name}`, confirm)), go);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      page.pause(true);
      const made = await act(go, () => recreate(inst, (t) => (go.textContent = t)), `${inst.name} is starting again, with your keys`);
      page.pause(false);
      if (!made) {
        go.textContent = 'recreate for good';
        return;
      }
      // The overview only redraws when nothing in it has focus.
      document.activeElement?.blur();
      refresh();
    });
    fill(fix, form);
    confirm.focus();
  });
  const flag = session.project === 'default' ? '' : ` --project ${session.project}`;
  return h('section', { id: 'connect' },
    h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('No SSH key.', `Nobody can log in to ${inst.name}.`)),
    h('div', { class: 'pad', style: { paddingTop: 0 } }, h('div', { class: 'looks' },
      step('01', 'save an ssh key', mine.length ? st('running', `yours: ${mine.map((k) => k.name).join(', ')}`) : st('stopped', 'you have none saved'),
        mine.length ? null : button('add one ›', { small: true, href: '#/people' })),
      step('02', `recreate ${inst.name} with it`, h('span', { class: 'hint' }, `same shape and image, new disk · cu rm ${inst.name} && cu create ${inst.name}`), start),
      step('03', 'connect', h('span', { class: 'hint' }, `cu${flag} ssh ${inst.name}`), null)),
      fix));
}

function step(n, what, detail, action) {
  return h('div', { class: 'look' }, h('span', { class: 'when' }, n), h('div', {}, h('div', { class: 'name' }, what), detail), action ?? h('span'));
}

/** Delete, wait for the node to let it go, then make it again with the
 *  caller's saved keys: a create that names none gets them. */
async function recreate(inst, progress) {
  progress('deleting…');
  await del(instanceUrl(inst.name));
  for (let i = 0; ; i++) {
    try {
      await get(instanceUrl(inst.name));
    } catch (e) {
      if (e.status === 404) break;
      throw e;
    }
    if (i === 90) throw new Error(`${inst.name} is taking a while to go; make it again from new vm`);
    await new Promise((r) => setTimeout(r, 2000));
  }
  progress('creating…');
  return post(projectUrl('/instances'), {
    name: inst.name, vcpus: inst.vcpus, memory_mb: inst.memory_mb, arch: inst.arch, image: inst.image, disk_gb: inst.disk_gb,
    spread: isAdmin() && inst.spread ? inst.spread : undefined,
  });
}

/** Three ways in; the left list picks which commands show on the right. */
function connect(inst) {
  if (inst.state !== 'running') return null;
  const flag = session.project === 'default' ? '' : ` --project ${session.project}`;
  // Everything goes through kiln and the VM's node, wherever you are; the
  // tailnet has no route to VM addresses.
  const ways = [
    { k: 'cu ssh', v: 'through kiln, from anywhere', lines: [`$ cu${flag} ssh ${inst.name}`] },
    { k: 'ssh-config', v: 'scp, rsync, editors', lines: [`$ cu${flag} ssh-config ${inst.name} >> ~/.ssh/config`, `$ scp FILE ${inst.name}.copper:`] },
    { k: 'cu forward', v: 'one of its ports, on localhost', lines: [`$ cu${flag} forward ${inst.name} 8080`] },
  ];
  let chosen = 0;
  const list = h('div', { class: 'options' });
  const shown = h('div', { class: 'pad' });
  const draw = () => {
    fill(list, ways.map((w, i) => h('button', { type: 'button', class: 'option', 'aria-current': i === chosen ? 'true' : 'false', onclick: () => { chosen = i; draw(); } },
      h('span', { class: 'n' }, String(i + 1).padStart(2, '0')), h('span', { class: 'k' }, w.k), h('span', { class: 'v' }, w.v))));
    fill(shown, copyBlock(ways[chosen].v, ways[chosen].lines));
  };
  draw();
  return h('section', { id: 'connect' },
    h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('Connect.', `cu ssh ${inst.name}.`)),
    h('div', { class: 'cells split', style: { borderTop: '1px solid var(--l2)' } }, list, shown));
}

/** Kiln only resizes stopped instances; disks only grow. */
function resize(inst, refresh) {
  const stopped = inst.state === 'stopped';
  const vcpus = input({ type: 'number', min: 1, value: inst.vcpus, disabled: !stopped });
  const memory = input({ type: 'number', min: 128, step: 128, value: inst.memory_mb, disabled: !stopped });
  const disk = input({ type: 'number', min: inst.disk_gb, value: inst.disk_gb, disabled: !stopped });
  const go = button('resize ›', { primary: true, type: 'submit', disabled: !stopped });
  const form = h('form', { class: 'panel' },
    stopped ? null : h('div', { class: 'lockline' }, icon('lock'), `locked · ${inst.name} is ${inst.state}`),
    h('div', { class: 'cells three flat' },
      field('vcpu', vcpus, `now ${inst.vcpus}`),
      field('memory, mb', memory, `now ${inst.memory_mb}`),
      field('boot disk, gb', disk, `only grows · ${inst.disk_gb} or more`)),
    h('div', { class: 'row', style: { gap: '8px' } },
      inst.state === 'running' || inst.state === 'failed' ? button('stop to resize', { onclick: (e) => change(e.currentTarget, inst, 'stop', refresh) }) : null,
      go));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    await act(go, () => post(instanceUrl(inst.name, '/resize'), {
      vcpus: Number(vcpus.value), memory_mb: Number(memory.value), disk_gb: Number(disk.value),
    }), `${inst.name} resized; it applies when you start it`);
    refresh();
  });
  return h('div', {},
    h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('Resize.', stopped ? 'Applies on next start.' : `Stop ${inst.name} first.`)),
    h('div', { class: 'pad', style: { paddingTop: 0 } }, form));
}

function properties(inst, mine) {
  return props([
    isAdmin() ? ['node', h('a', { href: '#/nodes' }, `${inst.node_name ?? '—'} ›`)] : null,
    ['project', inst.project],
    ['image', inst.image],
    ['arch', inst.arch],
    ['vcpu', String(inst.vcpus)],
    ['memory', `${inst.memory_mb} MB`],
    ['boot disk', `${inst.disk_gb} GB`],
    ['address', inst.addresses.join(', ') || '—'],
    ['up', inst.running_since ? since(inst.running_since) : '—'],
    ['ssh keys', inst.ssh_keys.length ? inst.ssh_keys.map((k) => keyName(k, mine)).join(', ') : st('failed', 'none')],
    isAdmin() && inst.spread ? ['spread group', inst.spread] : null,
    ['id', inst.id.slice(0, 8)],
    ['made', `${ago(inst.created_at)} ago by ${inst.owner}`],
  ]);
}

/** Delete is final, so the button asks for the name before it acts. */
function remove(inst) {
  const holder = h('div', { class: 'pad-x', style: { paddingBottom: '28px' } });
  const start = button(`delete ${inst.name}`, { danger: true });
  start.addEventListener('click', () => {
    const confirm = input({ placeholder: inst.name, autocomplete: 'off' });
    const go = button('delete for good', { danger: true, type: 'submit', disabled: true });
    confirm.addEventListener('input', () => (go.disabled = confirm.value !== inst.name));
    const form = h('form', { class: 'row', style: { alignItems: 'flex-end', maxWidth: '640px' } },
      h('div', { style: { flex: '1' } }, field(`its ${inst.disk_gb} gb disk goes with it · type ${inst.name}`, confirm)), go);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      const ok = await act(go, async () => { await del(instanceUrl(inst.name)); return true; }, `deleting ${inst.name}`);
      if (ok) location.hash = '#/instances';
    });
    fill(holder, form);
    confirm.focus();
  });
  return h('div', { id: 'delete' },
    h('div', { class: 'head row spread', style: { flexDirection: 'row', paddingTop: '56px' } }, subtitle('Delete.'), start),
    holder);
}

// ---- history tab -----------------------------------------------------------------

/** How each kind of event is coloured in the log. */
const TONE = {
  running: 'running', 'node back': 'running', 'node approved': 'running',
  failed: 'failed', 'node retired': 'failed',
  stopped: 'stopped', removed: 'stopped', 'node quiet': 'stopped', forgotten: 'stopped',
};

export function eventRows(events) {
  return h('div', { class: 'table-wrap' }, h('table', { class: 'log' },
    h('thead', {}, h('tr', {}, h('th', {}, 'when'), h('th', {}, 'event'), h('th', {}, 'detail'))),
    h('tbody', {}, events.map((e) => h('tr', {},
      h('td', {}, `${ago(e.at)} ago`),
      h('td', {}, st(TONE[e.kind] ?? 'warm', e.kind)),
      h('td', {}, e.detail, e.actor ? ` · by ${e.actor}` : ''))))));
}

function history(inst) {
  const list = h('div', {}, h('p', { class: 'hint pad-x' }, 'loading…'));
  get(`/v1/events?project=${enc(session.project)}&instance=${enc(inst.name)}&limit=100`).then((events) => {
    fill(list, events.length ? eventRows(events) : empty('nothing has happened yet.'));
  }).catch((e) => fill(list, notice(e.message, 'bad')));
  return { node: h('div', { style: { paddingBottom: '28px' } }, h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('History.')), list) };
}

// ---- console tab -----------------------------------------------------------------

function serialConsole(inst) {
  const head = h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('Console.', 'For when ssh won’t.'));
  if (inst.state !== 'running') return { node: h('div', {}, head, h('div', { class: 'pad' }, notice(`${inst.name} is ${inst.state}; the console opens on running instances.`))) };
  const screen = h('div', { id: 'console-term' });
  const status = h('span', { class: 'label row', style: { gap: '8px' } }, dot('starting'), `serial · ttyS0 · ${inst.name}`);
  const box = h('div', {}, head, h('div', { class: 'pad', style: { paddingTop: 0 } }, h('div', { class: 'term' },
    h('div', { class: 'bar' }, status, h('button', { type: 'button', onclick: () => ws.close() }, 'disconnect')), screen)));
  if (!window.Terminal) {
    fill(screen, notice('the terminal didn’t load; reload the page.', 'bad'));
    return { node: box };
  }
  const term = new window.Terminal({
    cursorBlink: true, fontFamily: 'IBM Plex Mono, monospace', fontSize: 13, rows: 24,
    theme: { background: '#070908', foreground: '#CBC7BF', cursor: '#E8915C', selectionBackground: 'rgba(232,145,92,.35)' },
  });
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${scheme}://${location.host}${instanceUrl(inst.name, '/console')}`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => status.firstChild.replaceWith(dot('running'));
  ws.onmessage = (m) => term.write(typeof m.data === 'string' ? m.data : new Uint8Array(m.data));
  ws.onclose = () => {
    status.firstChild.replaceWith(dot('stopped'));
    term.write('\r\n[copper] console closed\r\n');
  };
  const encoder = new TextEncoder();
  term.onData((d) => ws.readyState === WebSocket.OPEN && ws.send(encoder.encode(d)));
  // xterm needs to be in the document before it can measure itself.
  requestAnimationFrame(() => {
    term.open(screen);
    term.focus();
  });
  return { node: box, leave: () => { ws.close(); term.dispose(); } };
}

// ---- metrics tab -----------------------------------------------------------------

function metrics(inst) {
  const charts = h('div', {}, h('p', { class: 'hint pad-x' }, 'loading…'));
  const draw = async () => {
    const samples = await get(instanceUrl(inst.name, '/metrics'));
    if (!samples.length) {
      fill(charts, empty('no readings from the node yet.'));
      return;
    }
    const last = samples[samples.length - 1].usage;
    const cell = (name, big, small, values, max, kind) => h('div', { class: 'metric' },
      h('span', { class: 'label' }, name),
      h('span', { class: 'big' }, big, small ? h('small', {}, ` / ${small}`) : null),
      spark(values, max, { kind }), h('div', { class: 'axis' }, h('span', {}, '1h ago'), h('span', {}, 'now')));
    fill(charts, h('div', { class: 'cells three', style: { borderTop: '1px solid var(--l2)' } },
      cell('cpu', percent(last.cpu_permille), null, samples.map((s) => s.usage.cpu_permille), 1000),
      cell('memory', bytes(last.memory_used_bytes), bytes(last.memory_total_bytes), samples.map((s) => s.usage.memory_used_bytes), last.memory_total_bytes, 'pat'),
      cell('disk', bytes(last.disk_used_bytes), bytes(last.disk_total_bytes), samples.map((s) => s.usage.disk_used_bytes), last.disk_total_bytes)));
  };
  draw().catch((e) => fill(charts, notice(e.message, 'bad')));
  const timer = setInterval(() => draw().catch(() => {}), 10000);
  return { node: h('div', {}, h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('Metrics.', 'Last hour.')), charts), leave: () => clearInterval(timer) };
}
