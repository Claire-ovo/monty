"""Locates the worker binary that pools run as subprocesses: `monty` for
Python sessions, `monty-js` for JavaScript ones.

Each binary ships in its own wheel (`pydantic-monty-runtime` and
`pydantic-monty-js-runtime`, maturin `bin`-bindings packages, the same pattern
`uv` and `ruff` use), which installs it into the environment's scripts
directory. Installing `pydantic-monty` pulls the Python worker in, and the
`js` extra the JavaScript one; the `pydantic-monty-client` distribution alone
does not, so the other resolution steps below matter when only the bindings
are installed.

The compiled `_monty` module imports this file during its own init, while
`pydantic_monty/__init__.py` is still executing, so it must import nothing
from `pydantic_monty`.
"""

from __future__ import annotations

import os
import shutil
import sys
import sysconfig
from pathlib import Path

__all__ = ('find_monty_binary',)

_EXE = '.exe' if sys.platform == 'win32' else ''

# per language: the binary's name, the environment variable naming it, and the wheel that ships it
_RUNTIMES = {
    'python': ('monty', 'MONTY_BIN', 'pydantic-monty'),
    'javascript': ('monty-js', 'MONTY_JS_BIN', 'pydantic-monty[js]'),
    'typescript': ('monty-js', 'MONTY_JS_BIN', 'pydantic-monty[js]'),
}


def find_monty_binary(explicit: str | Path | None = None, language: str = 'python') -> str:
    """Resolves the worker binary path used to spawn pool workers.

    Resolution order:

    1. the explicit `binary_path` argument, when given
    2. the `MONTY_BIN` (or `MONTY_JS_BIN`) environment variable
    3. the environment's scripts directory (where the runtime wheel installs
       the binary)
    4. a `monty` (or `monty-js`) executable on `PATH`
    5. a cargo-built binary in the monty workspace, when running from an
       editable/development install of this package
    """
    if explicit is not None:
        return str(explicit)

    name, env_var, package = _RUNTIMES[language]
    if env := os.environ.get(env_var):
        return env

    for scripts_dir in _script_dirs():
        candidate = scripts_dir / f'{name}{_EXE}'
        if candidate.is_file():
            return str(candidate)

    if found := shutil.which(name):
        return found

    if dev := _development_binary(f'{name}{_EXE}'):
        return str(dev)

    raise FileNotFoundError(
        f'could not locate the `{name}` binary required to run sandboxed code; '
        f'install it with `pip install "{package}"` (or `make dev-py` in the monty repo), '
        f'pass binary_path=..., or set the {env_var} environment variable'
    )


def _script_dirs() -> list[Path]:
    """Scripts directories of the current environment, most specific first."""
    dirs = [Path(sysconfig.get_path('scripts'))]
    # user-scheme installs (`pip install --user`) place scripts elsewhere
    try:
        dirs.append(Path(sysconfig.get_path('scripts', f'{os.name}_user')))
    except KeyError:
        pass
    return dirs


def _development_binary(file_name: str) -> Path | None:
    """A cargo-built worker next to an editable install inside the monty repo.

    With `maturin develop`, this file lives at
    `<repo>/crates/monty-python/python/pydantic_monty/_binary.py`, so a
    cargo-built binary is at `<repo>/target/{debug,release}/<file_name>`.
    Prefer the most recently built one so `cargo build` and
    `cargo build --release` both behave as expected.
    """
    repo = Path(__file__).resolve().parents[4]
    if not (repo / 'Cargo.toml').is_file():
        return None
    candidates = [path for profile in ('debug', 'release') if (path := repo / 'target' / profile / file_name).is_file()]
    return max(candidates, key=lambda path: path.stat().st_mtime, default=None)
