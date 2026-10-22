// Saved SSH keys: the list on the people page, and the two ways to get one
// (make a new pair here, or paste one you have), which New VM reuses.

import { del, enc, get, post } from '../api.js';
import { fill, h } from '../dom.js';
import { ago } from '../format.js';
import { session } from '../session.js';
import { download, makeKeyPair } from '../sshkey.js';
import { act, button, copyBlock, empty, field, input, label, subtitle, table } from '../ui.js';

/** `ed25519`, `rsa`, `ecdsa-sha2-nistp256`: the type without its `ssh-`. */
export const keyType = (k) => k.public_key.split(' ')[0].replace(/^ssh-/, '').replace(/@openssh\.com$/, '');

const shortPrint = (fp) => `${fp.slice(0, 15)}…${fp.slice(-5)}`;

/** The people page's section: your keys, then both ways to add one.
 *  `onCount(n)` hears how many there are after each change. */
export function keysSection(initial, onCount) {
  let keys = initial;
  const list = h('div');
  const show = () => fill(list, keys.length ? keyTable(keys, refresh) : empty('no keys saved yet.'));
  const refresh = async () => {
    keys = await get('/v1/keys');
    show();
    onCount(keys.length);
  };
  show();
  return h('div', { style: { paddingBottom: '28px' } },
    h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('SSH keys.', 'For new VMs.')),
    list,
    h('div', { class: 'pad' }, addKeys(() => keys, refresh)));
}

/** Make one or paste one, side by side. `onSaved(key)` runs after either. */
export function addKeys(current, onSaved) {
  return h('div', { class: 'cells two flat' },
    h('div', { class: 'panel' }, label('make a new key'), makeKey(current, onSaved)),
    h('div', { class: 'panel' }, label('add one you have'), pasteKey(onSaved)));
}

function keyTable(keys, refresh) {
  return table([
    { title: 'name', cell: (k) => k.name, class: 'name' },
    { title: 'type', cell: keyType },
    { title: 'fingerprint', cell: (k) => h('span', { class: 'quiet', title: k.fingerprint }, shortPrint(k.fingerprint)), class: 'hide-sm' },
    { title: 'added', cell: (k) => `${ago(k.created_at)} ago` },
    { title: '', cell: (k) => forget(k, refresh), class: 'act' },
  ], keys);
}

function forget(k, refresh) {
  const btn = button('remove', { small: true, danger: true });
  btn.addEventListener('click', async () => {
    if (!window.confirm(`remove ${k.name}? new vms won't get it; vms that have it keep it.`)) return;
    const ok = await act(btn, () => del(`/v1/keys/${enc(k.id)}`), `${k.name} removed`);
    if (ok !== undefined) refresh();
  });
  return btn;
}

/** A name nobody has used yet: mac, mac-2, … */
function freshName(keys) {
  const p = navigator.userAgentData?.platform ?? navigator.platform ?? '';
  const base = /mac/i.test(p) ? 'mac' : /win/i.test(p) ? 'windows' : /linux/i.test(p) ? 'linux' : 'laptop';
  const taken = new Set(keys.map((k) => k.name));
  for (let i = 1; ; i++) {
    const name = i === 1 ? base : `${base}-${i}`;
    if (!taken.has(name)) return name;
  }
}

/** Make a pair here. The private half goes to your downloads and nowhere
 *  else; only the public half is sent to kiln. */
function makeKey(current, onSaved) {
  const name = input({ value: freshName(current()), maxlength: 64, pattern: '[A-Za-z0-9._\\-]+', required: true, spellcheck: 'false' });
  const go = button('generate ›', { primary: true, type: 'submit' });
  const out = h('div');
  const form = h('form', { class: 'stack' }, h('div', { class: 'key-row' }, field('name', name), go), out);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const file = name.value.trim();
    const saved = await act(go, async () => {
      const pair = await makeKeyPair(`${session.me.name}@${file} (copper)`);
      const key = await post('/v1/keys', { public_key: pair.publicKey, name: file });
      // Only once kiln took it, so a refused key never lands in downloads.
      download(file, pair.privateKey);
      return key;
    }, `${file} saved; its private key is in your downloads`);
    if (!saved) return;
    fill(out, copyBlock('then, where cu ssh looks for it', [
      '$ mkdir -p ~/.copper/keys',
      `$ mv -i ~/Downloads/${file} ~/.copper/keys/`,
      `$ chmod 600 ~/.copper/keys/${file}`,
    ]));
    name.value = freshName([...current(), saved]);
    onSaved(saved);
  });
  return form;
}

/** Save the public half of a key you already have. */
function pasteKey(onSaved) {
  const key = input({ placeholder: 'ssh-ed25519 AAAA… you@laptop', spellcheck: 'false', required: true, 'aria-label': 'public key' });
  const save = button('save ›', { type: 'submit' });
  const form = h('form', { class: 'stack' }, h('div', { class: 'key-row' }, field('public key', key), save));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const saved = await act(save, () => post('/v1/keys', { public_key: key.value.trim() }), 'key saved');
    if (!saved) return;
    key.value = '';
    onSaved(saved);
  });
  return form;
}
