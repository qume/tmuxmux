# tmuxmux — progress

> **Now:** Cross-platform-ready and much lighter. sshpass is no longer invoked (tmuxmux types the password itself), so the same commands work on Windows; idle CPU fixed from ~50% to ~0%; hide/unhide sessions added. Next move: try the Windows build from CI, and consider SSH keys to get passwords out of the config entirely.
> **Health:** working — verified the sshpass-free paths (interactive attach + snapshot) against live geocam apps with sshpass shadowed by a failing fake; idle CPU measured ~0%.
> **Watch out:** enabling an [[app_managers]] entry makes tmuxmux rewrite hosts.toml on launch (only below the auto marker). Apps are only snapshotted when their sidebar row is expanded, to keep connection fan-out low on flaky links.

## Log

### 2026-07-11 · sshpass-free (Windows-ready), CPU fix, hide sessions
Three things. (1) tmuxmux no longer shells out to `sshpass` — it strips the
`sshpass -p PW` prefix and types the password into the pty itself (interactive
attach) or via a pty-capture helper (snapshot/log). Same command now works on
Windows, where sshpass doesn't exist. Proven by shadowing sshpass with a
failing fake and confirming attach + snapshot still connect. (2) Idle CPU was
~50% — the loop force-repainted the terminal at 60fps always; now
activity-based (fast only after real input/output), idle ~0%. (3) Hidden
sessions: hover-× or Delete to hide other people's processes; unhide from a
'⊘ hidden' group; persisted in sqlite.

**Next:** grab the Windows binary from CI and smoke-test it; if we want passwords out of hosts.toml, do SSH-key injection on the containers (apps-manager side).

### 2026-07-11 · App-manager integration
tmuxmux can now pull apps from geocam apps-manager instances. Config is
`[[app_managers]]` (domain + user/pass); on launch it logs in
(`/api/auth/login` → JWT), calls the new `/api/apps/connect-list`, and
materialises each app as a host grouped instance → category → app. The
apps-manager side already returned three buckets + a ready ssh command, so the
other bot only had to add a thin stable endpoint. Discovered apps are written
back below a marker in hosts.toml; reconcile keeps hand-written hosts and marks
vanished apps closed.

**Next:** add [[app_managers]] to ~/sync/hosts.toml with your real login; consider gating snapshots to expanded apps if the connection fan-out feels heavy.

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
