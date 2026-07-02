// build.rs — emit rustc-env values for the managed tool bundle so the
// `env!()` lookups in `gate_bundle.rs` resolve at compile time.
//
// This is the build-time pin: every managed tool's version lives here.
// Bumping a pin is a single-file change in this repo (plus a doc update
// in `docs/CI-PARITY-GATE.md`), and the test suite asserts the bundle
// stays in sync.

fn main() {
    // Each entry pins a current stable release of the managed gate tool.
    // Versions are reviewed at slice time; bumps go in a separate commit.
    println!("cargo:rustc-env=CARGO_DENY_VERSION=0.16.2");
    println!("cargo:rustc-env=CARGO_AUDIT_VERSION=0.21.4");
    println!("cargo:rustc-env=OSV_SCANNER_VERSION=v2.2.1");
    println!("cargo:rustc-env=GITLEAKS_VERSION=v8.28.0");
    println!("cargo:rustc-env=CYCLONEDX_VERSION=v1.9.1");
    println!("cargo:rustc-env=GOVULNCHECK_VERSION=v1.1.4");
    println!("cargo:rustc-env=PIP_AUDIT_VERSION=2.9.0");
}