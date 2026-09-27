---
name: board
description: Claim work from the project's radar board (BOARD.md) before editing files, and hand finished work back through it. Required for every non-trivial change.
---

# The board

This project coordinates work through a kanban in the repo root: BOARD.md.
radar renders it, the `radar card` commands edit it atomically, and the file
can be read and edited with the tools you already have. Other agents may be
working in this repository at the same time — the board is how we stay out of
each other's way.

## The loop

1. Claim work before editing anything:
   `radar card next --by "$RADAR_AGENT"` — claims the first unclaimed card in
   your name and prints it.
2. Work one card at a time, and only what the card describes.
3. Blocked, or the card is wrong? Append a note saying why (an indented line
   under the card in BOARD.md), release with
   `radar card release --title "…"`, claim the next card.
4. Finished? Leave a short handover note for the reviewer under the card —
   what changed, how to check it — then move the card to Review:
   `radar card move --title "…" --to Review`. Moving hands your claim over:
   from that moment the card is not yours.

## Review — the handoff between agents

Your card is not finished when you say so; it is finished when a *different*
agent has reviewed it. When claiming work, prefer reviewing first:

    radar card next --in Review --by "$RADAR_AGENT"

Approve with `radar card done --title "…"`, or send it back to In progress
with a note saying what failed. If your own card comes back, reclaim it by
title: `radar card claim --title "…" --by "$RADAR_AGENT"`. `card done` is the
reviewer's word, never the worker's.

## Rules

- Never touch a card another agent has claimed.
- Never move your own card past Review.
- Non-trivial work always goes through a card. If the board has none that
  fits, add one (`radar card add --title "…" --body "…"`) and claim it.
- Editing BOARD.md itself is always allowed — claims are managed on it.
- Committing requires a claim: this repository's git pre-commit hook refuses
  a commit from you when BOARD.md shows no live claim by your name. Claim
  first, commit after — the hook's refusal text says how.

If `$RADAR_AGENT` is unset you were not launched by radar: pick a short unique
name for `--by` (your model name plus a suffix) and follow the same loop.
