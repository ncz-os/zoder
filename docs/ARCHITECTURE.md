# Architecture policy: orchestration belongs in SOPs

ZeroClaw's SOP engine is the native model for new repeatable, multi-step orchestration. Its SOP steps provide scoped tools, output schemas, routing, and explicit failure policies such as `Retry { max }` and `Goto { step }`, so new orchestration primitives should be implemented as zeroclaw SOPs rather than as additional options or phases on zoder's `loop` command.

`zoder loop` remains supported as-is for compatibility and because it adds zoder-specific value beyond SOPs, including routing and cost accounting, reviewer-chain policy, check-subprocess safety, and diff-substance anti-gaming. This policy prevents the two orchestration mechanisms from growing in parallel and drifting apart.

