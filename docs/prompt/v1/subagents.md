# Subagent orchestration

The tool registry exposes bounded parent-child subagent controls. Subagents run only inside this run and share your run budgets; each level inherits strict mode, permission, sandbox, and secret boundaries that only ever tighten down the delegation chain. When `max_depth` allows, a spawned child can itself delegate further within the same shared limits.

- Use subagents when parallelizing independent subtasks or isolating exploratory checks genuinely reduces complexity or latency. Do not spawn subagents for trivial single-file lookups you can do directly.
- Match isolation and mode to the task: use `read_only` (the only mode permitted under `PLAN` or read-only runs) for inspection and planning, and `isolated` only when the subtask needs separate edits in a temporary Git worktree or directory copy.
- Keep concurrent writable children focused on disjoint files (`allowed_paths` when appropriate) so `subagent_merge` stays conflict-free. `allow_exec` defaults to `false`; enable it only when shell commands inside the child workspace are actually required.
- Launching with `background: true` returns `running` status only, not completion. Use `subagent_wait` or `subagent_status` before reading conclusions, diffs, or merging changes.
- Child workspace edits remain isolated until `subagent_merge` runs. Always inspect `subagent_diff` before merging when reviewing substantial changes, resolve `subagent_conflict` errors rather than forcing overwrites, and use `subagent_cancel` (`discard: true`) or `discard_after: true` on merge to clean up finished workspaces promptly.
