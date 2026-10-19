# monty-js-runtime

The `monty-js` binary: the JavaScript sandbox of
[Monty](https://github.com/pydantic/monty), built on the `monty-ecma` engine
and served behind the same wire protocol as the Python `monty` worker, so
`monty-pool` and the language bindings drive it without changes.

```console
$ monty-js -e "console.log('hello', [1, 2, 3])"
hello [ 1, 2, 3 ]
```

## Usage

- `monty-js file.js` — run a JavaScript file (`node`-style: output on stdout,
  an uncaught error on stderr with exit status 1)
- `monty-js -e "<code>"` — run a program passed as a string
- `--max-memory`, `--max-feed-duration`, `--max-recursion-depth` — sandbox
  resource limits
- `monty-js subprocess` — run as a protocol child for `monty-pool`

Every program runs as a strict-mode ES module. See the Monty documentation
for the supported subset of the language.
