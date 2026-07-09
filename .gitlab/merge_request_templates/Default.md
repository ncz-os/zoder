## Summary

- **Base branch:** `master`
- **What changed and why:** (2–5 bullets — the diff shows *what*, you explain *why*)
- **Scope boundary:** (what this PR explicitly does NOT change)
- **Blast radius:** (what other subsystems or consumers could be affected)
- **Linked issue(s):** Use `Closes #` / `Fixes #` / `Resolves #` only for issues this
  PR fully resolves; otherwise `Related #`, `Depends on #`, or `Supersedes #`.

## Validation Evidence (required)

Run the full gate locally and paste literal output tails (failures/warnings, not "all passed"):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features   # or: cargo test --workspace --all-features
cargo deny check
```

- **Commands run and tail output:**
- **Beyond CI — what did you manually verify?** (scenarios, edge cases, what you did NOT verify)
- **If any command was intentionally skipped, why:**

## Security & Privacy Impact (required)

Yes/No each; explain any `Yes` in 1–2 sentences.

- New permissions, capabilities, or filesystem access scope? (`Yes/No`)
- New external network calls? (`Yes/No`)
- Secrets / tokens / credentials handling changed? (`Yes/No`)
- PII / real identities / personal data in diff, tests, fixtures, or docs? (`Yes/No`)

## Compatibility (required)

- Backward compatible? (`Yes/No`)
- Config / env / CLI surface changed? (`Yes/No`)
- If `No` to compat or `Yes` to surface change: exact upgrade steps for existing users:

## Rollback

Low-risk: `git revert <sha>`. Medium/high-risk — fill: fast rollback path, feature
flags/toggles (or `None`), observable failure symptoms.

---

**No bot/AI attribution footers** (`Co-authored-by: Claude …`, "Generated with …") in
the PR body or commit tails. **Never** commit real identities, secrets, personal emails,
or PII in diff, tests, fixtures, or docs — this is a merge gate.
