# Krypper

Krypper currently has a minimal local command-line entry point. It uses only
Python's standard library and runs offline.

## Local CLI

Build/check syntax:

```sh
python3 -m py_compile krypper.py
```

Run:

```sh
python3 krypper.py --help
```

Test:

```sh
python3 -m unittest discover -s tests -v
```

The CLI currently provides usage output and rejects unsupported arguments; it
does not yet perform product operations. The build command checks Python syntax
and does not produce a packaged executable.

Contributions should go through reviewed pull requests and the existing required checks.

See the [verify workflow](.github/workflows/verify.yml).
