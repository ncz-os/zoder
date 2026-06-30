# Daily trio builds

Automated daily builds of the version-matched trio (`zoder` + `zerocode` +
`zeroclaw`) produced on the host that builds each target **natively**, then
published to ARGONAS. This avoids cross-from-macOS toolchain breakage (a pinned
`1.94.1-x86_64-unknown-linux-gnu` toolchain can't install on an arm64 Mac) and
does not depend on GitLab fleet runners being online.

## Host → target map

| Host | IP | Targets | How |
|---|---|---|---|
| ULTRA | 192.168.207.60 | `aarch64-apple-darwin` | native `cargo` (rust-toolchain 1.94.1) |
| ULTRA | 192.168.207.60 | `aarch64-unknown-linux-gnu` | native **arm64** `rust:1.94` Docker (Apple Silicon) |
| HYDRA | 192.168.207.78 | `x86_64-unknown-linux-gnu` | **amd64** `rust:1.94` Docker |

Linux targets build inside a pinned `rust:1.94` container so the release
toolchain matches the GitLab quality gate exactly. macOS binaries build natively
(no Docker for Mach-O).

The driver is [`scripts/daily-build.sh`](../scripts/daily-build.sh): it detects
the host, pulls the latest `main` of zoder, builds its target(s) via
`scripts/package.sh`, stamps the commit SHA, and publishes to
`ARGONAS:/mnt/datapool/zoder-releases/<YYYYMMDD>-<sha>/` plus a `latest/` mirror.

## One-time host setup

On **ULTRA** and **HYDRA**:

```sh
# 1. Secret env (NOT committed) — gitlab read token + the build role. Set the
#    role explicitly (hostnames are unreliable; ULTRA reports "MacBookPersonal").
#    ULTRA: ZODER_BUILD_ROLE=ultra   HYDRA: ZODER_BUILD_ROLE=hydra
cat > ~/.zoder-build.env <<'EOF'
export ZODER_PAT=glpat-xxxxxxxxxxxxxxxxxxxx
export ZODER_BUILD_ROLE=ultra
EOF
chmod 600 ~/.zoder-build.env

# 2. Cron — staggered so the two hosts don't both hammer the zeroclaw clone at once.
#    ULTRA at 03:30, HYDRA at 04:00 (local time). `package.sh` is on PATH via the
#    checkout; the script self-locates the repo under ~/zoder-daily.
#    ULTRA crontab line:
#      30 3 * * *  /bin/bash $HOME/zoder-daily/zoder/scripts/daily-build.sh >> $HOME/zoder-daily/daily-build.log 2>&1
#    HYDRA crontab line:
#      0  4 * * *  /bin/bash $HOME/zoder-daily/zoder/scripts/daily-build.sh >> $HOME/zoder-daily/daily-build.log 2>&1
```

On first run the checkout under `~/zoder-daily/zoder` may not exist yet; seed it
once with `git clone -b main https://oauth2:$ZODER_PAT@gitlab.com/ncz-os/zoder.git ~/zoder-daily/zoder`
(the script also self-clones if missing).

## Artifacts

```
ARGONAS:/mnt/datapool/zoder-releases/
  20260630-f32340a/
    zoder-0.2.1-aarch64-apple-darwin.tar.gz(.sha256)
    zoder-0.2.1-aarch64-unknown-linux-gnu.tar.gz(.sha256)
    zoder-0.2.1-x86_64-unknown-linux-gnu.tar.gz(.sha256)
    GIT_COMMIT
  latest/   # newest of each, overwritten daily
```

## Relationship to GitLab CI

`.gitlab-ci.yml` still defines `trio:*` package jobs tagged for fleet runners
(`fleet-x86`, `fleet-macos-arm64`, `fleet-linux-arm64`); they run on a tag push
or manually. Those require the fleet runners to be **registered and online** —
currently they are not (TYPHON was reimaged to Proxmox). This cron-based path is
the reliable daily mechanism and is independent of runner state. If the fleet
runners are brought back, a daily GitLab pipeline **schedule** plus a
`$CI_PIPELINE_SOURCE == "schedule"` rule on the trio jobs would mirror this in CI.
