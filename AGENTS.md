# Palette development

This repository owns the Julia operator surface, Rust host, adapters, tests, and
public documentation (`runtime/docs/`). Cyan's harness and deployment-specific language toolchains
live separately. Internal research notes and run receipts belong in the gitignored `.archive/`
or an external private archive. Preserve source revisions, hashes, original
receipts, failed attempts, and attribution for a future research paper. Never
stage internal research or credentials into the public release.

## Implementation

- Read owning files before broad changes. Preserve documented behavior.
- Session mode defines the host boundary. `operator` mode runs with the user's
  normal authority; configured broker aliases retain their declared policy.
  `sandbox` mode is opt-in and keeps host effects ceiling-bound inside the OS
  boundary. Workers must not mint grants or weaken that sandbox isolation.
- Prefer Julia-native APIs. Keep process entrypoints and durable formats explicit.
- Routine dependency updates require a minimum release age of seven days.
  Urgent patches require an explicitly justified exception.
- Work on feature branches. Never merge PRs or rewrite shared history without
  Matt's instruction. Stage only owned, explicitly named paths.
- Do not remove intentional functionality unless the user authorizes its removal.
- Never use blanket reset, checkout, clean, stash, or bypassed Git hooks.

## Verification

- Run `git diff --check` and `node runtime/scripts/check-test-policy.mjs` after code changes.
  This standalone Julia/Rust repository does not have npm package scripts.
- Run every modified test file directly. Run Julia package tests for Julia changes
  and Rust tests for host changes. Real process/conformance entrypoints are in
  `cargo xtask verify` (`runtime/xtask`); its prepared test environment must point here.
- Tests must fail on broken behavior. Prove regressions with negative controls.
- No fabricated results, retries, weakened assertions, conditional skips, optional
  assertions, or environment-gated success paths. Record every failed attempt.
- Await concrete readiness/completion signals. Timers bound failure; fixed sleeps
  and polling must not establish successful readiness.
- Use unique temporary resources, port 0, and finally-based cleanup. For process,
  concurrency, or ordering changes, run focused suites with multiple shuffle seeds
  and stop on the first failure.
- Added test lines must not exceed added source lines. Put regressions in the
  owning existing suite, using case IDs in names.
- Fix verification failures before committing. Publish changes on a feature branch
  for review, with concise descriptions of behavior and actual validation.
