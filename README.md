# Krypper

Krypper is a local command-line tool. It uses only Python's standard library
and runs offline.

## Local CLI

Build/check syntax:

```sh
python3 -m py_compile krypper.py
```

Run from the directory containing the file you want to display:

```sh
python3 krypper.py show README.md
```

`show` writes the selected file to stdout. The path must be relative and resolve
inside the current working directory. Only regular UTF-8 files up to 1 MiB are
accepted; absolute paths, missing files, directories, invalid UTF-8, and paths
that escape the current directory are rejected.

Test:

```sh
python3 -m unittest discover -s tests -v
```

The build command checks Python syntax and does not produce a packaged
executable.

Contributions should go through reviewed pull requests and the existing required checks.

See the [verify workflow](.github/workflows/verify.yml).
