# Project Instructions for AI Agents

This file provides instructions and context for AI coding agents working on this project.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

## Development Workflow (Beads + Git Worktrees)

This project uses **git worktrees** to develop features and fixes in isolation before merging to main.

### Standard Workflow

1. **Find and claim work**
   ```bash
   bd ready                    # Show available issues
   bd show <id>                # Review issue details
   bd update <id> --claim      # Claim the issue
   ```

2. **Create a worktree for development**
   ```bash
   git worktree add feature/<bead-id> main
   cd feature/<bead-id>
   ```
   Branch naming: Use `feature/<bead-id>` for consistency with beads tracking.

3. **Develop and test**
   - Implement changes in the worktree
   - Commit changes locally (commits stay in the worktree branch)
   - Run tests and validation
   - Iterate until satisfied

4. **Merge back to main**
   ```bash
   # Switch to main branch
   cd ..
   git checkout main
   git pull --rebase origin main  # Ensure main is up-to-date (if using a remote)
   
   # Merge the feature branch
   git merge feature/<bead-id>
   ```

5. **Clean up and close**
   ```bash
   git worktree remove feature/<bead-id>
   bd close <id>  # Close the beads issue
   ```

### Key Commands

```bash
# Worktree management
git worktree list                              # Show all active worktrees
git worktree add feature/<name> main           # Create new worktree from main
git worktree remove feature/<name>             # Remove worktree after merge

# Development in a worktree
cd feature/<id>                                # Work in isolated directory
git status                                     # See changes
git commit -m "message"                        # Commit changes
git push origin feature/<id>                   # Push if using remote
cd ..                                          # Return to main worktree

# Merge when ready
git checkout main
git merge feature/<id>                         # Merge after testing
```

### Tips

- **Each beads issue → one worktree**: This keeps work isolated and makes it easy to switch between tasks.
- **Main branch stays clean**: Main is always deployable since all work is done in worktrees before merging.
- **Commit discipline**: Use meaningful commit messages that reference the bead ID.
- **Before merging**: Always verify tests pass and code quality checks pass in the worktree.

## Build & Test

_Add your build and test commands here_

```bash
# Example:
# npm install
# npm test
```

## Architecture Overview

_Add a brief overview of your project architecture_

## Conventions & Patterns

_Add your project-specific conventions here_
