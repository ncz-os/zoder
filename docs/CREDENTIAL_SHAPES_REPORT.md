# Zoder non-Rust credential shapes report

Method: keyword-shape scan over the same non-Rust set as
`scripts/audit_nonrust_addresses.py` (tracked TOML/JSON/YAML/shell/Python/
Markdown/text/templates + shebang scripts; `*.rs` excluded). Shapes only —
file path, shape kind, count. No value is quoted.

Shape kinds: `bearer_token_assignment` (`bearer`), `token_assignment`
(`token =`/`:`), `api_key_assignment`, `password_assignment`,
`secret_assignment`, `private_key_block`.

Total: 14 files with shape hits.

| File | Shape kind | Count |
| --- | --- | --- |
| .github/workflows/container-weekly.yml | password_assignment | 2 |
| .github/workflows/native-builds.yml | token_assignment | 4 |
| README.md | bearer_token_assignment | 2 |
| config.enterprise.toml | bearer_token_assignment | 2 |
| config.enterprise.toml | token_assignment | 1 |
| config.ibm.toml | bearer_token_assignment | 4 |
| config.ibm.toml | token_assignment | 1 |
| config.microsoft.toml | bearer_token_assignment | 2 |
| config.oracle.toml | bearer_token_assignment | 4 |
| crates/agent-descriptor/schema/agent-descriptor.v1.schema.json | bearer_token_assignment | 2 |
| crates/agent-descriptor/schema/goose.descriptor.json | secret_assignment | 2 |
| crates/agent-descriptor/schema/zoder.descriptor.json | bearer_token_assignment | 2 |
| crates/agent-descriptor/schema/zoder.descriptor.json | secret_assignment | 1 |
| packaging/deb/publish-deb.sh | bearer_token_assignment | 2 |
| scripts/ci/corpus-sync-alert.py | token_assignment | 5 |
| scripts/ci/public-repo-guard.py | token_assignment | 3 |
| scripts/tests/corpus-sync-alert-tests.py | token_assignment | 7 |

Notes: hits are field-name occurrences in public templates, schemas, docs,
workflows, and guard code — not confirmed secrets. No remediation is done
by this task (report only). Public-repo rule observed: no credential or
internal address is quoted here.
