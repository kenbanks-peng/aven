# Validation

Use focused checks for changed behavior, not a full gate after every edit or
commit. Documentation and formatting changes need only applicable static checks;
tooling changes need their focused fixtures, not application builds.

- Prefer `just test-lib <package> '<module>::'` for library tests and
  `just test-target <package> <target>` for integration tests. These reuse the
  full suite's environment and `target/test` cache. Avoid bare `cargo test
  <filter>`, which builds unrelated test targets even with a narrow name filter.
- Batch related filters in one `just _test --package <package> --lib ...`
  invocation. Do not run competing Cargo commands against the same target
  directory; they wait on build locks.
- Do not stack build, check, clippy, and tests as a default checklist. Tests
  compile their selected targets. Run package linting or broader checks when
  the affected contracts warrant them, not merely because a commit is ready.
- Reserve `just check-full` for explicit full-validation requests, release
  qualification, or an integration whose risk warrants the full workspace.
  Protocol, cryptography, migrations, and shared storage changes need relevant
  cross-component coverage; select it from the actual diff.
- Preserve full command logs and exit status; read the saved log after failure
  rather than rerunning to recover diagnostics. Fix known blockers before retrying.
- Reuse passing evidence when source, dependencies, configuration, and relevant
  environment are unchanged. A commit, rebase, or merge is not by itself a reason
  to rerun; inspect what changed and repeat affected checks.
- When coordinating agents, assign focused validation to each worker and one
  owner for any broader integration check after the batch is stable. Reuse worker
  evidence where applicable instead of repeating every worker's checks.

Keep hooks and required safety checks enabled. Report what ran and what was
intentionally deferred; focused validation is not a claim of release readiness.
