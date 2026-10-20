// Async components: a thenable anywhere in the children makes the element a
// promise of its node, settled once every child has.

const log = (label: string) => console.log(label);

// === An async component makes its parent a promise ===
async function Users() {
  log('Users start');
  const rows = await Promise.resolve(['ada', 'lin']);
  log('Users done');
  return <ul>{rows.map((row) => <li key={row}>{row}</li>)}</ul>;
}
const page = <main><Users /><p>static</p></main>;
console.log(typeof page, typeof (page as Promise<unknown>).then);
log('after construction');
const node = await page;
console.log(node.type, node.children.length, node.children[0].type, node.children[0].children.length);

// === Siblings start together and settle in order ===
const delayed = (label: string, ms: number) =>
  new Promise<string>((resolve) => setTimeout(() => { log(`${label} resolved`); resolve(label); }, ms));
async function Slow() {
  log('Slow start');
  return <i>{await delayed('slow', 20)}</i>;
}
async function Fast() {
  log('Fast start');
  return <b>{await delayed('fast', 5)}</b>;
}
console.log(await renderToString(<div><Slow /><Fast /><Slow /></div>));

// === renderToString returns a string or a promise ===
console.log(typeof renderToString(<p>sync</p>), typeof renderToString(<p><Fast /></p>));
console.log(await renderToString(<p><Fast /></p>));
console.log(await renderToString(Promise.resolve(<p>from a promise</p>)));
console.log(await renderToString(Promise.resolve(['a', Promise.resolve('b') as unknown as string])) === 'a[object Promise]' ? 'nested promise stringified' : 'unexpected');

// === Promises as children, nested and in fragments ===
console.log(await renderToString(<p>{Promise.resolve('x')}{Promise.resolve([<b>y</b>, Promise.resolve('z')])}</p>));
console.log(await renderToString(<>{Promise.resolve(1)}{Promise.resolve(null)}{2}</>));
const settled = await (<p>{Promise.resolve(['a', ['b']])}</p> as Promise<{ children: unknown[] }>);
console.log(settled.children);
async function Nested() {
  return <section><Users /></section>;
}
console.log(await renderToString(<Nested />));
async function Empty() {
  return null;
}
async function ReturnsList() {
  return [<i>1</i>, Promise.resolve('two')];
}
console.log(await renderToString(<div><Empty /><ReturnsList /></div>));

// === Components receive settled props, not promises ===
const Shows = ({ children }: { children?: unknown }) => <span>{String(typeof children)}</span>;
console.log(await renderToString(<Shows>{Promise.resolve('p')}</Shows>));

// === Rejections propagate ===
async function Failing() {
  throw new RangeError('component failed');
}
try {
  await renderToString(<div><Failing /></div>);
} catch (error) {
  console.log(`${(error as Error).constructor.name}: ${(error as Error).message}`);
}
try {
  await (<div>{Promise.reject(new TypeError('child failed'))}</div>);
} catch (error) {
  console.log(`${(error as Error).constructor.name}: ${(error as Error).message}`);
}
try {
  await renderToString(Promise.resolve({ not: 'a node' }));
} catch (error) {
  console.log(`${(error as Error).constructor.name}: ${(error as Error).message}`);
}
try {
  await (<div>{Promise.resolve({ not: 'a node' })}</div>);
} catch (error) {
  console.log(`${(error as Error).constructor.name}: ${(error as Error).message}`);
}
log('end');
