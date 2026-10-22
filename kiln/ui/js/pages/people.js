import { del, enc, get, post } from '../api.js';
import { fill, h } from '../dom.js';
import { ago, plural } from '../format.js';
import { isAdmin, session } from '../session.js';
import { act, button, copyBlock, empty, field, input, subtitle, table, title } from '../ui.js';

/** People and tokens. Admins invite people and see everyone's tokens;
 *  everyone manages their own tokens for cu. */
export default async function people() {
  const [users, tokens] = await Promise.all([isAdmin() ? get('/v1/users') : Promise.resolve(null), get('/v1/tokens')]);
  return [
    h('section', {}, h('div', { class: 'head' }, title('Users & tokens.', users ? `${plural(users.length, 'person', 'people')}, ${plural(tokens.length, 'token')}.` : `${plural(tokens.length, 'token')}.`))),
    users ? h('section', {}, peopleList(users)) : null,
    users ? h('section', {}, inviteForm()) : null,
    h('section', {}, tokenList(tokens)),
  ];
}

const reload = () => window.dispatchEvent(new HashChangeEvent('hashchange'));

function peopleList(users) {
  return h('div', {},
    h('div', { class: 'pad' }, subtitle('People.')),
    table([
      { title: 'name', cell: (u) => u.name },
      { title: 'role', cell: (u) => u.role },
      { title: 'projects', cell: (u) => u.projects.join(', ') || '—' },
      { title: 'console', cell: (u) => (u.has_password ? 'can sign in' : 'cu only'), class: 'hide-sm' },
      { title: 'last seen', cell: (u) => (u.last_seen ? `${ago(u.last_seen)} ago` : '—') },
      { title: '', cell: (u) => (u.name === session.me.name ? h('span', { class: 'hint' }, 'you') : userActions(u)) },
    ], users));
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
  return h('div', { class: 'row', style: { gap: '6px' } }, role, reset, remove);
}

function inviteForm() {
  const name = input({ placeholder: 'leave empty to let them choose', pattern: '[a-z0-9]([a-z0-9-]*[a-z0-9])?' });
  const role = h('select', { class: 'input' }, h('option', { value: 'user' }, 'user'), h('option', { value: 'admin' }, 'admin'));
  const project = input({ placeholder: 'e.g. games · optional' });
  const make = button('make invite link ›', { primary: true, type: 'submit' });
  const out = h('div');
  const form = h('form', { class: 'pad stack' },
    subtitle('Invite someone.'),
    h('div', { class: 'cells four', style: { background: 'none', gap: '16px' } }, field('name', name), field('role', role), field('project', project)),
    h('div', { class: 'row' }, make),
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
  fill(where, h('div', { class: 'stack', style: { marginTop: '10px' } },
    copyBlock(`${invite.reset ? 'password reset' : 'invite'} · ${invite.role} · one use · expires in ${hours} h`, [url])));
}

function tokenList(tokens) {
  const name = input({ placeholder: 'laptop', required: true, maxlength: 64 });
  const make = button('new token ›', { primary: true, type: 'submit' });
  const out = h('div');
  const form = h('form', { class: 'row', style: { alignItems: 'flex-end' } }, h('div', { style: { flex: '1', maxWidth: '360px' } }, field('name', name)), make);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const made = await act(make, () => post('/v1/tokens', { name: name.value.trim() }));
    if (made) {
      // Kiln returns the secret this once and stores only its hash.
      fill(out, copyBlock(`${made.token.name} · shown once`, [`$ export CU_URL=${location.origin} CU_TOKEN=${made.secret}`]));
      name.value = '';
      fill(list, tokenTable([...tokens, made.token]));
    }
  });
  const list = h('div', {}, tokenTable(tokens));
  return h('div', { class: 'pad stack' }, subtitle('Tokens.'), list, form, out);
}

function tokenTable(tokens) {
  if (!tokens.length) return empty('no tokens yet.');
  return table([
    { title: 'token', cell: (t) => t.name },
    { title: 'owner', cell: (t) => t.owner },
    { title: 'made', cell: (t) => `${ago(t.created_at)} ago` },
    { title: 'last used', cell: (t) => (t.last_used ? `${ago(t.last_used)} ago` : 'never') },
    { title: '', cell: (t) => revoke(t) },
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
