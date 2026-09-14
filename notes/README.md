# Notes

Fork-internal development documents: bug investigations, release checklists,
design scratch, and agent-generated analysis.

This directory is gitignored. Nothing in it is committed, published, or visible
to anyone who clones the fork. Treat it as a local scratch space that happens to
live inside the repo for convenience.

It sits outside `docs/` on purpose. `docs/` is upstream's mdBook and its most
heavily churned directory, so anything stored there lands in the blast radius of
every upstream sync, and `docs/.rules` applies Zed's user-facing documentation
voice to that whole tree.

## Do not move these here

They look like notes but something depends on them:

- `docs/features.md` - public feature inventory, linked from the README
- `docs/mobile-development.md` - user-facing setup walkthrough, linked from `docs/features.md`
- `RELEASE_NOTES.md` - parsed by `.github/workflows/release_fork.yml` to build release bodies

## Layout

- `bugs/` - investigations into specific defects
- `releases/` - per-release readiness checks and postmortems
