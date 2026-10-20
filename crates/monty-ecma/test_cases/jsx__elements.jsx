// JSX as a templating language: every element evaluates to a plain
// `{ type, props, children }` node (or a component's result), and the same
// tree is what the host receives.

// === Intrinsic elements ===
console.log(<div />);
console.log(<div id="a" hidden data-n={1} aria-label={'x'} />);
console.log(<p>plain text</p>);
console.log(<custom-tag x:y="ns" />);

// === Children: flattening, dropping and stringifying ===
const items = ['a', 'b'];
console.log(<ul>{items.map((item) => <li key={item}>{item}</li>)}</ul>);
console.log(<span>{1}{true}{false}{null}{undefined}{[3, [4, [5]]]}</span>);
console.log(<span>{items.length > 5 ? 'many' : null}{items.length > 1 && 'some'}</span>);
console.log(<span>{[]}{[[]]}{''}</span>);

// === Whitespace and entities follow the JSX text rules ===
console.log(
  <p>
    first line
    second   line
    {'expr'} tail
  </p>,
);
console.log(<p>a &amp; b &lt; c &gt; d &quot;q&quot; &#169; &nbsp;x</p>);
console.log(<p title="&amp; &lt;x&gt;" plain="a &amp; b" />);
console.log(<p>  leading and trailing on one line  </p>);
console.log(<p>{'  kept  '}</p>);

// === Spread attributes and the children prop ===
const extra = { b: 2, c: 3 };
console.log(<div a={1} {...extra} c={4} />);
console.log(<div children="from prop" />);
console.log(<div children="overridden">between</div>);
console.log(<div {...{ children: ['x', 'y'] }} />);

// === Fragments ===
console.log(<></>);
console.log(<><b>1</b><i>2</i></>);
console.log(<div><>{'a'}{'b'}</>c</div>);
console.log(<Fragment key="k">frag</Fragment>);

// === Components ===
function Card({ title, children }) {
  return (
    <section className="card">
      <h2>{title}</h2>
      {children}
    </section>
  );
}
console.log(<Card title="Users" />);
console.log(<Card title="Users"><p>one</p></Card>);
console.log(<Card title="Users"><p>one</p><p>two</p></Card>);
const Echo = (props) => props;
console.log(<Echo />);
console.log(<Echo a={1} key="k">child</Echo>);
console.log(<Echo>{1}{2}</Echo>);
const Nothing = () => null;
console.log(<Nothing />, <div><Nothing /></div>);
const Text = () => 'just text';
console.log(<Text />, <div><Text />!</div>);
const List = () => [<li>1</li>, 'loose', 2];
console.log(<ul><List /></ul>);
const Outer = ({ children }) => <div>{children}</div>;
console.log(<Outer><Card title="t">x</Card></Outer>);

// === Member and this expressions as types ===
const UI = { Button: ({ label }) => <button type="button">{label}</button> };
console.log(<UI.Button label="go" />);

// === Elements are plain data ===
const node = <div a="1">x</div>;
console.log(JSON.stringify(node), Object.getPrototypeOf(node) === Object.prototype, Array.isArray(node.children));
console.log(Object.keys(node), node.type, node.props.a, node.children[0]);
node.children.push('appended');
console.log(node.children);
console.log(typeof jsx, jsx.name, jsx.length, typeof Fragment, Object.isFrozen(Fragment));
console.log(jsx('p', { id: 'manual' }, 'a', 'b'));
console.log(jsx('p', null));
console.log(jsx(Fragment, null, 'a', ['b']));

// === Invalid children and types ===
function failing(make) {
  try {
    make();
    console.log('no error');
  } catch (error) {
    console.log(`${error.constructor.name}: ${error.message}`);
  }
}
failing(() => <div>{{ plain: 'object' }}</div>);
failing(() => <div>{Symbol('s')}</div>);
failing(() => <div>{() => 1}</div>);
failing(() => jsx(undefined, null));
failing(() => jsx(null, null));
failing(() => jsx(42, null));
failing(() => jsx({}, null));
failing(() => {
  const Broken = () => {
    throw new RangeError('inside');
  };
  return <Broken />;
});
