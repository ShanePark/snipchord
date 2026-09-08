## Implementation

- Read the relevant code, tests, and local instructions before editing.
- Before implementation, ask clarifying questions about any remaining material ambiguity that affects scope, behavior, interfaces, or acceptance criteria. Do not proceed on material assumptions.
- Make the smallest, simplest change that fully satisfies the request.
- Do not invent requirements, broaden scope, or add speculative abstractions or dependencies.
- Follow existing conventions and preserve unrelated behavior and work.
- Write clear code. Comment only non-obvious rationale, invariants, or constraints.

## Delegation and Parallel Work

- The main agent's primary role is orchestration: planning, decomposition, delegation, coordination, review, user communication, and handling additional work—not hands-on execution.
- Delegate repository exploration, implementation, testing, and verification to subagents by default. Do not occupy the main agent with substantial work that can be delegated.
- Maximize safe parallelism across independent workstreams.
- The main agent may directly handle only brief, local tasks that do not benefit from delegation, as well as integration, conflict resolution, or work that requires its broader context.
- Give each delegated task a single owner with explicit scope, deliverables, dependencies, and verification criteria.
- Parallelize only independent work. Concurrent writes must not overlap in files, mutable state, or contracts.
- Shared files and cross-cutting contracts must have a single owner.
- The main agent remains accountable for integration, review of the final diff and verification results, and the accuracy of the completion report.

## Completion

- Run verification proportional to the change, starting with focused checks and expanding based on risk and blast radius.
- Never weaken or bypass tests or checks to make a change pass.
- Do not claim completion without relevant verification. State exactly what was not verified and why.
- Distinguish failures caused by the current change from pre-existing failures.
- Report what changed, the checks run and their results, unverified areas, and remaining risks or assumptions.
- After completing and verifying application changes, build and install the updated executable, then restart SnipChord yourself: run `~/.local/bin/snipchord --quit`, wait for the old process to exit, and launch `~/.local/bin/snipchord --daemon` in the background.
- This restart is authorized as part of development completion. Do not ask the user to run the quit command or request confirmation for a routine restart. Preserve settings and shortcuts, and preserve clipboard contents where possible without deferring the restart to the user.
- Verify that the new process is running the installed build before reporting completion. Report any restart failure explicitly. Documentation-only changes do not require a build or restart.

## Git

- Do not perform version-control operations that change local or remote repository state unless explicitly requested.
- Before any requested version-control write, inspect the working tree and relevant diffs.
- Commit only changes made for the current task.
- Treat pre-existing changes as user-owned. Never discard, overwrite, stage, or commit unrelated work.
