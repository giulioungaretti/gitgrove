//! gitgrove — recursively discover git repositories and present their
//! worktrees and branches as a friendly command-line tree.
//!
//! Concurrency story: directory discovery is a single `walkdir` pass, then
//! every unique repository is interrogated in parallel on `rayon`'s
//! work-stealing pool. The pipeline is two-phase so each repo is queried once:
//!
//!   1. discover `.git` candidates, resolve each to its shared git dir, dedup;
//!   2. for every unique repo, gather worktrees and branches concurrently.

#![warn(clippy::pedantic, clippy::nursery)]
#![allow(clippy::must_use_candidate)]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use rayon::prelude::*;
use walkdir::WalkDir;

/// Directories we never descend into while searching for repositories.
const PRUNE_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "bin",
    "obj",
    ".venv",
    "venv",
    "Pods",
    "DerivedData",
];

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// A git invocation that failed, retaining enough context to be diagnosable.
#[derive(Debug)]
enum GitError {
    Spawn { dir: PathBuf, source: io::Error },
    Failed { dir: PathBuf, args: String, code: Option<i32>, stderr: String },
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { dir, source } => {
                write!(f, "failed to run git in {}: {source}", dir.display())
            }
            Self::Failed { dir, args, code, stderr } => {
                let code = code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
                write!(
                    f,
                    "`git {args}` exited with {code} in {}{}",
                    dir.display(),
                    if stderr.is_empty() { String::new() } else { format!(": {stderr}") },
                )
            }
        }
    }
}

impl Error for GitError {}

/// Print a warning to stderr without aborting the scan.
fn warn(err: &GitError) {
    eprintln!("warning: {err}");
}

// ---------------------------------------------------------------------------
// ANSI colouring (no external dependency; auto-disabled when not a TTY).
// ---------------------------------------------------------------------------

struct Style {
    on: bool,
}

impl Style {
    fn new() -> Self {
        let on = io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
        Self { on }
    }

    /// Wrap `text` in an SGR sequence, borrowing it untouched when colour is off.
    fn paint<'a>(&self, code: &str, text: &'a str) -> Cow<'a, str> {
        if self.on {
            Cow::Owned(format!("\x1b[{code}m{text}\x1b[0m"))
        } else {
            Cow::Borrowed(text)
        }
    }

    fn bold<'a>(&self, t: &'a str) -> Cow<'a, str> {
        self.paint("1", t)
    }
    fn dim<'a>(&self, t: &'a str) -> Cow<'a, str> {
        self.paint("2", t)
    }
    fn green<'a>(&self, t: &'a str) -> Cow<'a, str> {
        self.paint("32", t)
    }
    fn yellow<'a>(&self, t: &'a str) -> Cow<'a, str> {
        self.paint("33", t)
    }
    fn magenta<'a>(&self, t: &'a str) -> Cow<'a, str> {
        self.paint("35", t)
    }
    fn red<'a>(&self, t: &'a str) -> Cow<'a, str> {
        self.paint("31", t)
    }
}

// ---------------------------------------------------------------------------
// Data model.
// ---------------------------------------------------------------------------

struct Worktree {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    detached: bool,
    bare: bool,
    locked: bool,
}

struct Repo {
    /// The main worktree path, used as the display root.
    root: PathBuf,
    remotes: Vec<(String, String)>,
    worktrees: Vec<Worktree>,
}

// ---------------------------------------------------------------------------
// Git helpers.
// ---------------------------------------------------------------------------

/// Run `git` inside `dir`, returning stdout on success or a rich [`GitError`].
fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|source| GitError::Spawn { dir: dir.to_path_buf(), source })?;

    if !out.status.success() {
        return Err(GitError::Failed {
            dir: dir.to_path_buf(),
            args: args.join(" "),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve the shared git directory, used as the identity of a repository so
/// linked worktrees collapse back onto the repo that owns them.
fn git_common_dir(dir: &Path) -> Result<PathBuf, GitError> {
    let raw = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let path = PathBuf::from(raw.trim());
    Ok(path.canonicalize().unwrap_or(path))
}

/// The main worktree's top level (the first entry of `worktree list`).
fn main_worktree_root(dir: &Path) -> Result<Option<PathBuf>, GitError> {
    let list = git(dir, &["worktree", "list", "--porcelain"])?;
    Ok(list
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from))
}

fn parse_worktrees(dir: &Path) -> Result<Vec<Worktree>, GitError> {
    let out = git(dir, &["worktree", "list", "--porcelain"])?;
    let mut trees = Vec::new();
    let mut cur: Option<Worktree> = None;

    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            trees.extend(cur.take());
            cur = Some(Worktree {
                path: PathBuf::from(p),
                head: String::new(),
                branch: None,
                detached: false,
                bare: false,
                locked: false,
            });
        } else if let Some(w) = cur.as_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                w.head = h.chars().take(8).collect();
            } else if let Some(b) = line.strip_prefix("branch ") {
                w.branch = Some(b.trim_start_matches("refs/heads/").to_owned());
            } else if line == "detached" {
                w.detached = true;
            } else if line == "bare" {
                w.bare = true;
            } else if line.starts_with("locked") {
                w.locked = true;
            }
        }
    }
    trees.extend(cur);
    Ok(trees)
}

/// Configured remotes as `(name, fetch-url)` pairs, sorted by name.
fn parse_remotes(dir: &Path) -> Result<Vec<(String, String)>, GitError> {
    let out = git(dir, &["remote", "-v"])?;
    // Lines look like `origin<TAB>url (fetch|push)`; keep one url per remote.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for line in out.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(name), Some(url)) = (fields.next(), fields.next()) {
            seen.entry(name.to_owned()).or_insert_with(|| url.to_owned());
        }
    }
    Ok(seen.into_iter().collect())
}

/// Gather everything about a single repository, identified by `dir` (any path
/// inside it). Failures are downgraded to warnings so one bad repo does not
/// sink the whole report.
fn build_repo(dir: &Path) -> Repo {
    let root = match main_worktree_root(dir) {
        Ok(Some(root)) => root,
        Ok(None) => dir.to_path_buf(),
        Err(err) => {
            warn(&err);
            dir.to_path_buf()
        }
    };

    let worktrees = parse_worktrees(dir).unwrap_or_else(|err| {
        warn(&err);
        Vec::new()
    });
    let remotes = parse_remotes(&root).unwrap_or_else(|err| {
        warn(&err);
        Vec::new()
    });

    Repo { root, remotes, worktrees }
}

// ---------------------------------------------------------------------------
// Discovery.
// ---------------------------------------------------------------------------

/// Walk `root` and collect every directory containing a `.git` entry (a normal
/// repo dir or a linked worktree's `.git` file). Walk errors become warnings.
fn discover_candidates(root: &Path) -> Vec<PathBuf> {
    let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
        // Don't descend into `.git` internals or known noise dirs (but the
        // directories themselves are still inspected for a `.git` child below).
        if e.file_type().is_dir() {
            if let Some(name) = e.file_name().to_str() {
                if name == ".git" {
                    return false;
                }
                if e.depth() > 0 && PRUNE_DIRS.contains(&name) {
                    return false;
                }
            }
        }
        true
    });

    let mut candidates = Vec::new();
    for entry in walker {
        match entry {
            Ok(entry) => {
                // A repo is any directory holding a `.git` entry: a `.git`
                // directory for a normal checkout, or a `.git` file for a
                // linked worktree.
                if entry.file_type().is_dir() && entry.path().join(".git").exists() {
                    candidates.push(entry.into_path());
                }
            }
            Err(err) => eprintln!("warning: {err}"),
        }
    }
    candidates
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

fn rel_display(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(p) if p.as_os_str().is_empty() => ".".to_owned(),
        Ok(p) => format!(".{}{}", std::path::MAIN_SEPARATOR, p.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The label shown for a worktree: its checked-out branch, or its bare/detached state.
fn worktree_label<'a>(w: &'a Worktree, s: &Style) -> Cow<'a, str> {
    if w.bare {
        s.yellow("(bare)")
    } else if w.detached {
        Cow::Owned(s.red(&format!("detached @ {}", w.head)).into_owned())
    } else {
        s.green(w.branch.as_deref().unwrap_or("?"))
    }
}

fn render(repos: &[Repo], base: &Path, s: &Style) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    let total_worktrees: usize = repos.iter().map(|r| r.worktrees.len()).sum();

    writeln!(
        out,
        "\n{} {}  ({})\n",
        s.green("🌳"),
        s.bold(&format!("{} repositories", repos.len())),
        s.dim(&format!("{total_worktrees} worktrees")),
    )?;

    for repo in repos {
        let name = repo.root.file_name().and_then(OsStr::to_str).unwrap_or("?");

        // A single worktree *is* the repo folder, so fold its branch into the
        // header rather than printing a redundant one-item worktrees section.
        let single = match repo.worktrees.as_slice() {
            [only] => Some(only),
            _ => None,
        };

        if let Some(w) = single {
            let lock = if w.locked { " 🔒" } else { "" };
            writeln!(
                out,
                "{} {}  {}  {}{lock}",
                s.magenta("📦"),
                s.bold(name),
                s.dim(&rel_display(&repo.root, base)),
                worktree_label(w, s),
            )?;
        } else {
            writeln!(
                out,
                "{} {}  {}",
                s.magenta("📦"),
                s.bold(name),
                s.dim(&rel_display(&repo.root, base)),
            )?;
        }

        // Remotes ---------------------------------------------------------
        // When a worktrees section follows, remotes always branch (`├─`);
        // otherwise the last remote closes the repo (`└─`).
        let remotes_are_last = single.is_some();
        let rlast = repo.remotes.len().saturating_sub(1);
        for (i, (rname, url)) in repo.remotes.iter().enumerate() {
            let twig = if remotes_are_last && i == rlast { "└─" } else { "├─" };
            writeln!(
                out,
                "  {} {} {}  {}",
                s.dim(twig),
                s.bold("remote"),
                s.magenta(rname),
                s.dim(url),
            )?;
        }

        // Worktrees — only when there are several (or none to report) --------
        if single.is_none() {
            if repo.worktrees.is_empty() {
                writeln!(out, "  {} {}", s.dim("└─"), s.dim("no worktrees"))?;
            } else {
                writeln!(out, "  {} {}", s.dim("└─"), s.bold("worktrees"))?;
                let last = repo.worktrees.len() - 1;
                for (i, w) in repo.worktrees.iter().enumerate() {
                    let twig = if i == last { "└─" } else { "├─" };
                    let lock = if w.locked { " 🔒" } else { "" };
                    writeln!(
                        out,
                        "     {} {}  {}{lock}",
                        s.dim(twig),
                        worktree_label(w, s),
                        s.dim(&rel_display(&w.path, base)),
                    )?;
                }
            }
        }
        writeln!(out)?;
    }
    out.flush()
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

fn run() -> Result<(), Box<dyn Error>> {
    let style = Style::new();

    let start = env::args()
        .nth(1)
        .map_or_else(env::current_dir, |a| Ok(PathBuf::from(a)))?;
    let base = start
        .canonicalize()
        .map_err(|e| format!("cannot access {}: {e}", start.display()))?;

    eprintln!("⏳ scanning {} …", base.display());

    // Phase 1: discover candidates and resolve each to its owning repo.
    let candidates = discover_candidates(&base);
    let resolved: Vec<(PathBuf, PathBuf)> = candidates
        .par_iter()
        .filter_map(|dir| match git_common_dir(dir) {
            Ok(common) => Some((common, dir.clone())),
            Err(err) => {
                warn(&err);
                None
            }
        })
        .collect();

    // Dedup by shared git dir, keeping the first candidate seen for each repo.
    let mut unique: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for (common, dir) in resolved {
        unique.entry(common).or_insert(dir);
    }

    // Phase 2: query every unique repo once, in parallel.
    let unique_dirs: Vec<PathBuf> = unique.into_values().collect();
    let mut repos: Vec<Repo> = unique_dirs.par_iter().map(|dir| build_repo(dir)).collect();
    repos.sort_by(|a, b| a.root.cmp(&b.root));

    if repos.is_empty() {
        println!("∅ no git repositories found under {}", base.display());
        return Ok(());
    }

    render(&repos, &base, &style)?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("gitgrove: {err}");
            ExitCode::FAILURE
        }
    }
}
