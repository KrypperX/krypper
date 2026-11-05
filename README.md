# Krypper

Krypper is a local command-line tool. It uses only Python's standard library
and runs offline.

## Local CLI

Build/check syntax:

```sh
python3 -m py_compile krypper.py
```

Run commands from the workspace directory:

```sh
python3 krypper.py show README.md
python3 krypper.py list
python3 krypper.py list docs
python3 krypper.py write notes/today.txt "Remember to review the change."
python3 krypper.py write --stdin notes/from-pipe.txt < notes/draft.txt
```

`show` writes the selected file to stdout. The path must be relative and resolve
inside the current working directory. Only regular UTF-8 files up to 1 MiB are
accepted; absolute paths, missing files, directories, invalid UTF-8, and paths
that escape the current directory are rejected.

`list` prints JSON-quoted names for the immediate entries in the selected
directory, in sorted order. The path defaults to the current directory and
must be relative and stay inside it, including through symlinks. Hidden names
are included; entries are not followed and subdirectories are not traversed.
Control characters are escaped, and the command never reads entry contents.
Absolute paths, missing paths, non-directories, and paths that escape the
current directory are rejected.

`write <path> <text>` creates one new UTF-8 text file and never replaces an
existing file. The target path must be relative, remain inside the current
directory, and have existing directory parents. Symlink path components and
targets are refused. Text is limited to 1 MiB after UTF-8 encoding; the command
does not create parent directories or contact a network service. Use
`write --stdin <path>` to read UTF-8 text from standard input; input is limited
to 1 MiB and invalid UTF-8 is rejected before a file is created.

Test:

```sh
python3 -m unittest discover -s tests -v
```

The build command checks Python syntax and does not produce a packaged
executable.

Contributions should go through reviewed pull requests and the existing required checks.

See the [verify workflow](.github/workflows/verify.yml).
