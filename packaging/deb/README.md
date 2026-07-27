# Debian packaging + APT publishing

zoder ships to the NCZ APT registry hosted on Buildkite Packages:

- Registry: `ncz-os/ncz` (Debian ecosystem, public)
- Browse: <https://buildkite.com/organizations/ncz-os/packages/registries/ncz>
- APT base URL: `https://packages.buildkite.com/ncz-os/ncz/`

## What is here

| File | Purpose |
|---|---|
| `build-deb.sh` | Wraps an already-built `zoder` binary into `zoder_<version>_<arch>.deb`. |
| `publish-deb.sh` | Uploads `.deb` files to the Buildkite Packages registry. |

There is no `debian/` source package and no `dpkg-buildpackage`. zoder is a
single self-contained Rust binary (rustls throughout — no OpenSSL linkage), so
the package is just the executable plus a copyright file. The binaries are
built natively per architecture by GitHub Actions, which is where the project's
native `x86_64` and `aarch64` Linux runners live.

## Wiring

`.github/workflows/release.yml` builds the release matrix, then a `publish-apt`
job packages the two Linux targets and uploads them. It runs when:

- a `v*` tag is pushed (always publishes), or
- the workflow is dispatched manually **with `publish_apt` set to `true`**.

A manual dispatch without that input builds and attaches the `.deb` files as
workflow artifacts but does **not** touch the live registry, so the packaging
can be exercised without shipping.

The job is additionally guarded on the `BUILDKITE_PACKAGES_TOKEN` secret being
present; if it is missing the job is skipped rather than failed, so an
unconfigured registry never turns a green release red.

### Versioning

- Tag build (`v0.2.1`) → deb version `0.2.1`.
- Manual dispatch → `0.2.1~dev<YYYYMMDD>.<short-sha>`. The `~` makes the dev
  build sort *below* the matching release in Debian version comparison, so a
  dispatch build can never shadow a real release for `apt upgrade`.

## Credentials

| Name | Where | Notes |
|---|---|---|
| `BUILDKITE_PACKAGES_TOKEN` | GitHub Actions repository secret on `ncz-os/zoder` | Write-capable Buildkite **API access token** (`bkua_…`). |

Buildkite has two credential types and they are not interchangeable:

- `bkua_…` **API access token** — can publish. This is the one the workflow needs.
- `bkrt_…` **registry token** — read/consume only, the kind an installed system
  puts in `/etc/apt/auth.conf.d/`. It returns an auth error on publish; that is
  by design, not a scope misconfiguration.

The org's write-capable token already exists in the fleet credential store
(`~/.api_keys_master.json` → `buildkite.api_access_token`). It only has to be
copied into the repository secret.

## Running it by hand

```sh
cargo build --release --locked --bin zoder --target x86_64-unknown-linux-gnu
packaging/deb/build-deb.sh \
  --binary target/x86_64-unknown-linux-gnu/release/zoder \
  --arch amd64 --version 0.2.1 --outdir dist

BUILDKITE_PACKAGES_TOKEN=… packaging/deb/publish-deb.sh dist/*.deb
```

`build-deb.sh` needs `dpkg-deb`, so run it on a Debian/Ubuntu host (or in a
container) — not on macOS.

## Consuming the registry

```sh
curl -fsSL https://packages.buildkite.com/ncz-os/ncz/gpgkey \
  | sudo gpg --dearmor -o /etc/apt/keyrings/ncz-os-ncz.gpg
echo "deb [signed-by=/etc/apt/keyrings/ncz-os-ncz.gpg] https://packages.buildkite.com/ncz-os/ncz/any/ any main" \
  | sudo tee /etc/apt/sources.list.d/ncz-os-ncz.list
sudo apt-get update && sudo apt-get install zoder
```

Note that the NCZ installer images point their kernel/CIX APT source at a
Cloudflare R2 bucket rather than at this registry (see `cix-installer`,
`post-install/24-apt-sources.sh`). That migration was about the *installer's*
unattended-download path; the Buildkite registry remains the org's published
APT home and is where zoder is served from.
