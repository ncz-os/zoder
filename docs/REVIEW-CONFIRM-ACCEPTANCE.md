# Confirm-on-reject acceptance

Measured on 2026-10-26 against the recorded offline review evaluation.

The evaluation itself is private: the corpus, the reviewer transcripts and the
confirmer answers live outside this repository. What is in this repository is
the part that can be re-checked by anyone — the grounding replay fixture
(`crates/zoder-cli/tests/data/grounding-replay-20261026.jsonl`) and the test
that drives the production rules over it.

## The target

| criterion                                  | target    |
|--------------------------------------------|-----------|
| false blocking findings stopped            | at least 80% |
| real findings lost                         | at most 2 |
| real findings lost to a timeout            | 0         |

A finding is **stopped** when it no longer blocks the change: either the
grounding pre-check disproved it, or a confirmer rejected it and the rejection
rule demoted it. A finding is **lost** when a real defect stopped blocking.

## What was replayed

Two recorded sets from the same evaluation:

- the **grounding replay**: 137 blocking findings (73 false, 62 real, 2 other);
- the **confirmation replay**: 92 blocking findings (44 false, 46 real, 1 nit,
  1 valid), with the answer each of two independent confirmer families gave for
  every finding.

The replay applies the production rules, not an intent: the grounding rule
(`Grounding::grounded`), the pre-check (`precheck`), the settlement rule
(`settlement`), the driver's rounds (the first round asks two confirmers at the
same time, a later round asks one tiebreaker, and a later round is only entered
while the finding is still unsettled), the per-review budget of 8 calls, and
the two-independent-rejects rule for `critical`.

## Result

| chain                                   | false stopped        | real lost | real kept blocking after a failed confirmer |
|-----------------------------------------|----------------------|-----------|--------------------------------------------|
| grounding pre-check alone (137 findings) | 15 / 73 = 20.5%      | 0 / 62    | 0 |
| the stricter confirmer family, alone     | 36 / 44 = 81.8%      | 1 / 46    | 0 |
| the more permissive family, alone        | 30 / 44 = 68.2%      | 0 / 46    | 4 |
| both families                            | 33 / 44 = 75.0%      | 0 / 46    | 2 |
| both families, no budget cap             | 36 / 44 = 81.8%      | 1 / 46    | 3 |

**The target is met with one confirmer family**: 81.8% of the false blocking
findings stopped, 1 real finding lost, and no real finding lost to a timeout or
any other failed call — a failed confirmer keeps the finding blocking.

**The target is not met with two families under the default budget of 8 calls
per review**: 75.0%.

## Why a second family lowers the rate

The second family is more permissive: on its own it confirmed 7 of the 44 false
findings and rejected only 26 of them. In the first round the driver asks two
confirmers at the same time, so a review with more than four findings runs out
of budget — with both families, 5 false findings and 34 real findings were
never reached at all. The rate moved because the budget moved, not because the
rule became safer.

The `0 real findings lost` of the two-family row is that same effect seen from
the other side: the one real finding the stricter family rejects wrongly is not
reached before the budget runs out. With the budget uncapped it is reached,
rejected by both families, and demoted — the loss is back.

The two-family row therefore buys nothing here. The chain is not shorter than
one family, and the budget it spends is the budget that would otherwise have
reached the findings that were never confirmed.

## The one real finding that is lost

A `high` finding in the seeded case: the reviewer reported a one-byte overrun
in a buffer write. The stricter family disagreed, arguing the buffer is
allocated with `n + 1` bytes and the loop writes exactly the indices it holds.
That answer is wrong — the defect is a real out-of-bounds write — so a real
defect was demoted by one independent reject, which is exactly what the rule
asks of a `high`.

The two-reject rule is what protects a `critical`: five of the nine real
rejections the stricter family made were `critical`, and none of them was
demoted without a second, independent reject. That protection costs a second
family, and a second family is what the budget above cannot afford.

## What the grounding rule does on its own

The grounding pre-check runs before any model call and costs nothing. Over the
137 recorded findings it drops 15 of the 73 false findings (20.5%) and loses
none of the 62 real ones. The rule was chosen on those numbers: a stricter
variant that drops a finding whose cited line is out of range but whose path
exists stops fewer false findings, and a stricter variant that drops on any
missing span stops 20 false findings while losing 5 real ones.

The 15 dropped false findings break down as 14 whose cited place does not
exist at the reviewed head (a path that was never there, or a line past the end
of the file) and 1 whose every quoted code span occurs nowhere in the reviewed
text.

The replay is in the tree and is driven through the production path — the
location parser, the span extractor, the grounding check and the pre-check —
so the numbers above are re-derived by `cargo test` rather than asserted in
prose. Six mutations of that path each make the test fail: a strict grounded
rule, an absent path accepted, a pre-check that drops nothing, the cited path
taken from the last colon, a longer span floor, and every second span skipped.
