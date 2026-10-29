# Zoder non-Rust address audit

Base: `master` (`01a0dd7` lineage). Scope: non-Rust files only — docs,
scripts, config templates, TOML/JSON/YAML/shell (plus Markdown/text).
Rust sources (`*.rs`) are never scanned and cargo is never run.

Tool: `scripts/audit_nonrust_addresses.py` (stdlib only; same ratchet design
as the fleet-ops audit). Baseline:
`scripts/nonrust_address_baseline.json` — 1 file, 2 occurrences.
Guard: `scripts/test_audit_nonrust_addresses.py` — 11 tests.

- `python3 scripts/audit_nonrust_addresses.py --check`:
  `OK: 1 files, 2 occurrences, none over baseline` (exit 0).
- `python3 -m pytest scripts/test_audit_nonrust_addresses.py -q`:
  `11 passed`.

No production file is changed by this audit. No Rust file is touched.

## Counts per file

| File | Count | Class |
| --- | --- | --- |
| scripts/ci/public-repo-guard.py | 2 | fixture (self-test sample strings) |

Details: the two occurrences are synthetic sample inputs inside the
`--self-test` path of the public-tree guard itself (its own unit check for
private-address detection). They are not live configuration, not examples
for operators, and not stale references — they exist only so the guard can
prove it fires. All other non-Rust files (config templates, docs, scripts,
workflows) contain zero RFC1918 occurrences.

Rust exclusion proof: `test_rust_sources_are_never_scanned` places a LAN
address in a scratch `x.rs` and asserts the scanner ignores it and
`--check` still passes.

Tool notes: Markdown is in scope here (unlike the fleet-ops audit) because
the brief asks for docs; binaries and NUL-containing files are skipped;
extensionless shebang scripts are included; public/reserved ranges are
ignored (covered by
`test_each_private_range_is_detected_and_public_ones_are_not`).

## Guard firing proof

- `test_tree_is_within_baseline` runs `--check` on the real tree (exit 0).
- `test_guard_fires_when_a_file_gains_an_address`: copies real
  address-free `install.sh` to scratch, asserts pass, appends one RFC1918
  address, asserts exit 1 with filename and no value echoed.
- `test_guard_fires_when_a_baselined_file_gains_one_more`,
  `test_removing_addresses_never_fails`, `test_write_baseline_round_trips`,
  `test_missing_baseline_is_an_error_not_a_pass` cover the ratchet
  semantics; shebang/binary/range tests cover scope.

Public-repo hygiene: this report describes files and counts only. It quotes
no address and no credential.
