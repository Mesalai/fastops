use std::collections::HashMap;
use std::fs::{self, FileTimes};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use clap::{Args, Parser, Subcommand};
use dashmap::DashMap;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

#[derive(Parser)]
#[command(
    name = "fastops",
    about = "Fast large file operations with parallel processing",
    version,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    // The original flag style (`fastops --c SRC DEST` ...), still accepted.
    #[arg(long = "c", value_names = ["SRC", "DEST"], num_args = 2, hide = true)]
    copy: Option<Vec<PathBuf>>,
    #[arg(long = "m", value_names = ["SRC", "DEST"], num_args = 2, hide = true)]
    move_op: Option<Vec<PathBuf>>,
    #[arg(long = "d", value_name = "SRC", hide = true)]
    delete: Option<PathBuf>,
    #[arg(long = "check-size", value_name = "PATH", hide = true)]
    check_size: Option<PathBuf>,
    #[arg(long = "tree", hide = true)]
    tree: bool,
    #[arg(long = "min-size", value_name = "SIZE", requires = "tree", value_parser = parse_size, hide = true)]
    min_size: Option<u64>,
    #[arg(short = 'n', long = "skip-existing", hide = true)]
    skip_existing: bool,
    #[arg(short = 'y', long = "yes", hide = true)]
    yes: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Copy SRC to DEST, like `cp -r`. A SRC ending in `/` copies its contents.
    Copy {
        src: PathBuf,
        dest: PathBuf,
        #[command(flatten)]
        opts: TransferOpts,
    },
    /// Move SRC to DEST, like `mv` (one rename when DEST does not exist yet).
    Move {
        src: PathBuf,
        dest: PathBuf,
        #[command(flatten)]
        opts: TransferOpts,
    },
    /// Delete SRC, after showing what will go and asking.
    Delete {
        src: PathBuf,
        /// Do not ask for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Find build and cache folders (node_modules, Rust target, __pycache__, ...) and delete them.
    Clean {
        /// Where to look (default: the current folder)
        path: Option<PathBuf>,
        /// Only list what would be deleted
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
        /// Leave folders smaller than SIZE, e.g. 100M
        #[arg(long = "min-size", value_name = "SIZE", value_parser = parse_size)]
        min_size: Option<u64>,
        /// Also Python virtual environments (.venv / venv): the project needs them recreated
        #[arg(long)]
        venv: bool,
    },
    /// Total size and file count of PATH.
    Size {
        path: PathBuf,
        /// Show the nested directory tree with sizes
        #[arg(long)]
        tree: bool,
        /// Hide directories smaller than SIZE in the tree, e.g. 100G, 1.5T, 500M
        #[arg(long = "min-size", value_name = "SIZE", requires = "tree", value_parser = parse_size)]
        min_size: Option<u64>,
    },
}

#[derive(Args, Clone, Copy, Default)]
struct TransferOpts {
    /// Keep files that already exist at the destination instead of overwriting them
    #[arg(short = 'n', long = "skip-existing")]
    skip_existing: bool,
}

/// Parses a human size such as `100G`, `1.5T`, `500MB`, `2GiB` or `1024` (bytes).
/// Units are binary (1K = 1024) to match how sizes are displayed.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num
        .parse()
        .map_err(|_| format!("invalid size '{}'", s))?;
    let exp = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 0,
        "K" | "KB" | "KIB" => 1,
        "M" | "MB" | "MIB" => 2,
        "G" | "GB" | "GIB" => 3,
        "T" | "TB" | "TIB" => 4,
        "P" | "PB" | "PIB" => 5,
        _ => return Err(format!("unknown size unit in '{}' (use B, K, M, G, T or P)", s)),
    };
    Ok((num * 1024_f64.powi(exp)) as u64)
}

/// A source ending in `/` (or `.`) means "the contents of src".
fn contents_only(src: &Path) -> bool {
    let raw = src.as_os_str().as_encoded_bytes();
    raw.ends_with(b"/") || raw.ends_with(b"/.") || raw == b"."
}

/// Resolves where `src` should land, following `cp -r` / `mv` semantics:
/// if `dest` is an existing directory, `src` goes inside it as `dest/<name of src>`,
/// merging with whatever is already there. A source ending in `/` (or `.`) means
/// "the contents of src", which are merged directly into `dest`.
fn resolve_dest(src: &Path, dest: &Path) -> PathBuf {
    match src.file_name() {
        Some(name) if dest.is_dir() && !contents_only(src) => dest.join(name),
        _ => dest.to_path_buf(),
    }
}

/// Maps a source entry to its target path under `dest`.
fn target_for(entry: &Path, src: &Path, dest: &Path) -> PathBuf {
    let relative = entry.strip_prefix(src).unwrap();
    if relative.as_os_str().is_empty() {
        // the entry is `src` itself
        dest.to_path_buf()
    } else {
        dest.join(relative)
    }
}

/// Absolute form of `p` without requiring it to exist: the nearest existing ancestor
/// canonicalized, the rest appended.
fn absolute(p: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = cur.canonicalize() {
            return missing.iter().rev().fold(c, |acc, part: &std::ffi::OsString| acc.join(part));
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name.to_os_string());
                cur = if parent.as_os_str().is_empty() { PathBuf::from(".") } else { parent.to_path_buf() };
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Everything below a path, from one walk. Symlinks are listed, never followed.
/// Entries that could not be read are in `errors`, so nothing is skipped silently.
#[derive(Default)]
struct Tree {
    /// Directories, parents before children.
    dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
    links: Vec<PathBuf>,
    errors: Vec<String>,
}

fn walk(src: &Path) -> Tree {
    let mut t = Tree::default();
    // follow_root_links(false): `src` itself may be a symlink, and is then the link (walkdir
    // follows a root symlink by default, which would delete or copy the linked folder)
    for e in WalkDir::new(src).follow_root_links(false) {
        match e {
            Ok(e) => {
                let ft = e.file_type();
                let p = e.into_path();
                if ft.is_dir() {
                    t.dirs.push(p);
                } else if ft.is_symlink() {
                    t.links.push(p);
                } else if ft.is_file() {
                    t.files.push(p);
                } else {
                    t.errors.push(format!("{}: special file (device, socket or pipe), left as is", p.display()));
                }
            }
            Err(e) => {
                let path = e.path().map_or_else(|| "?".into(), |p| p.display().to_string());
                let why = e.io_error().map_or_else(|| e.to_string(), |io| io.to_string());
                t.errors.push(format!("{path}: cannot read: {why}"));
            }
        }
    }
    t
}

/// Counts and errors of an operation, shared by the worker threads.
#[derive(Default)]
struct Report {
    done: AtomicU64,
    skipped: AtomicU64,
    errors: Mutex<Vec<String>>,
}

impl Report {
    fn with_errors(errors: Vec<String>) -> Self {
        Self { errors: Mutex::new(errors), ..Default::default() }
    }

    fn error(&self, msg: String) {
        self.errors.lock().unwrap().push(msg);
    }

    fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }

    /// Prints the outcome; the exit code is non-zero if anything failed.
    fn finish(&self, verb: &str) -> ExitCode {
        let errors = self.errors();
        let (done, skipped) = (self.done.load(Ordering::Relaxed), self.skipped.load(Ordering::Relaxed));
        for e in errors.iter().take(20) {
            eprintln!("  {e}");
        }
        if errors.len() > 20 {
            eprintln!("  ... and {} more", format_count(errors.len() as u64 - 20));
        }
        let skipped = if skipped > 0 { format!(", {} skipped (already there)", format_count(skipped)) } else { String::new() };
        if errors.is_empty() {
            println!("{verb} {}{skipped}.", format_count(done));
            ExitCode::SUCCESS
        } else {
            eprintln!("{verb} {}{skipped}; {} problem(s) above, so the result is incomplete.", format_count(done), format_count(errors.len() as u64));
            ExitCode::FAILURE
        }
    }
}

fn make_progress_bar(total: u64, msg: &'static str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} entries ({eta})",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    pb.set_message(msg);
    pb
}

fn times_of(meta: &fs::Metadata) -> io::Result<FileTimes> {
    Ok(FileTimes::new().set_accessed(meta.accessed()?).set_modified(meta.modified()?))
}

/// Opens a file only to change its timestamps (unix: the owner may do that on a
/// read-only file; Windows needs the write-attributes right).
fn open_for_times(p: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        fs::OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).open(p)
    }
    #[cfg(not(windows))]
    fs::File::open(p)
}

fn make_symlink(target: &Path, link: &Path, original: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = original;
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if fs::metadata(original).is_ok_and(|m| m.is_dir()) {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
}

/// Copies one file (contents, permissions, timestamps) or symlink (as a link).
/// Ok(false): skipped because `dst` exists and `skip_existing`.
fn copy_entry(src: &Path, dst: &Path, skip_existing: bool) -> io::Result<bool> {
    let meta = fs::symlink_metadata(src)?;
    let exists = fs::symlink_metadata(dst).is_ok();
    if exists && skip_existing {
        return Ok(false);
    }
    if meta.file_type().is_symlink() {
        if exists {
            fs::remove_file(dst)?;
        }
        make_symlink(&fs::read_link(src)?, dst, src)?;
        return Ok(true);
    }
    fs::copy(src, dst)?;
    open_for_times(dst)?.set_times(times_of(&meta)?)?;
    Ok(true)
}

/// Gives a created directory its source's permissions and (unix) timestamps. Run after
/// the directory's contents are in place: adding entries changes a directory's times.
fn copy_dir_meta(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    #[cfg(unix)]
    open_for_times(dst)?.set_times(times_of(&meta)?)?;
    fs::set_permissions(dst, meta.permissions())
}

/// Refuses to copy or move something onto itself, or a directory into itself.
fn check_not_inside(src: &Path, target: &Path) -> Result<(), String> {
    let (s, t) = (absolute(src), absolute(target));
    if s == t {
        return Err(format!("{} and {} are the same", src.display(), target.display()));
    }
    if fs::symlink_metadata(src).is_ok_and(|m| m.is_dir()) && t.starts_with(&s) {
        return Err(format!("cannot put {} inside itself ({})", src.display(), target.display()));
    }
    Ok(())
}

/// Creates the target directories (empty ones too) and the parent of a single-entry source.
fn make_dirs(tree: &Tree, src: &Path, dest: &Path, report: &Report) {
    if tree.dirs.is_empty()
        && let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = fs::create_dir_all(parent)
    {
        report.error(format!("{}: cannot create: {e}", parent.display()));
    }
    for d in &tree.dirs {
        let t = target_for(d, src, dest);
        if let Err(e) = fs::create_dir_all(&t) {
            report.error(format!("{}: cannot create: {e}", t.display()));
        }
    }
}

/// Directory metadata, deepest first (WalkDir lists parents before children).
fn finish_dirs(tree: &Tree, src: &Path, dest: &Path, report: &Report) {
    for d in tree.dirs.iter().rev() {
        let t = target_for(d, src, dest);
        if let Err(e) = copy_dir_meta(d, &t) {
            report.error(format!("{}: cannot set permissions / times: {e}", t.display()));
        }
    }
}

fn op_copy(src: &Path, dest: &Path, opts: TransferOpts) -> Report {
    let dest = &resolve_dest(src, dest);
    if let Err(e) = check_not_inside(src, dest) {
        return Report::with_errors(vec![e]);
    }
    let tree = walk(src);
    let report = Report::with_errors(tree.errors.clone());
    let entries: Vec<&PathBuf> = tree.files.iter().chain(&tree.links).collect();
    println!("Copying {} files and links, {} folders...", format_count(entries.len() as u64), format_count(tree.dirs.len() as u64));

    make_dirs(&tree, src, dest, &report);
    let pb = make_progress_bar(entries.len() as u64, "Copying");
    entries.par_iter().for_each(|entry| {
        let target = target_for(entry, src, dest);
        match copy_entry(entry, &target, opts.skip_existing) {
            Ok(true) => _ = report.done.fetch_add(1, Ordering::Relaxed),
            Ok(false) => _ = report.skipped.fetch_add(1, Ordering::Relaxed),
            Err(e) => report.error(format!("{} -> {}: {e}", entry.display(), target.display())),
        }
        pb.inc(1);
    });
    pb.finish_and_clear();
    finish_dirs(&tree, src, dest, &report);
    report
}

/// Moves one file or symlink: a rename, or copy + delete across filesystems.
/// Ok(false): skipped because `dst` exists and `skip_existing` (the source stays).
fn move_entry(src: &Path, dst: &Path, skip_existing: bool) -> io::Result<bool> {
    if skip_existing && fs::symlink_metadata(dst).is_ok() {
        return Ok(false);
    }
    if fs::rename(src, dst).is_ok() {
        return Ok(true);
    }
    // Another filesystem: copy to a temporary name next to `dst`, rename it into place once
    // complete, then remove the source. A failure leaves the source and no partial file.
    let mut tmp_name = dst.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".fastops-part");
    let tmp = dst.with_file_name(tmp_name);
    let _ = fs::remove_file(&tmp); // a leftover of an interrupted move
    if let Err(e) = copy_entry(src, &tmp, false).and_then(|_| fs::rename(&tmp, dst)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::remove_file(src)?;
    Ok(true)
}

fn op_move(src: &Path, dest: &Path, opts: TransferOpts) -> Report {
    let target = &resolve_dest(src, dest);
    if let Err(e) = check_not_inside(src, target) {
        return Report::with_errors(vec![e]);
    }
    // Nothing there yet: the whole tree in one rename (instant on the same filesystem).
    if !contents_only(src) && fs::symlink_metadata(target).is_err() {
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = fs::create_dir_all(parent);
        }
        if fs::rename(src, target).is_ok() {
            println!("Moved {} -> {} in one rename.", src.display(), target.display());
            let report = Report::default();
            report.done.store(1, Ordering::Relaxed);
            return report;
        }
    }

    let tree = walk(src);
    let report = Report::with_errors(tree.errors.clone());
    let entries: Vec<&PathBuf> = tree.files.iter().chain(&tree.links).collect();
    println!("Moving {} files and links, {} folders...", format_count(entries.len() as u64), format_count(tree.dirs.len() as u64));

    make_dirs(&tree, src, target, &report);
    let pb = make_progress_bar(entries.len() as u64, "Moving ");
    entries.par_iter().for_each(|entry| {
        let t = target_for(entry, src, target);
        match move_entry(entry, &t, opts.skip_existing) {
            Ok(true) => _ = report.done.fetch_add(1, Ordering::Relaxed),
            Ok(false) => _ = report.skipped.fetch_add(1, Ordering::Relaxed),
            Err(e) => report.error(format!("{} -> {}: {e}", entry.display(), t.display())),
        }
        pb.inc(1);
    });
    pb.finish_and_clear();
    finish_dirs(&tree, src, target, &report);

    // Remove the now-empty source directories, deepest first. Anything left behind
    // (a skipped file, an error) keeps its directory; that is not an error by itself.
    let keep_root = contents_only(src);
    for d in tree.dirs.iter().rev() {
        if keep_root && d == &tree.dirs[0] {
            continue;
        }
        let _ = fs::remove_dir(d);
    }
    report
}

// ---------------------------------------------------------------------------
// clean: build and cache folders that their tools recreate
// ---------------------------------------------------------------------------

/// A recognised build / cache folder. `sibling`: one of these must sit next to it, and
/// `inside`: one of these must be in it (empty = no condition), so that a folder that only
/// shares the name is never touched. `opt_in`: only with a flag.
struct Kind {
    label: &'static str,
    dir: &'static str,
    sibling: &'static [&'static str],
    inside: &'static [&'static str],
    opt_in: bool,
}

const GRADLE_FILES: &[&str] = &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"];

const KINDS: &[Kind] = &[
    Kind { label: "Node.js modules", dir: "node_modules", sibling: &["package.json"], inside: &[], opt_in: false },
    Kind { label: "Rust build", dir: "target", sibling: &["Cargo.toml"], inside: &["CACHEDIR.TAG", ".rustc_info.json"], opt_in: false },
    Kind { label: "Python bytecode", dir: "__pycache__", sibling: &[], inside: &[], opt_in: false },
    Kind { label: "pytest cache", dir: ".pytest_cache", sibling: &[], inside: &[], opt_in: false },
    Kind { label: "mypy cache", dir: ".mypy_cache", sibling: &[], inside: &[], opt_in: false },
    Kind { label: "Ruff cache", dir: ".ruff_cache", sibling: &[], inside: &[], opt_in: false },
    Kind { label: "Next.js build", dir: ".next", sibling: &["package.json"], inside: &[], opt_in: false },
    Kind { label: "Gradle cache", dir: ".gradle", sibling: GRADLE_FILES, inside: &[], opt_in: false },
    Kind { label: "Python venv", dir: ".venv", sibling: &[], inside: &["pyvenv.cfg"], opt_in: true },
    Kind { label: "Python venv", dir: "venv", sibling: &[], inside: &["pyvenv.cfg"], opt_in: true },
];

/// Finds the clean-able folders below `root` with a parallel walk; never descends into a
/// found folder or `.git`, never follows symlinks. -> (folder, label), unreadable entries.
fn scan_junk(root: &Path, venv: bool) -> (Vec<(PathBuf, &'static str)>, Vec<String>) {
    let found: Arc<Mutex<Vec<(PathBuf, &'static str)>>> = Arc::default();
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let (fd, er) = (found.clone(), errors.clone());
    jwalk::WalkDirGeneric::<((), ())>::new(root)
        .skip_hidden(false)
        .parallelism(jwalk::Parallelism::RayonNewPool(0))
        .process_read_dir(move |_, dir, _, children| {
            let names: Vec<std::ffi::OsString> = children.iter().flatten().map(|e| e.file_name.clone()).collect();
            let has = |n: &str| names.iter().any(|x| x == n);
            for entry in children.iter_mut() {
                let Ok(e) = entry else {
                    if let Err(err) = entry {
                        er.lock().unwrap().push(err.to_string());
                    }
                    continue;
                };
                if !e.file_type.is_dir() {
                    continue;
                }
                if e.file_name == ".git" {
                    e.read_children_path = None;
                    continue;
                }
                let path = dir.join(&e.file_name);
                let kind = KINDS.iter().find(|k| {
                    e.file_name == k.dir
                        && (venv || !k.opt_in)
                        && (k.sibling.is_empty() || k.sibling.iter().any(|f| has(f)))
                        && (k.inside.is_empty() || k.inside.iter().any(|f| fs::symlink_metadata(path.join(f)).is_ok()))
                });
                if let Some(k) = kind {
                    fd.lock().unwrap().push((path, k.label));
                    e.read_children_path = None; // all of it goes: no need to look inside
                }
            }
        })
        .into_iter()
        .for_each(|r| match r {
            Err(e) => errors.lock().unwrap().push(e.to_string()),
            Ok(e) => {
                if let Some(err) = &e.read_children_error {
                    errors.lock().unwrap().push(format!("{}: cannot read: {err}", e.path().display()));
                }
            }
        });
    let mut found = std::mem::take(&mut *found.lock().unwrap());
    found.sort();
    let errors = std::mem::take(&mut *errors.lock().unwrap());
    (found, errors)
}

/// A folder to clean, measured: its entries (for the delete) and size.
struct Junk {
    path: PathBuf,
    label: &'static str,
    tree: Tree,
    bytes: u64,
}

fn measure(found: Vec<(PathBuf, &'static str)>) -> Vec<Junk> {
    let mut junk: Vec<Junk> = found
        .into_par_iter()
        .map(|(path, label)| {
            let tree = walk(&path);
            let bytes = tree.files.iter().filter_map(|f| fs::symlink_metadata(f).ok()).map(|m| m.len()).sum();
            Junk { path, label, tree, bytes }
        })
        .collect();
    junk.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    junk
}

/// Deletes the folders (each one's files in parallel, as `delete`). -> (bytes freed by
/// the folders that are fully gone, report).
fn clean_delete(junk: &[Junk]) -> (u64, Report) {
    let report = Report::default();
    let mut freed = 0;
    for j in junk {
        let r = delete_tree(&j.tree);
        report.done.fetch_add(r.done.load(Ordering::Relaxed), Ordering::Relaxed);
        for e in r.errors() {
            report.error(e);
        }
        if fs::symlink_metadata(&j.path).is_err() {
            freed += j.bytes;
        }
    }
    (freed, report)
}

fn op_clean(root: &Path, dry_run: bool, yes: bool, min_size: u64, venv: bool) -> ExitCode {
    if absolute(root).parent().is_none() {
        eprintln!("Refusing to clean the filesystem root; pick a folder.");
        return ExitCode::from(2);
    }
    println!("Looking for build and cache folders in {} ...", root.display());
    let (found, scan_errors) = scan_junk(root, venv);
    let junk: Vec<Junk> = measure(found).into_iter().filter(|j| j.bytes >= min_size).collect();
    if !scan_errors.is_empty() {
        eprintln!("{} entries could not be read and were not searched:", format_count(scan_errors.len() as u64));
        for e in scan_errors.iter().take(5) {
            eprintln!("  {e}");
        }
    }
    if junk.is_empty() {
        println!("Nothing to clean.");
        return ExitCode::SUCCESS;
    }
    let total: u64 = junk.iter().map(|j| j.bytes).sum();
    println!("{} folders, {} in total:", format_count(junk.len() as u64), format_size(total));
    for j in &junk {
        let rel = j.path.strip_prefix(root).unwrap_or(&j.path);
        println!("  {:>10}  {:<16}  {}", format_size(j.bytes), j.label, rel.display());
    }
    if !venv && KINDS.iter().any(|k| k.opt_in) {
        println!("(Python virtual environments are left alone; add --venv to include them.)");
    }
    if dry_run {
        println!("Dry run: nothing deleted.");
        return ExitCode::SUCCESS;
    }
    if !yes {
        if !io::stdin().is_terminal() {
            eprintln!("not asking without a terminal: pass --yes to delete");
            return ExitCode::from(2);
        }
        print!("Delete these {} folders ({})? [y/N] ", format_count(junk.len() as u64), format_size(total));
        let _ = io::stdout().flush();
        let mut answer = String::new();
        if io::stdin().lock().read_line(&mut answer).is_err() || !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing deleted.");
            return ExitCode::from(2);
        }
    }
    let (freed, report) = clean_delete(&junk);
    println!("Freed {}.", format_size(freed));
    report.finish("Deleted files:")
}

/// Paths `delete` never touches: the filesystem root, the home directory and its parents.
fn protected(p: &Path) -> bool {
    let p = absolute(p);
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).map(|h| absolute(&h));
    p.parent().is_none() || home.is_some_and(|h| h.starts_with(&p))
}

/// `path` without trailing separators, so a symlink given as `link/` is the link itself
/// (the system would resolve `link/` to the folder it points to). None for `.` / `..`.
fn delete_target(path: &Path) -> Option<PathBuf> {
    let raw = path.as_os_str().as_encoded_bytes();
    let mut end = raw.len();
    while end > 1 && (raw[end - 1] == b'/' || (cfg!(windows) && raw[end - 1] == b'\\')) {
        end -= 1;
    }
    let trimmed = &raw[..end];
    let last = trimmed.rsplit(|&c| c == b'/' || (cfg!(windows) && c == b'\\')).next().unwrap_or(trimmed);
    if last == b"." || last == b".." {
        return None;
    }
    // SAFETY: cut at an ASCII separator of a valid OsStr encoding
    Some(PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(trimmed) }))
}

/// Shows what `delete` will remove and asks (Ok(false): the user said no).
fn confirm_delete(src: &Path, tree: &Tree) -> io::Result<bool> {
    println!(
        "About to delete {}: {} files, {} links, {} folders.",
        src.display(),
        format_count(tree.files.len() as u64),
        format_count(tree.links.len() as u64),
        format_count(tree.dirs.len() as u64)
    );
    if !tree.errors.is_empty() {
        println!("{} entries could not be read and will be left in place.", format_count(tree.errors.len() as u64));
    }
    if !io::stdin().is_terminal() {
        return Err(io::Error::other("not asking without a terminal: pass --yes to delete"));
    }
    print!("Delete? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn delete_tree(tree: &Tree) -> Report {
    let report = Report::with_errors(tree.errors.clone());
    // symlinks are removed as links: what they point to is never touched
    let entries: Vec<&PathBuf> = tree.files.iter().chain(&tree.links).collect();
    let pb = make_progress_bar(entries.len() as u64, "Deleting");
    entries.par_iter().for_each(|entry| {
        match fs::remove_file(entry) {
            Ok(()) => _ = report.done.fetch_add(1, Ordering::Relaxed),
            Err(e) => report.error(format!("{}: {e}", entry.display())),
        }
        pb.inc(1);
    });
    pb.finish_and_clear();
    for d in tree.dirs.iter().rev() {
        if let Err(e) = fs::remove_dir(d) {
            report.error(format!("{}: folder not removed: {e}", d.display()));
        }
    }
    report
}

/// None: refused or cancelled (the reason is printed).
fn op_delete(src: &Path, yes: bool) -> Option<Report> {
    let Some(src) = delete_target(src) else {
        eprintln!("Refusing to delete '.' or '..': name the folder instead.");
        return None;
    };
    let src = src.as_path();
    if protected(src) {
        eprintln!("Refusing to delete {}: it is the filesystem root or your home folder.", src.display());
        return None;
    }
    let tree = walk(src);
    if !yes {
        match confirm_delete(src, &tree) {
            Ok(true) => {}
            Ok(false) => {
                println!("Nothing deleted.");
                return None;
            }
            Err(e) => {
                eprintln!("{e}");
                return None;
            }
        }
    }
    Some(delete_tree(&tree))
}

fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let exp = (bytes as f64).log(1024.0).floor() as usize;
    let exp = exp.min(UNITS.len() - 1);
    if exp == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.2} {}", bytes as f64 / 1024_f64.powi(exp as i32), UNITS[exp])
    }
}

/// A file's size for the totals: a file with several hard links counts once (its first
/// name seen), as `du` and `dust` count disk usage.
fn counted_len(m: &fs::Metadata, seen: &DashMap<(u64, u64), ()>) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.nlink() > 1 && seen.insert((m.dev(), m.ino()), ()).is_some() {
            return 0;
        }
    }
    #[cfg(not(unix))]
    let _ = seen;
    m.len()
}

/// The totals leave out entries that could not be read; say so instead of under-counting silently.
fn warn_unreadable(n: u64) {
    if n > 0 {
        eprintln!("Warning: {} entries could not be read (permissions?); the totals below leave them out.", format_count(n));
    }
}

fn op_check_size(path: &Path, tree: bool, min_size: u64) {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message("Scanning...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));

    if path.is_file() {
        match fs::metadata(path) {
            Ok(meta) => {
                spinner.finish_and_clear();
                println!("{}: {}", path.display(), format_size(meta.len()));
            }
            Err(e) => {
                spinner.finish_and_clear();
                eprintln!("Error reading {}: {}", path.display(), e);
            }
        }
        return;
    }

    if !tree {
        // Fast path: process_read_dir runs inside jwalk's worker threads.
        // Each thread accumulates directly into shared atomics — no bridge mutex,
        // no per-file allocations.
        let total_bytes = Arc::new(AtomicU64::new(0));
        let file_count = Arc::new(AtomicU64::new(0));
        let sp = spinner.clone();
        let tb = total_bytes.clone();
        let fc = file_count.clone();
        let unreadable = Arc::new(AtomicU64::new(0));
        let ur = unreadable.clone();
        let sl: Arc<DashMap<(u64, u64), ()>> = Arc::new(DashMap::new());

        jwalk::WalkDirGeneric::<((), ())>::new(path)
            .skip_hidden(false) // count dot-files and dot-folders too, like du
            .parallelism(jwalk::Parallelism::RayonNewPool(0))
            .process_read_dir(move |_, _, _, children| {
                for entry in children.iter() {
                    if entry.is_err() {
                        ur.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Ok(e) = entry {
                        if e.file_type().is_file() {
                            let size = e.metadata().map_or(0, |m| counted_len(&m, &sl));
                            tb.fetch_add(size, Ordering::Relaxed);
                            let count = fc.fetch_add(1, Ordering::Relaxed) + 1;
                            if count % 5_000 == 0 {
                                sp.set_message(format!(
                                    "Scanning... {} files, {}",
                                    format_count(count),
                                    format_size(tb.load(Ordering::Relaxed))
                                ));
                            }
                        }
                    }
                }
                // Drop file entries from the iterator queue — all data already captured above.
                // Reduces iterator steps from O(files) to O(dirs).
                children.retain(|e| e.as_ref().map(|e| e.file_type().is_dir()).unwrap_or(false));
            })
            .into_iter()
            .for_each(|r| {
                // a folder that could not be listed comes back with read_children_error set
                if r.as_ref().map_or(true, |e| e.read_children_error.is_some()) {
                    unreadable.fetch_add(1, Ordering::Relaxed);
                }
            });

        spinner.finish_and_clear();
        warn_unreadable(unreadable.load(Ordering::Relaxed));
        let bytes = total_bytes.load(Ordering::Relaxed);
        let count = file_count.load(Ordering::Relaxed);
        println!(
            "{}: {} ({} file{})",
            path.display(),
            format_size(bytes),
            format_count(count),
            if count == 1 { "" } else { "s" }
        );
        return;
    }

    // Tree path: process_read_dir computes each directory's own-files size inside
    // jwalk's worker threads, storing results in a DashMap (64-shard concurrent map).
    //
    // Key difference from the old par_bridge approach:
    //   Old: 1 PathBuf alloc per FILE  + bridge mutex serializing all entries
    //   New: 1 DashMap insert per DIR  + zero bridge overhead
    //
    // For 57M files across 500K dirs that's ~100x fewer allocations and no bottleneck.
    let dir_own: Arc<DashMap<PathBuf, (u64, u64)>> = Arc::new(DashMap::new());
    let scan_count = Arc::new(AtomicU64::new(0));
    let scan_bytes = Arc::new(AtomicU64::new(0));
    let sp = spinner.clone();
    let dm = dir_own.clone();
    let sc = scan_count.clone();
    let sb = scan_bytes.clone();
    let unreadable = Arc::new(AtomicU64::new(0));
    let ur = unreadable.clone();
    let sl: Arc<DashMap<(u64, u64), ()>> = Arc::new(DashMap::new());

    jwalk::WalkDirGeneric::<((), ())>::new(path)
            .skip_hidden(false) // count dot-files and dot-folders too, like du
        .parallelism(jwalk::Parallelism::RayonNewPool(0))
        .process_read_dir(move |_, dir_path, _, children| {
            let mut own_size = 0u64;
            let mut own_count = 0u64;

            for entry in children.iter() {
                if entry.is_err() {
                    ur.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(e) = entry {
                    if e.file_type().is_file() {
                        let size = e.metadata().map_or(0, |m| counted_len(&m, &sl));
                        own_size += size;
                        own_count += 1;
                        sb.fetch_add(size, Ordering::Relaxed);
                        let count = sc.fetch_add(1, Ordering::Relaxed) + 1;
                        if count % 5_000 == 0 {
                            sp.set_message(format!(
                                "Scanning... {} files, {}",
                                format_count(count),
                                format_size(sb.load(Ordering::Relaxed))
                            ));
                        }
                    }
                }
            }

            // One insert per directory regardless of how many files it contains.
            dm.insert(dir_path.to_path_buf(), (own_size, own_count));

            // Drop file entries before they reach the iterator — data already captured.
            // Reduces iterator steps from O(files) to O(dirs).
            children.retain(|e| e.as_ref().map(|e| e.file_type().is_dir()).unwrap_or(false));
        })
        .into_iter()
        .for_each(|r| {
            // a folder that could not be listed comes back with read_children_error set
            if r.as_ref().map_or(true, |e| e.read_children_error.is_some()) {
                unreadable.fetch_add(1, Ordering::Relaxed);
            }
        });

    spinner.finish_and_clear();
    warn_unreadable(unreadable.load(Ordering::Relaxed));

    // Drain DashMap into a plain HashMap for the sequential aggregation pass.
    let mut dir_totals: HashMap<PathBuf, (u64, u64)> =
        dir_own.iter().map(|r| (r.key().clone(), *r.value())).collect();

    // Bottom-up aggregation: sort deepest dirs first so each dir's full subtree
    // total is ready before we add it to the parent.
    let mut dirs: Vec<PathBuf> = dir_totals.keys().cloned().collect();
    dirs.sort_by_cached_key(|p| std::cmp::Reverse(p.components().count()));

    for dir in &dirs {
        let (size, count) = *dir_totals.get(dir).unwrap_or(&(0, 0));
        if let Some(parent) = dir.parent() {
            if let Some(p) = dir_totals.get_mut(parent) {
                p.0 += size;
                p.1 += count;
            }
        }
    }

    // Build parent -> sorted children index for O(1) lookups during printing.
    let mut children_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for dir in dir_totals.keys() {
        if dir != path {
            if let Some(parent) = dir.parent() {
                children_map
                    .entry(parent.to_path_buf())
                    .or_default()
                    .push(dir.clone());
            }
        }
    }
    for children in children_map.values_mut() {
        children.sort();
    }

    let (root_size, root_count) = dir_totals.get(path).copied().unwrap_or((0, 0));
    println!(
        "{} ({}, {} files)",
        path.display(),
        format_size(root_size),
        format_count(root_count)
    );
    print_tree(path, &dir_totals, &children_map, "", min_size);
}

/// Recursively prints the directory subtree rooted at `dir`.
/// `prefix` carries the continuation characters (`│   ` / `    `) for each depth level.
fn print_tree(
    dir: &Path,
    totals: &HashMap<PathBuf, (u64, u64)>,
    children_map: &HashMap<PathBuf, Vec<PathBuf>>,
    prefix: &str,
    min_size: u64,
) {
    // A directory's total always includes its subdirectories, so hiding a small
    // directory never hides a larger one beneath it.
    let children: Vec<&PathBuf> = children_map
        .get(dir)
        .map(|c| {
            c.iter()
                .filter(|child| totals.get(*child).map_or(0, |t| t.0) >= min_size)
                .collect()
        })
        .unwrap_or_default();
    let last = children.len().saturating_sub(1);

    for (i, child) in children.iter().enumerate() {
        let is_last = i == last;
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });

        let (size, count) = totals.get(*child).copied().unwrap_or((0, 0));
        let name = child.file_name().unwrap_or_default().to_string_lossy();
        println!(
            "{}{}{} ({}, {} files)",
            prefix, connector, name, format_size(size), format_count(count)
        );

        print_tree(child, totals, children_map, &child_prefix, min_size);
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cmd = match cli.cmd {
        Some(cmd) => cmd,
        None => {
            let opts = TransferOpts { skip_existing: cli.skip_existing };
            match (cli.copy, cli.move_op, cli.delete, cli.check_size) {
                (Some(p), None, None, None) => Cmd::Copy { src: p[0].clone(), dest: p[1].clone(), opts },
                (None, Some(p), None, None) => Cmd::Move { src: p[0].clone(), dest: p[1].clone(), opts },
                (None, None, Some(src), None) => Cmd::Delete { src, yes: cli.yes },
                (None, None, None, Some(path)) => Cmd::Size { path, tree: cli.tree, min_size: cli.min_size },
                _ => {
                    eprintln!("Provide one operation:");
                    eprintln!("  fastops copy SRC DEST [--skip-existing]");
                    eprintln!("  fastops move SRC DEST [--skip-existing]");
                    eprintln!("  fastops delete SRC [--yes]");
                    eprintln!("  fastops clean [PATH] [--dry-run] [--yes] [--min-size 100M] [--venv]");
                    eprintln!("  fastops size PATH [--tree [--min-size 100G]]");
                    return ExitCode::from(2);
                }
            }
        }
    };
    let exists = |p: &Path| {
        let ok = fs::symlink_metadata(p).is_ok();
        if !ok {
            eprintln!("Does not exist: {}", p.display());
        }
        ok
    };
    match cmd {
        Cmd::Copy { src, dest, opts } => {
            if !exists(&src) {
                return ExitCode::FAILURE;
            }
            op_copy(&src, &dest, opts).finish("Copied")
        }
        Cmd::Move { src, dest, opts } => {
            if !exists(&src) {
                return ExitCode::FAILURE;
            }
            op_move(&src, &dest, opts).finish("Moved")
        }
        Cmd::Delete { src, yes } => {
            if !exists(&src) {
                return ExitCode::FAILURE;
            }
            match op_delete(&src, yes) {
                Some(report) => report.finish("Deleted"),
                None => ExitCode::from(2),
            }
        }
        Cmd::Clean { path, dry_run, yes, min_size, venv } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            if !exists(&root) {
                return ExitCode::FAILURE;
            }
            op_clean(&root, dry_run, yes, min_size.unwrap_or(0), venv)
        }
        Cmd::Size { path, tree, min_size } => {
            if !exists(&path) {
                return ExitCode::FAILURE;
            }
            op_check_size(&path, tree, min_size.unwrap_or(0));
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn errors(r: &Report) -> Vec<String> {
        r.errors()
    }

    /// src/{a.txt, sub/b.txt, empty/, link -> sub/b.txt, outside -> <other dir>}
    fn sample(root: &Path, outside: &Path) -> PathBuf {
        let src = root.join("src");
        write(&src.join("a.txt"), "A");
        write(&src.join("sub/b.txt"), "B");
        fs::create_dir_all(src.join("empty")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("sub/b.txt", src.join("link")).unwrap();
            std::os::unix::fs::symlink(outside, src.join("outside")).unwrap();
        }
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::options().write(true).open(src.join("a.txt")).unwrap().set_times(FileTimes::new().set_modified(old)).unwrap();
        src
    }

    #[test]
    fn copy_keeps_empty_dirs_links_and_times() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        write(&outside.join("keep.txt"), "K");
        let src = sample(d.path(), &outside);
        let dest = d.path().join("dest");
        let r = op_copy(&src, &dest, TransferOpts::default());
        assert!(errors(&r).is_empty(), "{:?}", errors(&r));
        assert_eq!(fs::read_to_string(dest.join("sub/b.txt")).unwrap(), "B");
        assert!(dest.join("empty").is_dir(), "empty folder recreated");
        let m = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
        assert_eq!(m(&dest.join("a.txt")), m(&src.join("a.txt")), "timestamp kept");
        #[cfg(unix)]
        {
            assert!(fs::symlink_metadata(dest.join("link")).unwrap().file_type().is_symlink());
            assert_eq!(fs::read_link(dest.join("link")).unwrap(), PathBuf::from("sub/b.txt"));
            assert!(fs::symlink_metadata(dest.join("outside")).unwrap().file_type().is_symlink(), "not followed");
        }
    }

    #[test]
    fn skip_existing_keeps_the_destination() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("a.txt"), "new");
        write(&src.join("b.txt"), "new");
        let dest = d.path().join("dest");
        write(&dest.join("src/a.txt"), "old");
        fs::create_dir_all(&dest).unwrap();
        let r = op_copy(&src, &dest, TransferOpts { skip_existing: true });
        assert_eq!(fs::read_to_string(dest.join("src/a.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("src/b.txt")).unwrap(), "new");
        assert_eq!((r.done.load(Ordering::Relaxed), r.skipped.load(Ordering::Relaxed)), (1, 1));
        // without the option it overwrites, like cp
        op_copy(&src, &dest, TransferOpts::default());
        assert_eq!(fs::read_to_string(dest.join("src/a.txt")).unwrap(), "new");
    }

    #[test]
    fn refuses_to_copy_or_move_into_itself() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("a.txt"), "A");
        assert_eq!(errors(&op_copy(&src, &src.join("inner"), TransferOpts::default())).len(), 1);
        assert_eq!(errors(&op_move(&src, &src.join("inner"), TransferOpts::default())).len(), 1);
        assert!(src.join("a.txt").exists() && !src.join("inner").exists());
    }

    #[test]
    fn move_is_one_rename_when_the_destination_is_new() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let src = sample(d.path(), &outside);
        let dest = d.path().join("new/place");
        let r = op_move(&src, &dest, TransferOpts::default());
        assert!(errors(&r).is_empty());
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dest.join("sub/b.txt")).unwrap(), "B");
        assert!(dest.join("empty").is_dir());
    }

    #[test]
    fn move_merges_and_leaves_skipped_files_behind() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("a.txt"), "new");
        write(&src.join("sub/b.txt"), "B");
        let dest = d.path().join("dest");
        write(&dest.join("src/a.txt"), "old");
        let r = op_move(&src, &dest, TransferOpts { skip_existing: true });
        assert!(errors(&r).is_empty());
        assert_eq!(fs::read_to_string(dest.join("src/a.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("src/sub/b.txt")).unwrap(), "B");
        assert!(src.join("a.txt").exists(), "the skipped file stays at the source");
        assert!(!src.join("sub").exists(), "emptied folders are removed");
    }

    #[test]
    fn move_contents_keeps_the_source_folder() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("sub/b.txt"), "B");
        let dest = d.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let mut with_slash = src.clone().into_os_string();
        with_slash.push("/");
        let r = op_move(Path::new(&with_slash), &dest, TransferOpts::default());
        assert!(errors(&r).is_empty());
        assert_eq!(fs::read_to_string(dest.join("sub/b.txt")).unwrap(), "B");
        assert!(src.is_dir() && fs::read_dir(&src).unwrap().next().is_none());
    }

    #[test]
    fn delete_removes_links_not_their_targets() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        write(&outside.join("keep.txt"), "K");
        let src = sample(d.path(), &outside);
        let r = delete_tree(&walk(&src));
        assert!(errors(&r).is_empty(), "{:?}", errors(&r));
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(outside.join("keep.txt")).unwrap(), "K");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_folders_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("a.txt"), "A");
        write(&src.join("locked/b.txt"), "B");
        fs::set_permissions(src.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(src.join("locked")).is_ok() {
            return; // running as root: permissions do not lock anything
        }
        let copy = op_copy(&src, &d.path().join("dest"), TransferOpts::default());
        assert!(errors(&copy).iter().any(|e| e.contains("locked") && e.contains("cannot read")), "{:?}", errors(&copy));
        let del = delete_tree(&walk(&src));
        assert!(!errors(&del).is_empty(), "delete must not report success");
        assert!(src.join("locked").exists());
        fs::set_permissions(src.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    // ------------------------------------------------------------------ clean

    /// web/{package.json, node_modules/{a.js, pkg/node_modules/b.js, link -> outside}},
    /// loose/node_modules (no package.json), rs/{Cargo.toml, target/{CACHEDIR.TAG, debug/x}},
    /// fake/target (no Cargo.toml), py/__pycache__/m.pyc, app/.venv/pyvenv.cfg,
    /// repo/.git/node_modules (inside .git: never searched)
    fn projects(root: &Path, outside: &Path) {
        write(&root.join("web/package.json"), "{}");
        write(&root.join("web/node_modules/a.js"), "a");
        write(&root.join("web/node_modules/pkg/package.json"), "{}");
        write(&root.join("web/node_modules/pkg/node_modules/b.js"), "b");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside, root.join("web/node_modules/link")).unwrap();
        write(&root.join("loose/node_modules/c.js"), "c");
        write(&root.join("rs/Cargo.toml"), "[package]");
        write(&root.join("rs/target/CACHEDIR.TAG"), "Signature");
        write(&root.join("rs/target/debug/x"), "xxxx");
        write(&root.join("fake/target/keep.txt"), "k");
        write(&root.join("py/__pycache__/m.pyc"), "pyc");
        write(&root.join("app/.venv/pyvenv.cfg"), "home = /usr");
        write(&root.join("repo/package.json"), "{}");
        write(&root.join("repo/.git/node_modules/d.js"), "d");
    }

    fn rels(root: &Path, found: &[(PathBuf, &str)]) -> Vec<String> {
        // with `/` on every system, to compare against the expected names
        found.iter().map(|(p, _)| p.strip_prefix(root).unwrap().iter().map(|c| c.to_string_lossy()).collect::<Vec<_>>().join("/")).collect()
    }

    #[test]
    fn clean_finds_only_real_build_folders() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        write(&outside.join("keep.txt"), "K");
        let root = d.path().join("projects");
        projects(&root, &outside);
        let (found, errors) = scan_junk(&root, false);
        assert!(errors.is_empty(), "{errors:?}");
        // nested node_modules are part of the outer one; loose/fake/.git ones are not build folders
        assert_eq!(rels(&root, &found), vec!["py/__pycache__", "rs/target", "web/node_modules"]);
        let (with_venv, _) = scan_junk(&root, true);
        assert!(rels(&root, &with_venv).contains(&"app/.venv".to_string()));
    }

    #[test]
    fn clean_deletes_them_and_nothing_else() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        write(&outside.join("keep.txt"), "K");
        let root = d.path().join("projects");
        projects(&root, &outside);
        let junk = measure(scan_junk(&root, false).0);
        assert_eq!(junk[0].path, root.join("rs/target"), "largest first");
        let (freed, report) = clean_delete(&junk);
        assert!(report.errors().is_empty(), "{:?}", report.errors());
        assert_eq!(freed, junk.iter().map(|j| j.bytes).sum::<u64>());
        for gone in ["web/node_modules", "rs/target", "py/__pycache__"] {
            assert!(!root.join(gone).exists(), "{gone} left");
        }
        for kept in ["web/package.json", "rs/Cargo.toml", "loose/node_modules/c.js", "fake/target/keep.txt", "app/.venv/pyvenv.cfg", "repo/.git/node_modules/d.js"] {
            assert!(root.join(kept).exists(), "{kept} removed");
        }
        assert_eq!(fs::read_to_string(outside.join("keep.txt")).unwrap(), "K", "link target untouched");
    }

    #[test]
    fn clean_min_size_and_symlinked_folders() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("p");
        write(&root.join("a/package.json"), "{}");
        write(&root.join("a/node_modules/big.bin"), &"x".repeat(5000));
        write(&root.join("b/__pycache__/s.pyc"), "s");
        let junk: Vec<Junk> = measure(scan_junk(&root, false).0).into_iter().filter(|j| j.bytes >= 1000).collect();
        assert_eq!(junk.len(), 1);
        assert_eq!(junk[0].path, root.join("a/node_modules"));
        // a node_modules that is a symlink (pnpm-style) is a link, not a folder: left alone
        #[cfg(unix)]
        {
            let real = d.path().join("store");
            write(&real.join("pkg.js"), "p");
            write(&root.join("c/package.json"), "{}");
            std::os::unix::fs::symlink(&real, root.join("c/node_modules")).unwrap();
            let (found, _) = scan_junk(&root, false);
            assert!(!rels(&root, &found).contains(&"c/node_modules".to_string()));
        }
    }

    // ------------------------------------------------------------------ edge cases

    #[test]
    fn single_files() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        write(&f, "A");
        let dir = d.path().join("dir");
        fs::create_dir(&dir).unwrap();
        // into an existing folder, to a new name, and to a new name in missing folders
        for (dest, want) in [(dir.clone(), dir.join("a.txt")), (d.path().join("b.txt"), d.path().join("b.txt")), (d.path().join("x/y/c.txt"), d.path().join("x/y/c.txt"))] {
            let r = op_copy(&f, &dest, TransferOpts::default());
            assert!(r.errors().is_empty(), "{:?}", r.errors());
            assert_eq!(fs::read_to_string(&want).unwrap(), "A");
        }
        // onto itself: refused, the file is not emptied
        assert_eq!(op_copy(&f, &f, TransferOpts::default()).errors().len(), 1);
        assert_eq!(op_move(&f, &f, TransferOpts::default()).errors().len(), 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "A");
        let r = op_move(&f, &d.path().join("moved.txt"), TransferOpts::default());
        assert!(r.errors().is_empty() && !f.exists());
    }

    #[test]
    fn odd_names_and_empty_folders() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        let mut names = vec!["with space.txt", "ünïcödé ✓.txt", "-dash.txt", ".hidden"];
        if cfg!(unix) {
            names.push("new\nline.txt"); // not allowed in Windows file names
        }
        for name in &names {
            write(&src.join(name), name);
        }
        fs::create_dir_all(src.join("empty/nested")).unwrap();
        let dest = d.path().join("dest");
        let r = op_copy(&src, &dest, TransferOpts::default());
        assert!(r.errors().is_empty(), "{:?}", r.errors());
        for name in &names {
            assert_eq!(fs::read_to_string(dest.join(name)).unwrap(), *name);
        }
        assert!(dest.join("empty/nested").is_dir());
        let empty = d.path().join("only_empty");
        fs::create_dir(&empty).unwrap();
        let r = op_copy(&empty, &d.path().join("copy_of_empty"), TransferOpts::default());
        assert!(r.errors().is_empty() && d.path().join("copy_of_empty").is_dir());
        let del = delete_tree(&walk(&dest));
        assert!(del.errors().is_empty() && !dest.exists());
    }

    #[test]
    fn folder_onto_a_file_is_an_error_not_a_crash() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("a.txt"), "A");
        let dest = d.path().join("dest");
        write(&dest, "I am a file");
        // `dest` exists but is not a folder: src would have to become dest itself
        let r = op_copy(&src, &dest, TransferOpts::default());
        assert!(!r.errors().is_empty());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "I am a file");
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_reported_not_copied() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("a.txt"), "A");
        let ok = std::process::Command::new("mkfifo").arg(src.join("pipe")).status().is_ok_and(|s| s.success());
        if !ok {
            return;
        }
        let r = op_copy(&src, &d.path().join("dest"), TransferOpts::default());
        assert!(r.errors().iter().any(|e| e.contains("special file")), "{:?}", r.errors());
        assert!(d.path().join("dest/a.txt").exists() && !d.path().join("dest/pipe").exists());
    }

    #[cfg(unix)]
    #[test]
    fn delete_of_link_with_trailing_slash_removes_only_the_link() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        write(&real.join("precious.txt"), "P");
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut slashed = link.clone().into_os_string();
        slashed.push("/");
        let target = delete_target(Path::new(&slashed)).unwrap();
        assert_eq!(target, link);
        let r = delete_tree(&walk(&target));
        assert!(r.errors().is_empty());
        assert!(fs::symlink_metadata(&link).is_err(), "link removed");
        assert_eq!(fs::read_to_string(real.join("precious.txt")).unwrap(), "P", "its folder untouched");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_given_as_the_source_is_the_link() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        write(&real.join("precious.txt"), "P");
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // copy: the link is copied as a link, not the folder behind it
        let r = op_copy(&link, &d.path().join("copied"), TransferOpts::default());
        assert!(r.errors().is_empty(), "{:?}", r.errors());
        assert!(fs::symlink_metadata(d.path().join("copied")).unwrap().file_type().is_symlink());
        // delete by name (no slash): only the link goes
        let r = delete_tree(&walk(&delete_target(&link).unwrap()));
        assert!(r.errors().is_empty());
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read_to_string(real.join("precious.txt")).unwrap(), "P");
    }

    /// A temp folder on another filesystem than tempfile's default (/tmp), for the moves
    /// that cannot rename: the crate's target folder, if it is on another device.
    #[cfg(unix)]
    fn other_disk() -> Option<(tempfile::TempDir, tempfile::TempDir)> {
        use std::os::unix::fs::MetadataExt;
        let a = tempfile::tempdir().ok()?;
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        let b = tempfile::tempdir_in(&target).ok()?;
        let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
        if dev(a.path()) == dev(b.path()) {
            eprintln!("skipped: /tmp and {} are on the same filesystem", target.display());
            return None;
        }
        Some((a, b))
    }

    #[cfg(unix)]
    #[test]
    fn move_between_disks() {
        let Some((a, b)) = other_disk() else { return };
        let outside = a.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let src = sample(a.path(), &outside); // files, nested, empty folder, links, old timestamp
        let old = fs::metadata(src.join("a.txt")).unwrap().modified().unwrap();
        let dest = b.path().join("moved");
        let r = op_move(&src, &dest, TransferOpts::default());
        assert!(r.errors().is_empty(), "{:?}", r.errors());
        assert!(!src.exists(), "source removed");
        assert_eq!(fs::read_to_string(dest.join("sub/b.txt")).unwrap(), "B");
        assert!(dest.join("empty").is_dir());
        assert_eq!(fs::read_link(dest.join("link")).unwrap(), PathBuf::from("sub/b.txt"));
        assert_eq!(fs::metadata(dest.join("a.txt")).unwrap().modified().unwrap(), old, "timestamp kept");
        assert!(walk(&dest).files.iter().all(|f| !f.to_string_lossy().ends_with(".fastops-part")));

        // a single file, and a merge that keeps an existing file
        let f = a.path().join("one.txt");
        write(&f, "1");
        assert!(op_move(&f, &b.path().join("one.txt"), TransferOpts::default()).errors().is_empty());
        assert!(!f.exists() && b.path().join("one.txt").exists());
        let src2 = a.path().join("src2");
        write(&src2.join("x.txt"), "new");
        write(&src2.join("y.txt"), "y");
        write(&dest.join("src2/x.txt"), "old");
        let r = op_move(&src2, &dest, TransferOpts { skip_existing: true });
        assert!(r.errors().is_empty());
        assert_eq!(fs::read_to_string(dest.join("src2/x.txt")).unwrap(), "old");
        assert!(src2.join("x.txt").exists() && !src2.join("y.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_move_between_disks_keeps_the_source() {
        use std::os::unix::fs::PermissionsExt;
        let Some((a, b)) = other_disk() else { return };
        let src = a.path().join("src");
        write(&src.join("ok.txt"), "ok");
        write(&src.join("locked.txt"), "L");
        fs::set_permissions(src.join("locked.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(src.join("locked.txt")).is_ok() {
            return; // running as root
        }
        let dest = b.path().join("dest");
        let r = op_move(&src, &dest, TransferOpts::default());
        assert_eq!(r.errors().len(), 1, "{:?}", r.errors());
        assert!(src.join("locked.txt").exists(), "the file that failed stays at the source");
        assert!(!dest.join("locked.txt").exists() && !dest.join("locked.txt.fastops-part").exists(), "no partial file");
        assert!(dest.join("ok.txt").exists() && !src.join("ok.txt").exists());
        assert!(src.exists(), "its folder stays too");
        fs::set_permissions(src.join("locked.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn dot_paths_are_refused() {
        for p in [".", "..", "./", "a/..", "a/.", "a/../"] {
            assert_eq!(delete_target(Path::new(p)), None, "{p}");
        }
        assert_eq!(delete_target(Path::new("a/b/")), Some(PathBuf::from("a/b")));
        assert_eq!(delete_target(Path::new("/")), Some(PathBuf::from("/")));
    }

    #[test]
    fn home_parents_are_protected() {
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            if let Some(parent) = home.parent() {
                assert!(protected(parent), "{}", parent.display());
            }
            assert!(protected(&home.join("..")));
        }
    }

    #[test]
    fn root_and_home_are_protected() {
        assert!(protected(Path::new("/")));
        if let Some(home) = std::env::var_os("HOME") {
            assert!(protected(Path::new(&home)));
            assert!(!protected(&Path::new(&home).join("some/project")));
        }
    }
}
