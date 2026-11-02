# PLAN-FB108: rebase FB96 onto the merged FB95 (zoder, base master)

## Premise
freebuff/FB96-20261102 (deee2af) conflicts with freebuff/FB95-20261102, now merged into
zoder master (923f56c). Re-apply FB96's three finding commits on a fresh branch cut from
current master, resolving agentic.rs conflicts keeping BOTH fix sets.

## Steps
1. Fresh clone of zoder.git; branch freebuff/FB108-20261102 from origin/master (923f56c).
2. Cherry-pick FB96's finding commits in order, one commit per finding:
   - c7fceb5 <= 2ac9d7e: F2.2 truncated-diff substance (never judge a truncated diff on
     its visible prefix). Conflicts with FB95's added F2.1/F2.3 tests in the same test
     block - resolved by keeping both blocks; production hunks applied cleanly.
   - b141885 <= a3c4fa6: F2.4 keep retry_after + token_plan_resets_at hints on
     reviewer fallback. Applied cleanly.
   - 3d2bdee <= deee2af: F2.5 read the skip reason from the diagnostic head only.
     Applied cleanly.
3. FB96's 56088d6 build-repair commit is already on master (upstream merged it); not
   cherry-picked.
4. Tests: cargo test -p zoder-cli -p acp-client --no-fail-fast. Known pre-existing
   baseline failures are acceptable; anything else is mine to fix.
5. Structured non-author review (nemotron), push to ARGONAS, ls-remote, report.

## Risks
- agentic.rs is huge and hot; FB97/FB95 landed since FB96 was authored - tests must
  re-prove the kept interplay (comment-prefix guards + truncation deferral).
- The truncated-diff test relies on the exact marker constant; if master renamed it,
  the test needs updating (it did not: marker present at agentic.rs:3243).
