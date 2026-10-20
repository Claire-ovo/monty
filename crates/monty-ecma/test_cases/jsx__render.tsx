// renderToString: HTML from a tree, with typed components.

type Props = { title: string; count?: number; children?: unknown };

// === Elements, text and escaping ===
console.log(renderToString(<div />));
console.log(renderToString(<p>plain</p>));
console.log(renderToString(<p>{'<script>alert("x & y")</script>'}</p>));
console.log(renderToString(<p title={'say "hi" & <go>'} data-q="it's">t</p>));
console.log(renderToString(<ul>{['a', 'b'].map((item) => <li key={item}>{item}</li>)}</ul>));
console.log(renderToString(<span>{1}{2.5}{-0}{1e21}{null}{false}{[4, [5]]}</span>));

// === Void elements take no closing tag ===
console.log(renderToString(<div><br /><hr /><img src="x.png" alt="" /><input value="v" /></div>));
console.log(renderToString(<br>ignored</br>));
console.log(renderToString(<area shape="rect" />), renderToString(<wbr />), renderToString(<source />));

// === Attribute spellings and values ===
console.log(renderToString(<label className="a b" htmlFor="field">L</label>));
console.log(renderToString(<label class="c" for="field2">L</label>));
console.log(renderToString(<input disabled checked={false} readOnly={true} tabIndex={0} maxLength={10} />));
console.log(renderToString(<div aria-hidden={false} data-active={false} aria-busy />));
console.log(renderToString(<div onClick={() => 1} ref={{}} key="k" id={undefined} title={null} />));
console.log(renderToString(<a href="https://example.com/a?b=1&c=2" count={[1, 2]} obj={{ toString: () => 'custom' }} />));

// === style objects ===
console.log(renderToString(<div style={{ backgroundColor: 'red', fontSize: 12, '--gap': '4px', WebkitMask: 'none' }} />));
console.log(renderToString(<div style={{ color: undefined, display: null, hidden: true, width: '', height: 0 }} />));
console.log(renderToString(<div style="color: blue" />));
console.log(renderToString(<div style={{}} />));

// === dangerouslySetInnerHTML ===
console.log(renderToString(<div dangerouslySetInnerHTML={{ __html: '<i>raw</i> & more' }}>ignored children</div>));
console.log(renderToString(<div dangerouslySetInnerHTML={{ __html: 7 }} />));
console.log(renderToString(<div dangerouslySetInnerHTML={{}}>kept</div>));
console.log(renderToString(<div dangerouslySetInnerHTML={undefined}>kept</div>));

// === Components and fragments ===
function Card({ title, count = 0, children }: Props) {
  return (
    <section className="card">
      <h2>{title}</h2>
      {count > 0 ? <em>{count}</em> : null}
      {children}
    </section>
  );
}
const Rows = ({ rows }: { rows: string[] }) => (
  <>
    {rows.map((row) => <tr key={row}><td>{row}</td></tr>)}
  </>
);
console.log(renderToString(<Card title="Users &amp; co" count={2}><table><Rows rows={['x', 'y']} /></table></Card>));
console.log(renderToString(<><Card title="a" /><Card title="b" /></>));
const page = (
  <html lang="en">
    <head><meta charSet="utf-8" /><title>Hi</title></head>
    <body><Card title="Body" /></body>
  </html>
);
console.log('<!doctype html>' + renderToString(page));

// === Things that are not elements ===
console.log(JSON.stringify(renderToString('just text & <b>')), JSON.stringify(renderToString(42)));
console.log(JSON.stringify(renderToString(null)), JSON.stringify(renderToString(undefined)), JSON.stringify(renderToString(true)));
console.log(renderToString(['a', <b>b</b>, ['c']]));
console.log(renderToString({ type: 'p', props: { id: 'hand-built' }, children: ['from the host side'] }));
console.log(renderToString({ type: 'p', props: null, children: null }));
console.log(renderToString({ type: 'em', children: 'single string child' }));

// === Errors ===
function failing(make: () => unknown) {
  try {
    make();
    console.log('no error');
  } catch (error) {
    console.log(`${(error as Error).constructor.name}: ${(error as Error).message}`);
  }
}
failing(() => renderToString({ plain: 'object' }));
failing(() => renderToString({ type: 'div', props: {}, children: [{ nope: 1 }] }));
failing(() => renderToString({ type: 7, props: {}, children: [] }));
failing(() => renderToString({ type: Card, props: {}, children: [] }));
failing(() => renderToString(<div>{Symbol('s') as unknown as string}</div>));
failing(() => renderToString(<div id={Symbol('s') as unknown as string} />));
