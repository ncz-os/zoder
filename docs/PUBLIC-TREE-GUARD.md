# Public-tree guard

`scripts/ci/public-repo-guard.py` fails a pipeline when an internal
identifier appears in either:

- any file tracked by git in the checkout, or
- any commit message unique to the branch being tested (`BASE..HEAD`).

The repository is public, so the guard must work without spelling the names it
blocks. `scripts/ci/public-repo-denylist.txt` therefore contains only salted
SHA-256 digests. The script normalises text to lowercase words, then compares:

| Rule | Matches |
|---|---|
| `token` | one normalised word |
| `bigram` | two adjacent words, which catches dotted host names |
| `prefix` | an IPv4-like address prefix |
| `host` | a DNS host, or one of its labels/suffixes, seen in a URL or an `ssh`/`scp` destination |

Common words are never registered as plain tokens. A word that is harmless in
prose but unacceptable as a host name (for example a machine role) is added
with `--hash-host`, so `https://role.example.invalid` and `user@role` match
while ordinary prose does not.

## Adding a term

From the repository root:

```sh
# A distinctive word: prints the token digest and any adjacent bigrams.
python3 scripts/ci/public-repo-guard.py --hash 'provider-name' \
  >> scripts/ci/public-repo-denylist.txt

# An address prefix (for example a LAN /24).
python3 scripts/ci/public-repo-guard.py --hash-prefix '198.51.100' \
  >> scripts/ci/public-repo-denylist.txt

# A host name or a machine label that is safe in prose.
python3 scripts/ci/public-repo-guard.py --hash-host 'internal.example.invalid' \
  >> scripts/ci/public-repo-denylist.txt
```

The helper prints digests only; do not paste the original term into the file or
into a comment. Commit the updated denylist and run the guard before pushing.

## Running it

```sh
python3 scripts/ci/public-repo-guard.py --self-test
python3 scripts/ci/public-repo-guard.py --base origin/master
```

`--self-test` proves that both the tree scan and the commit-message scan fire on
an obviously-fake canary term. It builds that term at runtime, so the canary is
never a contiguous string in the tracked tree. The GitLab `public-repo-guard`
job runs the self-test and then scans the branch on every push and merge
request.

When the guard fails it prints the file (or commit SHA), line, and rule kind,
but never echoes the matched text.
