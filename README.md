# 🌳 gitgrove

Recursively discover **git repositories** beneath a directory and print their
**worktrees** and **branches** as a friendly, colourful command-line tree.

```
🌳 3 repositories  (9 worktrees, 21 branches)

📦 casetta  ./casetta
  ├─ worktrees
  │  ├─ main                  ./casetta
  │  ├─ docs/refine-vision    ./casetta_refine-vision
  │  └─ refined-vision-proto  ./casetta_refined-vision-proto
  └─ branches (5)
     ├─ ● main          795db74  ↪ origin/main
     ├─ ○ backup-main   d860082
     └─ …
```

## Why

When you keep many repos (and many `git worktree`s) side by side, it's hard to
see the whole landscape at a glance. `gitgrove` walks the tree once, then
interrogates every repository **in parallel** and renders a tidy overview.

## Features

- 🔎 Recursive discovery of git repos, pruning noise (`node_modules`, `target`,
  `.venv`, …).
- 🌲 Lists every **worktree** per repo, collapsing linked worktrees back onto
  their owning repository so each repo appears once.
- 🌿 Lists **local branches** with short SHAs and upstream tracking, sorted by
  most recent commit, highlighting the checked-out `HEAD`.
- ⚡ **Fast & concurrent** — directory discovery is a single pass, then all git
  queries run on a [`rayon`](https://crates.io/crates/rayon) work-stealing pool.
- 🎨 Colour output that auto-disables when piped or when `NO_COLOR` is set.

## Install / Build

Requires a Rust toolchain and `git` on `PATH`.

```sh
cargo build --release
# binary at ./target/release/gitgrove
```

## Usage

```sh
# scan the current directory
gitgrove

# scan a specific directory
gitgrove ~/source/repos

# plain output (no colour), e.g. for piping
NO_COLOR=1 gitgrove > repos.txt
```

## License

MIT
