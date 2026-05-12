# openab

## Repository Structure

- `origin` = `howie/openab` (personal fork); `upstream` = `openabdev/openab` (main repo)
- Local `main` is often far behind upstream — always `git fetch upstream` before checking upstream state
- To work on upstream changes without touching local working tree: `git worktree add ../openab-fix-XXX -b fix/XXX upstream/main`
- PRs target `openabdev/openab main`; push branch to `origin` then use `--head "howie:branch-name"` in `gh pr create`
