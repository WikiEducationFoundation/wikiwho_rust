# notes/

Session log + decision queue for the autonomous development loop. See `../CLAUDE.md` §"Autonomy posture" for the full framing.

## Files

- **`YYYY-MM-DD-<topic>.md`** — one per working session. Append-only after the session ends.
- **`decisions-needed.md`** — append-only queue of forks-in-the-road that need Sage's call. Each entry: brief context, candidate options, recommendation, `blocking | non-blocking` tag, and the date it was queued. Sage reads this file at the start of each interaction.

## Session note template

```markdown
# YYYY-MM-DD — <topic>

**Goal:** one sentence.

**Parity:** before → after (or "N/A" if no algorithm changes).
- tokens: X / Y (Z%)
- revisions: X / Y (Z%)
- ms/rev (Obama p50, single-core): N

**Done:**
- bullet list of substantive changes

**New decisions queued:** link to `decisions-needed.md` entries, if any.

**Next session likely starts with:** one sentence.
```

## Why this pattern

The development loop is mostly autonomous — Sage doesn't read every diff. The session note is the conversation between sessions: future-Claude opens it, sees the parity trajectory and the pending decisions, and picks up without losing context. Keep notes short; they exist to orient the next session, not to document the work in detail.
