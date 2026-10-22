import { post } from '../api.js';
import { h } from '../dom.js';
import { loadSession } from '../session.js';
import { logo } from '../shell.js';
import { button, field, input, notice } from '../ui.js';

/** The door: a picture on the left, a form on the right. */
export function door(form) {
  return h('div', { class: 'door' },
    h('div', { class: 'card' },
      h('div', { class: 'art', 'aria-hidden': 'true' }, h('span', { class: 'label' }, 'FIG.0 · copper')),
      form),
    h('p', { class: 'label where' }, location.host));
}

export const brandRow = () => h('div', { class: 'row', style: { font: "600 18px/1 var(--mono)" } }, logo(), 'copper');

export default async function signin() {
  const name = input({ name: 'name', autocomplete: 'username', required: true, autofocus: true });
  const password = input({ name: 'password', type: 'password', autocomplete: 'current-password', required: true });
  const error = h('div');
  const submit = button('sign in ›', { primary: true, wide: true, type: 'submit' });
  const form = h('form', {},
    brandRow(),
    h('h1', { class: 'title' }, h('b', {}, 'Sign in.')),
    field('name', name),
    field('password', password),
    error,
    submit);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    submit.disabled = true;
    error.replaceChildren();
    try {
      await post('/v1/session', { name: name.value.trim(), password: password.value });
      await loadSession();
      location.hash = '#/overview';
    } catch (err) {
      error.replaceChildren(notice(err.message, 'bad'));
      password.select();
    } finally {
      submit.disabled = false;
    }
  });
  return door(form);
}
