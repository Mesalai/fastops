# fastops

Parallel copy, move, delete, clean and size for large folders — made for trees with millions of
small files (datasets, checkpoints, caches), where `cp -r`, `mv`, `rm -rf` and `du` work
one file at a time.

```
fastops copy   SRC DEST  [--skip-existing]
fastops move   SRC DEST  [--skip-existing]
fastops delete SRC       [--yes]
fastops clean  [PATH]    [--dry-run] [--yes] [--min-size 100M] [--venv]
fastops size   PATH      [--tree [--min-size 100G]]
```

## What it does

- **copy** works like `cp -r`: if DEST is an existing folder, SRC goes inside it
  (`DEST/<name of SRC>`); a SRC ending in `/` copies its contents into DEST. Files are
  copied in parallel. Empty folders are recreated, file and folder timestamps and
  permissions are kept, and symlinks are copied as links (never followed).
- **move** works like `mv`. When the destination does not exist yet, the whole tree is
  moved with a single rename (instant on the same filesystem). Otherwise every file is
  renamed in parallel, or copied and then removed when it crosses filesystems; emptied
  source folders are removed afterwards. Across filesystems each file is copied under a
  temporary name, renamed into place once complete, and only then removed from the source,
  so a failure (unreadable file, full disk) leaves the original and no half-written file.
- **delete** lists what it will remove (files, links, folders) and asks before deleting.
  `--yes` skips the question; without a terminal it refuses unless `--yes` is given. The
  filesystem root, your home folder and its parent folders are always refused. Symlinks are removed as links:
  what they point to is never touched.
- **clean** finds build and cache folders that their tools recreate and lists them, largest
  first, with the space they take; then asks before deleting (`--dry-run` only lists). A
  folder is only taken when its project file proves what it is:

  | Folder | Taken when |
  |---|---|
  | `node_modules`, `.next` | a `package.json` is next to it |
  | `target` | a `Cargo.toml` is next to it and Cargo's `CACHEDIR.TAG` is inside |
  | `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache` | always |
  | `.gradle` | a Gradle build file is next to it |
  | `.venv`, `venv` | `pyvenv.cfg` is inside, and only with `--venv` |

  Symlinked folders (e.g. pnpm's `node_modules`) and anything inside `.git` are left alone.
- **size** totals a folder (size and file count) with a parallel walk, hidden files
  included and a hard-linked file counted once (like `du`); `--tree` shows every subfolder,
  `--min-size` hides the small ones.

Existing files at the destination are overwritten by default, like `cp` and `mv`;
`--skip-existing` (`-n`) keeps them (a skipped file stays at the source when moving).
Copying or moving something onto itself or a folder into itself is refused. A symlink given
as SRC is the link itself, also as `link/`: delete removes the link, never the folder it
points to. `delete .` and `delete ..` are refused.

## Nothing is skipped silently

Every folder that cannot be read, file that cannot be copied, moved or removed, and special
file (device, socket, pipe) is listed at the end, and the exit status is non-zero. A
"complete" message means everything was handled.

| Exit status | Meaning |
|---|---|
| 0 | done, no problems |
| 1 | done, but some entries failed (listed) |
| 2 | nothing done: bad arguments, refused, or delete not confirmed |

## Install

```
cargo install --path .
```

The original flag style (`fastops --c SRC DEST`, `--m`, `--d`, `--check-size`) still works.

## License

MIT, see [LICENSE](LICENSE).
