---
name: progress-log
description: >
  Maintain PROGRESS.md, a human-facing narrative log that lets the person who
  directs this work re-enter it fast after days or months away. Use at the end
  of any work session that changed the state of play — after shipping a change,
  making a decision, hitting a blocker, or finishing an investigation. Skip it
  for trivial one-off edits.
---

# Progress log

Keep a file called `PROGRESS.md` at the repository root (the git root of the
directory you're working in). tmuxmux shows it live in a side pane. Your job is
to make it the fastest, lowest-energy way for one specific person to get back
up to speed.

## Who reads this, and why it changes how you write

The reader is the person who *directs* this work — not the one who did it
keystroke by keystroke. You did that. They came back after days or months,
holding several other projects in their head, and they need to re-enter this
one with the least possible effort.

Two things about this reader are easy to get wrong:

1. **They have an LLM.** Detail is one prompt away — they can ask an agent to
   re-read the code, replay the git history, or explain any mechanism on
   demand. So do **not** write a manual. Anything cheaply re-derivable is
   wasted words that bury the words that matter. Write only what is *expensive
   to recover*: why a decision went the way it did, what you tried that failed,
   what's risky or unverified, what they were worried about.

2. **Their scarce resource is activation energy, not information.** The hard
   part of coming back isn't finding facts — it's the cost of the context
   switch and deciding what to do next. So the log's job is orientation and
   triage: *where am I, is it healthy, what's the one next move.* Front-load
   that; defer everything else. A glance should orient them; a half-read should
   be plenty.

That is the post-LLM shape of this job: the log is an **orientation layer over
queryable detail**, not a store of detail. Shallow, pointed, current.

## The file

Keep exactly this shape. The top block is **rewritten** every time. New journal
entries are **prepended** (newest first) so the reader never has to scroll to
find "now."

```markdown
# <project> — progress

> **Now:** <1–2 sentences: the current state of play + the single next move.>
> **Health:** working | partial | broken — <half a line why>
> **Watch out:** <the one thing they'd regret not knowing — or drop this line>

## Log

### YYYY-MM-DD · <3–6 word headline>
<2–4 lines. What changed and, mostly, *why*. A decision and its reasoning.
Anything that surprised you or that you'd have wanted told.>

**Next:** <one directable next step, phrased so they could hand it to an agent>
```

(The blank line before `**Next:**` keeps it on its own line; consecutive prose
lines above it flow into one paragraph.)

If the file doesn't exist, create it with this skeleton.

## How to write it

- **Rewrite `Now` every session.** It's the whole point. It must be true this
  minute — one glance = oriented.
- **Prepend one entry per meaningful session.** Not per commit, not per action:
  this is a narrative, not an event stream. If nothing changed the state of
  play, don't add an entry.
- **Headlines carry the arc.** Someone reading only the `###` lines should get
  the story. Make each a real summary, not a label.
- **Lead with the point.** First sentence says what happened. No throat-clearing,
  no "In this session I…".
- **Keep the why, drop the what.** "Switched to thin LTO — full LTO OOM-kills
  rustc on this box" → keep. "Edited Cargo.toml" → drop.
- **Name the risk.** If something is untested, fragile, or you weren't sure, say
  so plainly. The returning director's first question is "can I trust this?"
- **`Next` is a prompt, not a chore.** "Have an agent add timeout handling to the
  ssh fetch and test it against a dead host" — not "fix ssh".
- **Calm, concrete, plain.** Short sentences. No hype. Past tense for what
  happened, present for what is.

## Length is a feature

`Now` ≤ ~40 words. Each entry ≤ ~60 words. Over that, you're writing detail the
reader can re-derive — cut it. Under-write and let them ask.

## Don't confuse it with

- **CLAUDE.md / AGENTS.md** — instructions *to* the bot. This is a briefing *for*
  the human.
- **Commit messages / CHANGELOG** — the mechanical record of what changed. This
  is why it changed and where things stand.
- **README** — for someone *using* the project. This is for someone *resuming* it.
