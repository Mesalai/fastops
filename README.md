# fastops

Parallel copy, move, delete, clean and size for large folders — made for trees with millions of
small files (datasets, checkpoints, caches), where `cp -r`, `mv`, `rm -rf` and `du` work
one file at a time.

```
fastops copy   SRC DEST  [--skip-existing]
fastops move   SRC DEST  [--skip-existing]
fastops delete SRC
fastops clean  [PATH]    [--dry-run] [--yes] [--min-size 100M] [--venv]
fastops size   PATH      [--tree [--min-size 100G]]
```

## Speed

Every test uses 1,000,000 files (4 KiB each, 1,000 folders of 1,000 files) unless noted.
Times are the best of 3 runs; copies, moves, deletes and cleans are timed until the data is
on disk (`sync`).

**copy**

| | fastops | cp -r | rsync -a | fcp | xcp -r |
|---|---|---|---|---|---|
| 1,000,000 files of 4 KiB | **4.4 s** | 12.7 s | 19.8 s | 4.3 s | 8.6 s |
| 10 files of 64 MiB | **0.12 s** | 0.23 s | 0.37 s | 0.14 s | 0.13 s |
| 500,000 files of 200 KB (100 GB) | **110 s** (0.91 GB/s) | – | – | 119 s (0.84 GB/s) | – |
| `--skip-existing`, all 1,000,000 already there | **0.39 s** | 1.9 s (`-n`) | 2.7 s (`--ignore-existing`) | – | stops with an error (`--no-clobber`) |

**move**

| | fastops | mv | rsync -a --remove-source-files |
|---|---|---|---|
| to a new place on the same disk | **0.00 s** (one rename) | 0.00 s | – |
| into a folder that already has the same folders (merge) | **7.8 s** | refuses (`Directory not empty`) | 22.4 s |
| to another disk | **23.5 s** | 29.7 s | 26.7 s |

**delete**

| | fastops delete | rm -rf | find -delete |
|---|---|---|---|
| 1,000,000 files | **1.6 s** | 5.8 s | 5.6 s |

**clean** (1,000 projects, each with a `package.json` and a `node_modules` of 1,000 files)

| | fastops clean | find -name node_modules -prune -exec rm -rf |
|---|---|---|
| find and size them (`--dry-run`) | **0.20 s** | – |
| find and delete them | **2.6 s** | 6.2 s (deletes any folder named node_modules) |

**size** (folder already in the system's file cache)

| | fastops size | du | dust |
|---|---|---|---|
| total | **0.10 s** | 0.75 s (`-s`) | 0.24 s (`-d 0`) |
| every folder of 1 MB or more | **0.10 s** (`--tree --min-size 1M`) | 0.75 s (`-h -t 1M`) | 0.32 s (`-z 1M -n 100000`) |

Intel Core Ultra 9 275HX (24 cores), Linux 7.2. ext4 on two NVMe SSDs in RAID 0; "another
disk" is a btrfs NVMe SSD with LUKS encryption. GNU coreutils 9.10, rsync 3.5.0, fcp 0.2.2,
xcp 0.24.8, dust 1.2.6. The 100 GB copy is limited by how fast the disk can write (runs
varied from 110 to 144 s for both tools). fcp is as fast for plain copies; fastops also moves, deletes, cleans
and sizes, and lists everything it could not handle.

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
- **delete** removes SRC right away, without asking, like `rm -rf`. The filesystem root,
  your home folder and its parent folders are always refused. Symlinks are removed as links:
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
| 2 | nothing done: bad arguments, refused, or clean not confirmed |

## Install

```
cargo install fastops
```

The original flag style (`fastops --c SRC DEST`, `--m`, `--d`, `--check-size`) still works.

## License

MIT, see [LICENSE](LICENSE).
