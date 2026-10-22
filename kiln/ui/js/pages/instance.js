import { del, enc, get, post } from '../api.js';
import { fill, h } from '../dom.js';
import { ago, bytes, percent, shape, since } from '../format.js';
import { isAdmin, session } from '../session.js';
import { act, button, copyBlock, empty, field, input, kv, label, notice, spark, stateBadge, subtitle, title, toast } from '../ui.js';
import { instanceUrl, stopThenStart } from './common.js';

const TABS = ['overview', 'history', 'console', 'metrics'];

export default async function instance(ctx, name) {
  let inst = await get(instanceUrl(name));
  let tab = 'overview';
  let leaveTab = () => {};
  const header = h('div');
  const tabs = h('div', { class: 'tabs', role: 'tablist' });
  const body = h('div');

  const drawHeader = () => fill(header, heading(inst, refresh));
  const drawTabs = () => fill(tabs, TABS.map((t) => h('button', {
    type: 'button', role: 'tab', 'aria-selected': t === tab ? 'true' : 'false', onclick: () => open(t),
  }, t)));
  const open = (t) => {
    leaveTab();
    leaveTab = () => {};
    tab = t;
    drawTabs();
    const page = { overview, history, console: serialConsole, metrics }[t](inst, refresh);
    fill(body, page.node);
    leaveTab = page.leave ?? (() => {});
  };
  async function refresh() {
    try {
      inst = await get(instanceUrl(name));
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

function heading(inst, refresh) {
  const where = inst.node_name ? `on ${inst.node_name}` : null;
  const line = {
    running: `Running${where ? ` ${where}` : ''}.`,
    starting: 'Starting.',
    stopping: 'Stopping.',
    stopped: 'Stopped.',
    failed: 'Failed.',
    deleting: 'Deleting.',
    unknown: 'Unknown. Its machine went quiet.',
  }[inst.state];
  const toggle = inst.state === 'stopped' || inst.state === 'failed'
    ? button(inst.state === 'failed' ? 'stop + start' : 'start', { onclick: (e) => change(e.currentTarget, inst, inst.state === 'failed' ? 'reset' : 'start', refresh) })
    : button('stop', { onclick: (e) => change(e.currentTarget, inst, 'stop', refresh), disabled: inst.state !== 'running' && inst.state !== 'starting' });
  return h('div', { class: 'head' },
    h('div', { class: 'label' }, h('a', { href: '#/instances', style: { color: 'var(--fg3)', textDecoration: 'none' } }, 'instances'), ` / ${inst.name}`),
    title(`${inst.name}.`, line),
    h('div', { class: 'row' }, stateBadge(inst.state),
      h('span', { class: 'hint' }, [inst.id.slice(0, 8), inst.image, inst.arch, inst.addresses[0]].filter(Boolean).join(' · ')),
      h('span', { class: 'hint' }, `made ${ago(inst.created_at)} ago by ${inst.owner}`)),
    inst.error && inst.state !== 'running' ? notice(inst.error, inst.state === 'failed' ? 'bad' : 'warm') : null,
    h('div', { class: 'row' },
      button('connect ›', { primary: true, onclick: () => document.getElementById('connect')?.scrollIntoView({ behavior: 'smooth' }) }),
      toggle,
      button('delete', { danger: true, onclick: () => document.getElementById('delete')?.scrollIntoView({ behavior: 'smooth' }) })));
}

async function change(btn, inst, what, refresh) {
  await act(btn, async () => {
    if (what === 'reset') await stopThenStart(inst.name, (t) => (btn.textContent = t));
    else await post(instanceUrl(inst.name, `/${what}`));
  });
  refresh();
}

// ---- overview tab ----------------------------------------------------------------

function overview(inst, refresh) {
  return {
    node: h('div', {},
      connect(inst),
      resize(inst, refresh),
      h('div', { class: 'pad stack' }, subtitle('Properties.'), properties(inst)),
      remove(inst)),
  };
}

function connect(inst) {
  const project = session.project;
  const flag = project === 'default' ? '' : ` --project ${project}`;
  const address = inst.addresses[0];
  // Commands appear only once there is something to connect to.
  if (inst.state !== 'running') return null;
  return h('div', { class: 'pad stack', id: 'connect' },
    subtitle('Connect.', `cu ssh ${inst.name}.`),
    h('div', { class: 'cells two' },
      h('div', { style: { paddingRight: '12px' } }, copyBlock('through kiln', [`$ cu${flag} ssh ${inst.name}`])),
      h('div', { style: { paddingLeft: '12px' } }, copyBlock('scp, rsync, editors', [`$ cu${flag} ssh-config ${inst.name} >> ~/.ssh/config`]))),
    address ? copyBlock(`on the network of ${address}`, [`$ ssh ubuntu@${address}`]) : null);
}

function resize(inst, refresh) {
  const stopped = inst.state === 'stopped';
  const vcpus = input({ type: 'number', min: 1, value: inst.vcpus, disabled: !stopped });
  const memory = input({ type: 'number', min: 128, step: 128, value: inst.memory_mb, disabled: !stopped });
  const disk = input({ type: 'number', min: inst.disk_gb, value: inst.disk_gb, disabled: !stopped });
  const go = button('resize ›', { primary: true, type: 'submit', disabled: !stopped });
  // Kiln only resizes stopped instances, so the form is locked otherwise.
  const form = h('form', { class: 'pad stack' },
    subtitle('Resize.', stopped ? 'Applies on next start.' : `Locked while ${inst.state}.`),
    h('div', { class: 'cells four', style: { background: 'none', gap: '16px' } },
      field('vcpu', vcpus),
      field('memory, mb', memory),
      field('boot disk, gb', disk, `${inst.disk_gb} or more`)),
    h('div', { class: 'row' }, go));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    await act(go, () => post(instanceUrl(inst.name, '/resize'), {
      vcpus: Number(vcpus.value), memory_mb: Number(memory.value), disk_gb: Number(disk.value),
    }), `${inst.name} resized; it applies when you start it`);
    refresh();
  });
  return form;
}

function properties(inst) {
  return kv([
    isAdmin() ? ['node', inst.node_name ?? '—'] : null,
    ['project', inst.project],
    ['image', inst.image],
    ['arch', inst.arch],
    ['shape', shape(inst)],
    ['addresses', inst.addresses.join(', ') || '—'],
    ['up', inst.running_since ? since(inst.running_since) : '—'],
    isAdmin() && inst.spread ? ['spread group', inst.spread] : null,
    ['id', inst.id],
    ['made', `${ago(inst.created_at)} ago by ${inst.owner}`],
  ]);
}

function remove(inst) {
  const confirm = input({ placeholder: inst.name, autocomplete: 'off' });
  const go = button(`delete ${inst.name}`, { danger: true, type: 'submit', disabled: true });
  confirm.addEventListener('input', () => (go.disabled = confirm.value !== inst.name));
  const form = h('form', { class: 'pad stack', id: 'delete', style: { maxWidth: '560px' } },
    subtitle('Delete.', `Its ${inst.disk_gb} gb disk goes with it.`),
    field(`type ${inst.name} to confirm`, confirm),
    h('div', { class: 'row' }, go));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const ok = await act(go, async () => {
      await del(instanceUrl(inst.name));
      return true;
    }, `deleting ${inst.name}`);
    if (ok) location.hash = '#/instances';
  });
  return form;
}

// ---- history tab -----------------------------------------------------------------

function history(inst) {
  const list = h('div', {}, h('p', { class: 'hint' }, 'loading…'));
  get(`/v1/events?project=${enc(session.project)}&instance=${enc(inst.name)}&limit=100`).then((events) => {
    fill(list, events.length ? events.map(eventRow) : empty('nothing has happened yet.'));
  }).catch((e) => fill(list, notice(e.message, 'bad')));
  return { node: h('div', { class: 'pad stack' }, subtitle('History.', 'Newest first.'), list) };
}

export function eventRow(e) {
  return h('div', { class: 'event' },
    h('span', { class: 'when' }, `${ago(e.at)} ago`),
    h('span', { class: `kind ${e.kind}` }, e.kind),
    h('span', { class: 'what' }, e.detail, e.actor ? h('i', {}, ` · by ${e.actor}`) : null));
}

// ---- console tab -----------------------------------------------------------------

function serialConsole(inst) {
  const screen = h('div', { id: 'console-term' });
  const status = h('span', { class: 'hint' });
  if (inst.state !== 'running') return { node: h('div', { class: 'pad' }, notice(`${inst.name} is ${inst.state}; the console opens on running instances.`)) };
  const box = h('div', { class: 'pad stack' }, h('div', { class: 'row spread' }, subtitle('Console.', 'Serial, ttyS0.'), status), screen);
  if (!window.Terminal) {
    fill(screen, notice('the terminal didn’t load; reload the page.', 'bad'));
    return { node: box };
  }
  const term = new window.Terminal({
    convertEol: false, cursorBlink: true, fontFamily: 'IBM Plex Mono, monospace', fontSize: 13, rows: 26,
    theme: { background: '#070908', foreground: '#EDE9E2', cursor: '#E8915C', selectionBackground: 'rgba(232,145,92,.35)' },
  });
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${scheme}://${location.host}${instanceUrl(inst.name, '/console')}`);
  ws.binaryType = 'arraybuffer';
  status.textContent = 'connecting…';
  ws.onopen = () => (status.textContent = 'connected');
  ws.onmessage = (m) => term.write(typeof m.data === 'string' ? m.data : new Uint8Array(m.data));
  ws.onclose = () => {
    status.textContent = 'disconnected';
    term.write('\r\n[copper] console closed\r\n');
  };
  const encoder = new TextEncoder();
  term.onData((d) => ws.readyState === WebSocket.OPEN && ws.send(encoder.encode(d)));
  // xterm needs to be in the document before it can measure itself.
  requestAnimationFrame(() => {
    term.open(screen);
    term.focus();
  });
  return {
    node: box,
    leave: () => {
      ws.close();
      term.dispose();
    },
  };
}

// ---- metrics tab -----------------------------------------------------------------

function metrics(inst) {
  const charts = h('div', {}, h('p', { class: 'hint' }, 'loading…'));
  const draw = async () => {
    const samples = await get(instanceUrl(inst.name, '/metrics'));
    if (!samples.length) {
      fill(charts, empty('no readings from the node yet.'));
      return;
    }
    const last = samples[samples.length - 1].usage;
    const chart = (name, now, values, max) => h('div', { class: 'pad stack' }, label(name, h('span', { class: 'mono', style: { color: 'var(--fg)' } }, now)), spark(values, max), label('1h ago', 'now'));
    fill(charts, h('div', { class: 'cells two' },
      chart('cpu', percent(last.cpu_permille), samples.map((s) => s.usage.cpu_permille), 1000),
      chart('memory', `${bytes(last.memory_used_bytes)} / ${bytes(last.memory_total_bytes)}`, samples.map((s) => s.usage.memory_used_bytes), last.memory_total_bytes),
      chart('disk', `${bytes(last.disk_used_bytes)} / ${bytes(last.disk_total_bytes)}`, samples.map((s) => s.usage.disk_used_bytes), last.disk_total_bytes)));
  };
  draw().catch((e) => fill(charts, notice(e.message, 'bad')));
  const timer = setInterval(() => draw().catch(() => {}), 10000);
  return { node: h('div', {}, h('div', { class: 'pad' }, subtitle('Metrics.', 'Last hour.')), charts), leave: () => clearInterval(timer) };
}
