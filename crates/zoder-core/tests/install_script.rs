//! Regression tests for `install.sh` (the canonical zoder installer).
//!
//! Two adversarial-review findings motivate this file:
//!
//!   * **Finding #12** — the installer seeded pricing to
//!     `$ZODER_HOME/data/pricing.json` while every CLI load site
//!     (`Config::home().join("pricing.json")`) read
//!     `$ZODER_HOME/pricing.json`. Fresh installs therefore shipped with an
//!     empty pricing catalog. Worse, failed seed downloads could leave a
//!     truncated JSON file at the canonical path, poisoning future runs.
//!
//!   * **Finding #13** — checksum verification failed open (a missing
//!     `.sha256`, an empty body, or no sha256 tool silently installed an
//!     unverified binary), installs were non-transactional (zoder landed
//!     before optional trio members), and a failing `zoder --version` smoke
//!     test was masked by `|| true`.
//!
//! Both classes of bug are now fixed in `install.sh`. These tests pin the
//! contracts so the regressions cannot return without a deliberate change.
//! All assertions fail-on-old-behavior (the buggy source) and pass-on-new.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    // tests/ runs from `crates/zoder-core/`, so the repo root is two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn read_install_sh() -> String {
    let p = workspace_root().join("install.sh");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()))
}

/// The CI pricing catalog lands at a single canonical path that BOTH
/// `install.sh` (writer) AND every `Config::home().join("pricing.json")`
/// loadsite (reader) agree on.
fn canonical_pricing_path() -> String {
    // Mirror Config::home() exactly: $ZODER_HOME else $HOME/.zoder, then
    // "pricing.json".
    let home = std::env::var("ZODER_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".zoder")))
        .unwrap_or_else(|| PathBuf::from(".zoder"));
    home.join("pricing.json").to_string_lossy().into_owned()
}

#[test]
fn installer_seeds_pricing_at_canonical_cli_path() {
    let src = read_install_sh();
    let canonical = canonical_pricing_path();

    // Old buggy behavior wrote to "$ZODER_HOME/data/pricing.json" — that
    // must not appear anywhere in the new source, or a fresh install
    // would still end up with a pricing catalog the CLI never reads.
    assert!(
        !src.contains("$ZODER_HOME/data/pricing.json"),
        "install.sh still references the wrong pricing path ($ZODER_HOME/data/pricing.json); \
         the CLI loads from $ZODER_HOME/pricing.json and the installer must match"
    );

    // New contract: pricing is written via `dl_atomic ... "$ZODER_HOME/pricing.json"`.
    assert!(
        src.contains("\"$ZODER_HOME/pricing.json\""),
        "install.sh must seed pricing.json to the canonical CLI path \
         ($ZODER_HOME/pricing.json); canonical for this env: {canonical}"
    );
}

#[test]
fn installer_seeds_corpus_at_canonical_cli_path() {
    let src = read_install_sh();

    // Corpus path: $ZODER_HOME/model_corpus.json (matches corpus_path: home.join("model_corpus.json")).
    assert!(
        src.contains("\"$ZODER_HOME/model_corpus.json\""),
        "install.sh must reference the canonical corpus path ($ZODER_HOME/model_corpus.json)"
    );
    // No spurious data/ subdir pollution.
    assert!(
        !src.contains("$ZODER_HOME/data"),
        "install.sh must not assume a $ZODER_HOME/data subdirectory exists; \
         the CLI never reads from there"
    );
}

#[test]
fn installer_uses_atomic_download_for_corpus_and_pricing() {
    let src = read_install_sh();

    // dl_atomic must exist and be the function used for both corpus + pricing seeds.
    assert!(
        src.contains("dl_atomic()"),
        "install.sh must define an atomic-download helper so a truncated curl body \
         can't leave a half-written file at the canonical pricing/corpus path"
    );
    // Used at least twice (corpus + pricing).
    let uses = src.matches("dl_atomic ").count();
    assert!(
        uses >= 2,
        "dl_atomic must be used for both corpus and pricing seeds (found {uses} uses)"
    );
}

#[test]
fn checksum_verification_fails_closed_for_required_binaries() {
    let src = read_install_sh();

    // The buggy code warned ("no checksum published for ${asset}; skipping verify")
    // and installed anyway when NO_VERIFY was unset. The fix must `die` on a missing
    // checksum unless NO_VERIFY=1 was explicitly requested.
    assert!(
        src.contains("refusing to install unverified binary"),
        "a missing .sha256 for a REQUIRED binary must die (not warn-skip); the \
         verification path must not fall through to an unverified install"
    );
    // A required-binary checksum that doesn't match the 64-hex-char contract
    // must die too — both the empty-file body case and the random-trash case.
    assert!(
        src.contains("malformed checksum for ${asset}"),
        "checksum that isn't exactly 64 hex chars must abort the install"
    );
    // No sha256 tool available AND verification required must be fatal (regression
    // on the old `warn "no checksum tool available; installed ${b} without verification"`).
    assert!(
        src.contains("no sha256 tool available on this system; cannot verify"),
        "missing sha256 tool with verification enabled must be fatal, not warn-and-install"
    );
    // And the old non-fatal skip is gone.
    assert!(
        !src.contains("installed ${b} without verification"),
        "old non-fatal `installed ${{b}} without verification` branch must be removed"
    );
}

#[test]
fn installer_is_transactional_required_then_optional_then_smoke() {
    let src = read_install_sh();

    // Two-phase staging: the source must iterate BIN_REQUIRED first, then
    // install AFTER all binaries (required + optional) are verified. The
    // old code mixed install-time with verify-time, so a partial failure
    // left a half-installed trio on disk. Confirm the staging-then-install
    // order is present.
    let req_idx = src.find("for b in $BIN_REQUIRED").expect("required loop");
    let opt_idx = src.find("for b in $BIN_OPTIONAL").expect("optional loop");
    let install_idx = src.find("install -m 0755 \"${tmp}/").expect("install line");
    let smoke_idx = src
        .find("\"${BIN_DIR}/zoder\" --version")
        .expect("smoke line");

    assert!(
        req_idx < opt_idx && opt_idx < install_idx && install_idx < smoke_idx,
        "install order must be: required → optional → install → smoke \
         (req={req_idx}, opt={opt_idx}, install={install_idx}, smoke={smoke_idx})"
    );
}

#[test]
fn smoke_test_is_fatal_and_rolls_back() {
    let src = read_install_sh();

    // Old: `"${BIN_DIR}/zoder" --version 2>/dev/null || true`. New: smoke
    // failure must trigger a `die` and a rollback.
    assert!(
        !src.contains("--version 2>/dev/null || true"),
        "the smoke test must not be masked by `|| true` — a broken \
         `zoder --version` must abort the install"
    );
    assert!(
        src.contains("install was rolled back"),
        "on smoke failure the installer must roll back the newly-installed binaries"
    );
    assert!(
        src.contains("Smoke verified:"),
        "successful smoke must print a clear verification line"
    );
    assert!(
        src.contains("backup-${b}") && src.contains("rollback_install"),
        "rollback must restore pre-existing binaries, not merely delete them"
    );
    let smoke = src
        .find("Smoke verified:")
        .expect("successful smoke marker");
    let commit = src[smoke..]
        .find("transaction_active=0")
        .map(|offset| smoke + offset)
        .expect("transaction commit after smoke");
    assert!(
        commit > smoke,
        "the install transaction must remain active through seeding and smoke"
    );
    assert!(
        src.contains("seeding failed; install was rolled back"),
        "a corpus/pricing seed failure must abort while rollback is active"
    );
}

#[test]
fn package_script_fetches_commit_sha_after_branchless_clone() {
    let path = workspace_root().join("scripts/package.sh");
    let src = std::fs::read_to_string(&path).unwrap();
    let goose = src.split("ensure_goose() {").nth(1).expect("ensure_goose");
    let goose = goose.split("package_target() {").next().unwrap();
    assert!(
        !goose.contains("git clone --depth 1 -b")
            && !goose.contains("git clone --depth 1 --branch"),
        "ensure_goose must not pass an arbitrary commit SHA to git clone --branch"
    );
    assert!(
        goose.contains("git fetch -q --depth 1 origin \"$GOOSE_REF\"")
            && goose.contains("git checkout -q FETCH_HEAD"),
        "ensure_goose must fetch and check out the exact pinned ref"
    );
    let zeroclaw = src
        .split("ensure_zeroclaw() {")
        .nth(1)
        .expect("ensure_zeroclaw")
        .split("ensure_goose() {")
        .next()
        .unwrap();
    assert!(
        !zeroclaw.contains("git clone --depth 1 -b")
            && zeroclaw.contains("git fetch -q --depth 1 origin \"$ZEROCLAW_REF\"")
            && zeroclaw.contains("git checkout -q FETCH_HEAD"),
        "ensure_zeroclaw must retain the same SHA-safe clone/fetch/checkout flow"
    );
}

#[test]
fn checksum_parser_requires_exact_digest_body() {
    let src = read_install_sh();
    assert!(src.contains("read_checksum"));
    assert!(src.contains("*[!0-9a-fA-F]*"));
    assert!(
        !src.contains("tr -cd '0-9a-fA-F'"),
        "checksum verification must not manufacture a digest by stripping arbitrary response text"
    );
}
