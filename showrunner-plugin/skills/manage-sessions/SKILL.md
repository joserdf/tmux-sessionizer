---
name: manage-sessions
description: View, create and manage other Showrunner tasks and sessions, and ask an agent running in another session a question
user-invocable: true
---

You are running inside a Showrunner session. The `showrunner` CLI lets you see the other
tasks and sessions on this machine, create new ones, and talk to the agents running in them.

Every command is a plain shell call. Run them from anywhere; they act on the shared Showrunner
state (`~/.showrunner/`), not on your worktree.

## Session refs

Sessions are addressed as `<project>/<task>/<session>`:

- `myapp/fix-auth/main` — the task's main session (works on the task branch itself)
- `myapp/fix-auth/2` — an extra session (works on its own `<task-branch>-2` branch)
- `myapp/adhoc/scratch` — an ad-hoc session (no task, runs in the project dir)
- `myapp/fix-auth` — shorthand for that task's `main` session

Raw tmux names (`cm__myapp__fix-auth__main`) also work. Always take refs from `list` rather than
guessing — project and task names are sanitized in refs (spaces and symbols become `-`).

## Viewing

```
showrunner list                      # projects, tasks, and live sessions with status
showrunner list --json               # same, machine-readable
showrunner list --project <name>     # one project only
```

The session you are in is marked `(this session)`. Statuses are `running` (still working),
`waiting_input` (idle at its prompt), `waiting_permission` (blocked on a dialog, needs the user), and
`finished` (claude exited). Listing samples each pane twice, so it takes a second or two.

Tasks whose base branch is another task's branch form a stacked-PR chain; `list` marks each member
with `stack=<position>/<size>` (a `stack` object in `--json`). Treat a stacked task's parent branch
as its integration target: PRs and rebases go against the base branch, not main.

## Asking another session a question

```
showrunner ask <session> "<question>" [--timeout <secs>]
```

This sends the question, waits until that agent stops working, and prints its reply on stdout
(default timeout 300s). Use it when another agent holds context you don't have — what it changed and
why, which module owns something in its area, whether an interface it owns is settled.

- A busy target queues the message and answers after its current work; you just wait longer.
- Exit status is non-zero on timeout or when the target is stuck on a dialog. Whatever it printed so
  far still goes to stdout, so read that before retrying.
- Ask specific, self-contained questions. The other agent cannot see your conversation, so include
  the context it needs and name files by path.
- Don't use `ask` for work you can do yourself — reading the repo, or the other session's diff via
  its worktree, is cheaper and doesn't interrupt anyone.

To notify without waiting, or to read a pane directly:

```
showrunner send <session> "<text>"        # fire and forget (adds --no-submit to type without sending)
showrunner output <session> --lines 200   # what's currently on that session's screen
```

## Creating work

```
showrunner task create <project> <name> [--branch <b>] [--base <b>] [--prompt "<initial task>"]
showrunner task set-base <project> <task> <branch>
showrunner session create <project> <task> [--prompt "<initial task>"] [--no-worktree]
```

- `task create` branches off `main`, registers the task, and starts its main session in a fresh
  worktree. Without `--branch` the branch name is derived from the task name.
- `--base <branch>` makes the task part of a stack: it sets the task's base branch (the existing
  branch must exist in the repo), and a newly created task branch starts from it instead of main.
  Use it when building on another task's branch. `task set-base` fixes the base after creation
  (pass `main` to reset).
- `session create` adds a parallel session to an existing task, on its own `<task-branch>-<n>`
  branch in its own worktree. Use it to fan out independent work within the same task.
- `--prompt` is the agent's first instruction. Make it self-contained: a new session starts with no
  knowledge of your conversation. State the goal, the relevant paths, and what "done" looks like.
- New sessions start immediately in the background. Poll them with `list`, read them with `output`,
  or ask them with `ask`.

## Deleting

```
showrunner task delete <project> <task> --yes
showrunner session kill <session> --yes
```

Both are destructive: they kill the sessions, remove their worktrees, and delete the branches
involved, so unmerged work is lost. `--yes` is required. A task's main session can't be killed on
its own — delete the task instead.

**Only delete what you created, and only when the user asked for it.** Another agent's session may be
mid-task, and its branch may hold the only copy of its work. When in doubt, ask the user.
