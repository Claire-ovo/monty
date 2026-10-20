// The reference `jsx`, `Fragment` and `renderToString` the Node side of a JSX
// case runs: the harness lowers `.jsx` / `.tsx` cases with oxc's classic
// transform (`jsx(type, props, ...children)`) and prepends this file, so the
// engine's natives (`crates/monty-ecma/src/builtins/jsx.rs`) are checked
// against this JavaScript line for line. Keep the two in step.

const Fragment = Object.freeze({});

const VOID_ELEMENTS = new Set([
  'area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'keygen', 'link', 'meta', 'param', 'source', 'track',
  'wbr',
]);

const MAX_TREE_DEPTH = 512;

function jsx(type, props, ...children) {
  if (type === Fragment) return finishChildren(undefined, normalizeChildren(children));
  if (typeof type === 'string') return element(type, props, children);
  if (typeof type === 'function') return component(type, props, children);
  throw new TypeError(`Element type is invalid: expected a string or a function but got: ${typeName(type)}`);
}

function typeName(value) {
  return value === null ? 'null' : typeof value;
}

function element(type, props, children) {
  const node = { type, props: Object.assign({}, props), children: [] };
  let raw;
  if (children.length === 0) {
    raw = Object.hasOwn(node.props, 'children') ? [node.props.children] : [];
  } else {
    raw = children;
  }
  delete node.props.children;
  return finishChildren(node, normalizeChildren(raw));
}

function component(type, props, children) {
  const passed = Object.assign({}, props);
  if (children.length === 1) passed.children = children[0];
  else if (children.length > 1) passed.children = children;
  return type(passed);
}

function finishChildren(owner, { flat, pending }) {
  if (owner !== undefined) owner.children = flat;
  if (!pending) return owner === undefined ? flat : owner;
  return Promise.all(flat).then((values) => finishChildren(owner, normalizeChildren(values)));
}

function normalizeChildren(items) {
  const flat = [];
  let pending = false;
  const visit = (item, depth) => {
    if (depth > MAX_TREE_DEPTH) throw new RangeError('Maximum call stack size exceeded');
    if (item === undefined || item === null || typeof item === 'boolean') return;
    if (typeof item === 'number' || typeof item === 'bigint') {
      flat.push(String(item));
    } else if (typeof item === 'string') {
      flat.push(item);
    } else if (typeof item === 'symbol') {
      throw new TypeError('Symbols are not valid as a JSX child');
    } else if (Array.isArray(item)) {
      for (const inner of item) visit(inner, depth + 1);
    } else if (typeof item.then === 'function') {
      pending = true;
      flat.push(item);
    } else if (isNode(item)) {
      flat.push(item);
    } else {
      throw new TypeError('Objects are not valid as a JSX child');
    }
  };
  for (const item of items) visit(item, 0);
  return { flat, pending };
}

function isNode(value) {
  return Object.hasOwn(value, 'type') && Object.hasOwn(value, 'children');
}

function renderToString(tree) {
  if (tree !== null && (typeof tree === 'object' || typeof tree === 'function') && typeof tree.then === 'function') {
    return Promise.resolve(tree).then(renderToString);
  }
  const out = [];
  const escape = (text) => text.replace(/[&<>"']/g, (c) => ESCAPES[c]);
  const cssName = (name) => (name.startsWith('--') ? name : name.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`));
  const styleText = (style) => {
    const parts = [];
    for (const name of Object.keys(style)) {
      const value = style[name];
      if (value === undefined || value === null || typeof value === 'boolean') continue;
      const text = String(value);
      if (typeof value !== 'number' && text === '') continue;
      parts.push(`${cssName(name)}:${text}`);
    }
    return parts.join(';');
  };
  const attribute = (name, value) => {
    if (name === 'className') name = 'class';
    else if (name === 'htmlFor') name = 'for';
    let text;
    if (value === undefined || value === null) return;
    if (value === true) {
      out.push(' ', name);
      return;
    }
    if (value === false) {
      if (!name.startsWith('aria-') && !name.startsWith('data-')) return;
      text = 'false';
    } else if (typeof value === 'symbol') {
      throw new TypeError('Symbols are not valid as attribute values');
    } else if (typeof value === 'function') {
      return;
    } else if (name === 'style' && typeof value === 'object') {
      text = styleText(value);
    } else {
      text = String(value);
    }
    out.push(' ', name, '="', escape(text), '"');
  };
  const innerHtmlOf = (value) => {
    if (value === null || (typeof value !== 'object' && typeof value !== 'function')) return undefined;
    const html = value.__html;
    return html === undefined || html === null ? undefined : String(html);
  };
  const renderElement = (node, depth) => {
    const type = node.type;
    if (typeof type !== 'string') {
      throw new TypeError(`Element type is invalid: expected a string or a function but got: ${typeName(type)}`);
    }
    out.push('<', type);
    let innerHtml;
    const props = node.props;
    if (props !== null && (typeof props === 'object' || typeof props === 'function')) {
      for (const name of Object.keys(props)) {
        const value = props[name];
        if (name === 'key' || name === 'ref' || name === 'children') continue;
        if (name === 'dangerouslySetInnerHTML') {
          innerHtml = innerHtmlOf(value);
          continue;
        }
        attribute(name, value);
      }
    }
    out.push('>');
    if (VOID_ELEMENTS.has(type)) return;
    if (innerHtml !== undefined) out.push(innerHtml);
    else render(node.children, depth + 1);
    out.push('</', type, '>');
  };
  const render = (value, depth) => {
    if (depth > MAX_TREE_DEPTH) throw new RangeError('Maximum call stack size exceeded');
    if (value === undefined || value === null || typeof value === 'boolean') return;
    if (typeof value === 'number' || typeof value === 'bigint' || typeof value === 'string') {
      out.push(escape(String(value)));
    } else if (typeof value === 'symbol') {
      throw new TypeError('Symbols are not valid as a JSX child');
    } else if (Array.isArray(value)) {
      for (const item of value) render(item, depth + 1);
    } else if (isNode(value)) {
      renderElement(value, depth);
    } else {
      throw new TypeError('Objects are not valid as a JSX child');
    }
  };
  render(tree, 0);
  return out.join('');
}

const ESCAPES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#x27;' };
