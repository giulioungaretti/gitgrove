//! gitgrove — recursively discover git repositories and present their
//! worktrees and branches as a friendly command-line tree.
//!
//! Concurrency story: directory discovery is a fast single pass, then every
//! repository is interrogated in parallel with `rayon`'s work-stealing pool.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use rayon::prelude::*;
use walkdir::WalkDir;

/// Directories we never want to descend into while searching for repos.
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
// ANSI colouring (no external dependency; auto-disabled when not a TTY).
// ---------------------------------------------------------------------------

struct Style {
    on: bool,
}

impl Style {
    fn new() -> Self {
        // Respect NO_COLOR and only colour real terminals.
        let on = io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
        Style { on }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn bold(&self, t: &str) -> String {
        self.paint("1", t)
    }
    fn dim(&self, t: &str) -> String {
        self.paint("2", t)
    }
    fn green(&self, t: &str) -> String {
        self.paint("32", t)
    }
    fn yellow(&self, t: &str) -> String {
        self.paint("33", t)
    }
    fn magenta(&self, t: &str) -> String {
        self.paint("35", t)
    }
    fn red(&self, t: &str) -> String {
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

struct Branch {
    name: String,
    sha: String,
    upstream: Option<String>,
    is_head: bool,
}

struct Repo {
    /// The main worktree path (used as the display root).
    root: PathBuf,
    worktrees: Vec<Worktree>,
    branches: Vec<Branch>,
}

// ---------------------------------------------------------------------------
// Git helpers.
// ---------------------------------------------------------------------------

/// Run `git` inside `dir` and return trimmed stdout, or None on failure.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve the shared git directory for a path inside a repo. Used to collapse
/// linked worktrees back onto their owning repository so each repo appears once.
fn git_common_dir(dir: &Path) -> Option<PathBuf> {
    let raw = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let path = PathBuf::from(raw.trim());
    Some(path.canonicalize().unwrap_or(path))
}

/// The main worktree's top level (the repo's primary checkout).
fn main_worktree_root(dir: &Path) -> Option<PathBuf> {
    // The first `worktree` entry in the porcelain list is always the main one.
    let list = git(dir, &["worktree", "list", "--porcelain"])?;
    list.lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
}

fn parse_worktrees(dir: &Path) -> Vec<Worktree> {
    let Some(out) = git(dir, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    let mut trees = Vec::new();
    let mut cur: Option<Worktree> = None;

    let flush = |cur: &mut Option<Worktree>, trees: &mut Vec<Worktree>| {
        if let Some(w) = cur.take() {
            trees.push(w);
        }
    };

    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut cur, &mut trees);
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
                w.branch = Some(b.trim_start_matches("refs/heads/").to_string());
            } else if line == "detached" {
                w.detached = true;
            } else if line == "bare" {
                w.bare = true;
            } else if line.starts_with("locked") {
                w.locked = true;
            }
        }
    }
    flush(&mut cur, &mut trees);
    trees
}

fn parse_branches(dir: &Path) -> Vec<Branch> {
    // Tab-separated, easy to split, robust against spaces in upstream tracking info.
    let fmt = "%(HEAD)\t%(refname:short)\t%(objectname:short)\t%(upstream:short)";
    let Some(out) = git(
        dir,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            &format!("--format={fmt}"),
            "refs/heads",
        ],
    ) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let head = parts.next()?;
            let name = parts.next()?.to_string();
            let sha = parts.next()?.to_string();
            let upstream = parts.next().filter(|s| !s.is_empty()).map(|s| s.to_string());
            Some(Branch {
                is_head: head.trim() == "*",
                name,
                sha,
                upstream,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Discovery.
// ---------------------------------------------------------------------------

/// Walk `root` and collect every directory that contains a `.git` entry
/// (a normal repo dir or a linked worktree's `.git` file).
fn discover_candidates(root: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let walker = WalkDir::new(root).follow_links(false).into_iter();

    let it = walker.filter_entry(|e| {
        // Never descend into a `.git` directory or pruned noise dirs.
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

    for entry in it.flatten() {
        if entry.file_name() == OsStr::new(".git") {
            if let Some(parent) = entry.path().parent() {
                candidates.push(parent.to_path_buf());
            }
        }
    }
    candidates
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

fn rel_display(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(p) if p.as_os_str().is_empty() => ".".to_string(),
        Ok(p) => format!(".{}{}", std::path::MAIN_SEPARATOR, p.display()),
        Err(_) => path.display().to_string(),
    }
}

fn render(repos: &[Repo], base: &Path, s: &Style) {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    let total_branches: usize = repos.iter().map(|r| r.branches.len()).sum();
    let total_worktrees: usize = repos.iter().map(|r| r.worktrees.len()).sum();

    let _ = writeln!(
        out,
        "\n{} {}  ({}, {})\n",
        s.green("🌳"),
        s.bold(&format!("{} repositories", repos.len())),
        s.dim(&format!("{total_worktrees} worktrees")),
        s.dim(&format!("{total_branches} branches")),
    );

    for repo in repos {
        let name = repo
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?");
        let _ = writeln!(
            out,
            "{} {}  {}",
            s.magenta("📦"),
            s.bold(name),
            s.dim(&rel_display(&repo.root, base)),
        );

        // Worktrees -------------------------------------------------------
        if !repo.worktrees.is_empty() {
            let _ = writeln!(out, "  {} {}", s.dim("├─"), s.bold("worktrees"));
            let last = repo.worktrees.len() - 1;
            for (i, w) in repo.worktrees.iter().enumerate() {
                let twig = if i == last { "└─" } else { "├─" };
                let label = if w.bare {
                    s.yellow("(bare)")
                } else if w.detached {
                    s.red(&format!("detached @ {}", w.head))
                } else {
                    s.green(w.branch.as_deref().unwrap_or("?"))
                };
                let mut extra = String::new();
                if w.locked {
                    extra.push_str(&format!(" {}", s.yellow("🔒")));
                }
                let _ = writeln!(
                    out,
                    "  {}  {} {}  {}{}",
                    s.dim("│"),
                    s.dim(twig),
                    label,
                    s.dim(&rel_display(&w.path, base)),
                    extra,
                );
            }
        }

        // Branches --------------------------------------------------------
        if repo.branches.is_empty() {
            let _ = writeln!(out, "  {} {}", s.dim("└─"), s.dim("no local branches"));
        } else {
            let _ = writeln!(
                out,
                "  {} {} {}",
                s.dim("└─"),
                s.bold("branches"),
                s.dim(&format!("({})", repo.branches.len())),
            );
            let last = repo.branches.len() - 1;
            for (i, b) in repo.branches.iter().enumerate() {
                let twig = if i == last { "└─" } else { "├─" };
                let marker = if b.is_head {
                    s.green("●")
                } else {
                    s.dim("○")
                };
                let name = if b.is_head {
                    s.bold(&s.green(&b.name))
                } else {
                    b.name.clone()
                };
                let up = match &b.upstream {
                    Some(u) => format!("  {}", s.dim(&format!("↪ {u}"))),
                    None => String::new(),
                };
                let _ = writeln!(
                    out,
                    "     {} {} {}  {}{}",
                    s.dim(twig),
                    marker,
                    name,
                    s.yellow(&b.sha),
                    up,
                );
            }
        }
        let _ = writeln!(out);
    }
    let _ = out.flush();
}

// ---------------------------------------------------------------------------
// Main.
// ---------------------------------------------------------------------------

fn main() {
    let style = Style::new();

    // Optional positional argument: starting directory (defaults to cwd).
    let start = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().expect("cannot read current directory"));
    let base = start.canonicalize().unwrap_or(start);

    eprintln!("{} scanning {} …", style.dim("⏳"), base.display());

    let candidates = discover_candidates(&base);

    // Interrogate every candidate in parallel.
    let pairs: Vec<(PathBuf, Repo)> = candidates
        .par_iter()
        .filter_map(|dir| {
            let common = git_common_dir(dir)?;
            let root = main_worktree_root(dir).unwrap_or_else(|| dir.clone());
            Some((
                common,
                Repo {
                    worktrees: parse_worktrees(dir),
                    branches: parse_branches(&root),
                    root,
                },
            ))
        })
        .collect();

    // Collapse linked worktrees onto their owning repo (one entry per repo).
    let mut map: BTreeMap<PathBuf, Repo> = BTreeMap::new();
    for (common, repo) in pairs {
        map.entry(common).or_insert(repo);
    }

    let mut list: Vec<Repo> = map.into_values().collect();
    list.sort_by(|a, b| a.root.cmp(&b.root));

    if list.is_empty() {
        println!(
            "{} no git repositories found under {}",
            style.yellow("∅"),
            base.display()
        );
        return;
    }

    render(&list, &base, &style);
}
