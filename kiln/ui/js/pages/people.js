import { del, enc, get, post } from '../api.js';
import { fill, h } from '../dom.js';
import { ago, plural } from '../format.js';
import { isAdmin, session } from '../session.js';
import { act, button, copyBlock, empty, field, input, subtitle, table, title } from '../ui.js';
import { keysSection } from './keys.js';

/** People, keys and tokens. Admins invite people and see everyone's tokens;
 *  everyone manages their own SSH keys and their own tokens for cu. */
export default async function people() {
  const [users, keys, tokens] = await Promise.all([isAdmin() ? get('/v1/users') : Promise.resolve(null), get('/v1/keys'), get('/v1/tokens')]);
  const heading = h('div', { class: 'head' });
  const count = (nkeys) => fill(heading, title('Users & tokens.',
    `${[users ? plural(users.length, 'person', 'people') : null, plural(nkeys, 'key'), plural(tokens.length, 'token')].filter(Boolean).join(', ')}.`));
  count(keys.length);
  return [
    h('section', {}, heading),
    users ? h('section', {}, peopleList(users)) : null,
    h('section', { id: 'keys' }, keysSection(keys, count)),
    h('section', {}, tokenList(tokens)),
  ];
}

const reload = () => window.dispatchEvent(new HashChangeEvent('hashchange'));

function peopleList(users) {
  return h('div', { style: { paddingBottom: '28px' } },
    h('div', { class: 'head', style: { paddingTop: '56px' } }, subtitle('People.')),
    table([
      { title: 'name', cell: (u) => u.name, class: 'name' },
      { title: 'role', cell: (u) => h('span', { style: u.role === 'admin' ? { color: 'var(--cu)' } : {} }, u.role) },
      { title: 'projects', cell: (u) => u.projects.join(', ') || '—' },
      { title: 'console', cell: (u) => (u.has_password ? 'can sign in' : 'cu only'), class: 'hide-sm' },
      { title: 'last seen', cell: (u) => (u.last_seen ? `${ago(u.last_seen)} ago` : '—') },
      { title: '', cell: (u) => (u.name === session.me.name ? h('span', { class: 'hint' }, 'you') : userActions(u)), class: 'act' },
    ], users),
    h('div', { class: 'pad' }, inviteForm()));
}

function userActions(u) {
  const role = button(u.role === 'admin' ? 'make user' : 'make admin', { small: true });
  role.addEventListener('click', async () => {
    await act(role, () => post(`/v1/users/${enc(u.name)}/role`, { role: u.role === 'admin' ? 'user' : 'admin' }), 'role changed');
    reload();
  });
  const reset = button('reset link', { small: true });
  reset.addEventListener('click', async () => {
    const invite = await act(reset, () => post('/v1/invites', { name: u.name }));
    if (invite) showLink(reset.closest('td'), invite);
  });
  const remove = button('remove', { small: true, danger: true });
  remove.addEventListener('click', async () => {
    if (!window.confirm(`remove ${u.name}? their tokens stop working and their empty projects go. their vms must be deleted first.`)) return;
    await act(remove, () => del(`/v1/users/${enc(u.name)}`), `${u.name} removed`);
    reload();
  });
  return [role, reset, remove];
}

function inviteForm() {
  const name = input({ placeholder: 'they choose', pattern: '[a-z0-9]([a-z0-9-]*[a-z0-9])?' });
  const role = h('select', { class: 'input' }, h('option', { value: 'user' }, 'user'), h('option', { value: 'admin' }, 'admin'));
  const project = input({ placeholder: 'none', pattern: '[a-z0-9]([a-z0-9-]*[a-z0-9])?' });
  const make = button('make invite link ›', { primary: true, type: 'submit' });
  const out = h('div');
  const form = h('form', { class: 'panel' },
    h('span', { class: 'label' }, 'invite someone'),
    h('div', { class: 'invite-row' }, field('name', name), field('role', role), field('new project', project), make),
    out);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const invite = await act(make, () => post('/v1/invites', {
      name: name.value.trim() || undefined, role: role.value, project: project.value.trim() || undefined,
    }));
    if (invite) showLink(out, invite);
  });
  return form;
}

function showLink(where, invite) {
  const url = `${location.origin}/#/invite/${invite.code}`;
  const hours = Math.round((invite.expires_at - Date.now() / 1000) / 3600);
  fill(where, h('div', { class: 'stack', style: { marginTop: '10px', textAlign: 'left' } },
    copyBlock(`${invite.reset ? 'password reset' : 'send this'} · ${invite.role} · works once · ${hours} h`, [url])));
}

function tokenList(tokens) {
  const name = input({ placeholder: 'laptop', required: true, maxlength: 64, 'aria-label': 'token name', style: { width: '200px' } });
  const make = button('new token ›', { primary: true, type: 'submit' });
  const out = h('div', { class: 'pad', style: { paddingTop: 0 } });
  const form = h('form', { class: 'row', style: { gap: '8px' } }, name, make);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const made = await act(make, () => post('/v1/tokens', { name: name.value.trim() }));
    if (made) {
      // Kiln returns the secret this once and stores only its hash.
      fill(out, copyBlock(`${made.token.name} · shown once`, [`$ export CU_URL=${location.origin} CU_TOKEN=${made.secret}`]));
      name.value = '';
      tokens = [...tokens, made.token];
      fill(list, tokenTable(tokens));
    }
  });
  const list = h('div', {}, tokenTable(tokens));
  return h('div', { style: { paddingBottom: '28px' } },
    h('div', { class: 'head row spread', style: { flexDirection: 'row', paddingTop: '56px' } }, subtitle('Tokens.', 'For cu.'), form),
    list, out);
}

function tokenTable(tokens) {
  if (!tokens.length) return empty('no tokens yet.');
  return table([
    { title: 'token', cell: (t) => t.name, class: 'name' },
    { title: 'owner', cell: (t) => t.owner },
    { title: 'made', cell: (t) => `${ago(t.created_at)} ago` },
    { title: 'last used', cell: (t) => (t.last_used ? `${ago(t.last_used)} ago` : 'never') },
    { title: '', cell: (t) => revoke(t), class: 'act' },
  ], tokens);
}

function revoke(t) {
  const btn = button('revoke', { small: true, danger: true });
  btn.addEventListener('click', async () => {
    if (!window.confirm(`revoke ${t.name}? anything using it stops working.`)) return;
    await act(btn, () => del(`/v1/tokens/${enc(t.id)}`), `${t.name} revoked`);
    reload();
  });
  return btn;
}
