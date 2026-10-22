import { enc, get, post } from '../api.js';
import { h } from '../dom.js';
import { loadSession } from '../session.js';
import { button, field, input, notice } from '../ui.js';
import { brandRow, door } from './signin.js';

const MIN_PASSWORD = 10;

/** Opening an invite link: pick a password, and a name if the invite allows. */
export default async function invite(_ctx, code) {
  let info;
  try {
    info = await get(`/v1/invites/${enc(code)}`);
  } catch {
    return door(h('form', {}, brandRow(),
      h('h1', { class: 'title' }, h('b', {}, 'Link expired.'), ' Or already used.'),
      button('sign in', { href: '#/signin' })));
  }
  const name = input({ value: info.name ?? '', readonly: info.name ? true : null, autocomplete: 'username', required: true,
    pattern: '[a-z0-9]([a-z0-9-]*[a-z0-9])?', title: 'lowercase letters, digits and dashes' });
  const password = input({ type: 'password', autocomplete: 'new-password', required: true, minlength: MIN_PASSWORD });
  const again = input({ type: 'password', autocomplete: 'new-password', required: true });
  const error = h('div');
  const submit = button(info.reset ? 'set new password ›' : 'join ›', { primary: true, wide: true, type: 'submit' });
  const heading = info.reset
    ? h('h1', { class: 'title' }, h('b', {}, 'New password.'), ` For ${info.name}.`)
    : h('h1', { class: 'title' }, h('b', {}, 'Join copper.'), info.project ? ` Your project: ${info.project}.` : '');
  const form = h('form', {},
    brandRow(),
    heading,
    field('name', name, info.name ? null : 'a-z 0-9 -'),
    field('password', password, `at least ${MIN_PASSWORD} characters`),
    field('again', again),
    error,
    submit,
    h('p', { class: 'hint' }, `role: ${info.role}`));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    error.replaceChildren();
    if (password.value !== again.value) {
      error.append(notice("the passwords don't match", 'bad'));
      return;
    }
    submit.disabled = true;
    try {
      await post(`/v1/invites/${enc(code)}/accept`, { name: name.value.trim(), password: password.value });
      await loadSession();
      location.hash = '#/overview';
    } catch (err) {
      error.append(notice(err.message, 'bad'));
    } finally {
      submit.disabled = false;
    }
  });
  return door(form);
}
