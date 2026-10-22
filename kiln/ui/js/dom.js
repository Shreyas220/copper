// A tiny DOM builder. Text always goes in as text, never parsed as HTML, so
// names and messages from the API can't inject markup.
//
//   h('a', { href: '#/nodes', class: 'btn' }, 'nodes ›')
//   h('button', { onclick: () => go() }, 'go')
//
// Children may be nodes, strings, numbers, arrays, or null/false to skip.

export function h(tag, props, ...children) {
  const el = document.createElement(tag);
  setProps(el, props);
  append(el, children);
  return el;
}

const SVG_NS = 'http://www.w3.org/2000/svg';

export function svg(tag, attrs, ...children) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs ?? {})) {
    if (v != null) el.setAttribute(k, String(v));
  }
  append(el, children);
  return el;
}

function setProps(el, props) {
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value == null || value === false) continue;
    if (key.startsWith('on') && typeof value === 'function') {
      el.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key === 'class') {
      el.className = value;
    } else if (key === 'style' && typeof value === 'object') {
      Object.assign(el.style, value);
    } else if (key === 'value' && 'value' in el) {
      el.value = value;
    } else if (value === true) {
      el.setAttribute(key, '');
    } else {
      el.setAttribute(key, String(value));
    }
  }
}

function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child == null || child === false || child === true) continue;
    el.append(child instanceof Node ? child : String(child));
  }
}

/** Replace everything inside `el`. */
export function fill(el, ...children) {
  el.replaceChildren();
  append(el, children);
  return el;
}
