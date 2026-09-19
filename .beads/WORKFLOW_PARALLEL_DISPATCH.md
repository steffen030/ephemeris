# Parallel Beads Dispatch Workflow

## Overview
Dispatch max 4 dependency-free beads issues into parallel subagents using their model recommendations.

## Setup
```bash
mkdir -p .worktree
bd ready  # Identify open tasks with no blockers
```

## Dispatch Process

### 1. Create worktrees and feature branches
```bash
git stash  # Clear any pending changes
for id in $BEAD_IDS; do
  git branch feature/$id
  git worktree add .worktree/$id feature/$id
done
```

### 2. For each subagent
- **Working directory**: `.worktree/{bead-id}/`
- **Branch**: `feature/{bead-id}`
- **Model**: Use label from `bd show {bead-id}` (model:*)

### 3. Subagent workflow (per task)
```bash
cd .worktree/{bead-id}
bd update {bead-id} --claim
bd show {bead-id}  # Review requirements
# ... implement feature ...
cargo test
git add -A && git commit -m "feat({bead-id}): description"

# After implementation complete:
cd /Users/sgalan/GIT/ephemeris
git checkout main
git merge feature/{bead-id}
bd close {bead-id}
```

### 4. Cleanup
```bash
git worktree remove .worktree/{bead-id}
git branch -d feature/{bead-id}
```

## Status Checks
- `git worktree list` - Active worktrees
- `bd list --status=in_progress` - Claimed issues
- `git log main` - Merged commits

## Key Rules
- Each bead → one worktree
- Main stays clean (all work in feature branches)
- Model label guides agent selection
- Commit locally in worktree → merge to main → close issue
