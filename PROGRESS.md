# tmuxmux — progress

> **Now:** Progress pane shipped and working; sessions with a log now show an amber dot in the sidebar. Next move: dogfood for a week, then decide if the `Now`/entry word-caps feel right and whether to build a generator for non-agent sessions.
> **Health:** working — three-pane layout, remote log fetch, and per-session badges all verified on `bots`.
> **Watch out:** the log's *writer* side is unsolved — only Claude Code sessions that have the skill write logs. Populating them across all tools/hosts is still an open decision (generator vs teach-each-agent).

## Log

### 2026-07-03 · Sidebar log badges
Sessions whose repo has a `PROGRESS.md` now show a small amber dot, so you can
tell at a glance which of many have context waiting. Detection is folded into
the existing per-host snapshot — one ssh probe per host, not per session.
Gotcha caught: first tried a tab field-separator, but tabs get flattened to
`_` over ssh to non-UTF-8 hosts (same reason the sweep uses `<#~#>`); switched
to the printable separator with POSIX parameter-expansion splitting.

**Next:** decide the writer architecture (generator that distills scrollback, vs distributing the skill to every host) — the reader side is done.

### 2026-07-03 · Narrative progress pane
Added a right-hand pane that renders a repo's `PROGRESS.md` and a `progress-log`
skill telling agents how to write one. Key design call: the log is an
*orientation layer*, not a manual — the reader has an LLM to re-derive detail,
so we optimise for low-energy re-entry (a `Now` block + newest-first entries),
not completeness. Pane auto-hides when no log exists, so it costs no space.

**Next:** install the skill into `~/.claude/skills` and live with it before tuning the format.

### 2026-07-03 · Font zoom
Ctrl+= / Ctrl+- / Ctrl+0 resize the terminal font (clamped 7–40px); the PTY
reflows. Intercepted as global shortcuts so they never leak to the shell.

**Next:** none — done.

### 2026-06-12 · Dock launch was invisible
"No window on dock click" turned out to be config discovery, not a GUI bug:
the dock launches from `$HOME`, so `hosts.toml` wasn't found and the app exited
before drawing. Fixed by pointing the `.desktop` Exec at `~/sync/hosts.toml`.

**Next:** make a missing config open an error window instead of exiting silently — belt-and-braces so this never mystifies us again.

### 2026-06-07 · Session cache + reboot recovery
Every host is snapshotted on a timer into sqlite (structure, layouts, cwds, and
each pane's real command line via a remote process-tree walk). Vanished sessions
become a restorable "closed" group; "restore all" repopulates a rebooted VM.
Also killed the startup freeze — vsync blocked on a compositor frame callback
while the screen was locked; runs with vsync off now.

**Next:** none pressing.

### 2026-06-05 · The white-background bug, and the rest of v1
Rebuilt the native app in Rust/egui. The original's fatal bug: `Color::Default`
mapped to white for *both* fg and bg, so every cell painted a white background.
Fixed with separate fg/bg converters. Also: DEC ACS translation (so box-drawing
isn't "qqqq"), drag-select + clipboard, and a `--script` harness for headless
self-testing.

**Next:** superseded by later work.
