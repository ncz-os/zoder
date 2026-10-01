# Daily trio builds

The active nightly is GitHub Actions [`native-builds.yml`](../.github/workflows/native-builds.yml), scheduled at **09:15 UTC daily**. GitHub may delay scheduled runs. It builds native macOS arm64, Linux arm64, and Linux x86_64 on hosted runners, then publishes the `zoder` / `zerocode` / `zeroclaw` trio to the GitLab `zoder-nightly/master` package channel and GitHub rolling `nightly` release.

## Source and acceptance

- Scheduled/default-branch runs resolve the latest canonical GitLab `ncz-os/zoder` **master** at start. The GitHub mirror must contain that revision for checkout to succeed.
- The engine is the latest canonical GitLab `ncz-os/zeroclaw` **master**, including the fork's integration changes. The separate ZeroClaw upstream CI tracks upstream master.
- Both full SHAs are resolved once and shared by all matrix legs. Every Cargo build uses `--locked`; dependency drift fails the build.
- Every archive contains `manifest.json`: exact repository/commit pairs, target, and run URL. The raw package channel also contains `manifest.json-<target>` plus its checksum.
- All three targets must succeed before publication begins. Existing GitHub downloads remain available during compilation. Uploads across the package service and GitHub are not an atomic transaction; publication errors fail the run and must be retried.
- Manual dispatch on a topic branch validates that branch and uploads Actions artifacts, but does not replace the master/nightly fleet channels. Fixes on a topic branch must be reviewed and merged to master before the scheduled nightly includes them.

## Verify a nightly

```sh
gh run list -R ncz-os/zoder --workflow native-builds.yml --limit 5
gh run view <run-id> -R ncz-os/zoder --log
# Manual latest-master build:
gh workflow run native-builds.yml -R ncz-os/zoder --ref master
```

Require completed success for all three build jobs and publication, and read the archive manifest. A green build of an old source revision does not prove a newer repair was shipped. A scheduled pipeline is not evidence of fleet installation; verify installed binary provenance separately.

## Other automation

GitLab's active `nightly master-of-the-day` schedule runs at **09:00 UTC**. It refreshes the model corpus and runs scheduled advisory/upstream checks. Per-push Rust gates remain in GitLab. Tagged/dispatch CLI builds are in `release.yml`; the weekly stack image is in `container-weekly.yml`.

`scripts/daily-build.sh` is a retired host-local fallback, not the nightly source of truth. HYDRA's former 04:00 cron is explicitly retired. Do not re-enable stale host cron instructions or assume a local build updates fleet installations. Current fleet pullers consume the canonical rolling channel independently.

## Preserve the rolling release through mirroring

The GitLab push mirror must set `keep_divergent_refs=true`. Otherwise its next sync can delete GitHub's `nightly` tag and silently turn a successfully built release into a draft, breaking public download URLs. Verify both the mirror's successful update timestamp and the public release after a sync:

```sh
glab api projects/ncz-os%2Fzoder/remote_mirrors
gh api repos/ncz-os/zoder/releases/tags/nightly --jq '{draft, tag_name}'
```

On 2026-10-01 the Sep 30 green nightly had six assets but was a draft. Mirror retention was corrected and that existing release republished. This setting is part of nightly operation, independent of compiler success.
