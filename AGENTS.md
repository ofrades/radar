## The board (required)

Claim work before editing files, and hand finished work back through the
board: run `radar board` to read it, or `radar card next --by "$RADAR_AGENT"`
to claim the next card. There is no board file — cards live in radar's store.
The board skill (installed at `~/.agents/skills/board/SKILL.md`) has the full
loop. A claim is also required to commit — the git pre-commit hook enforces it.
