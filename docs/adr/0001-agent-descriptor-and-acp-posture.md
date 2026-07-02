# ADR 0001 — ACP posture + vendor-neutral Agent Descriptor standard

Status: Accepted (2026-07-02) · Scope: zoder / zerocode / zeroclaw · Owner: Jason Perlow

## Context

zerocode is a separate effort from zeroclaw. The wider agent ecosystem
(goose, Gemini CLI, Zed, etc.) is standardizing on **ACP** (Agent Client
Protocol, agentclientprotocol.com) — goose is making ACP the *primary*
interface for all its clients (desktop, CLI, beyond) and ships a `goose-acp`
crate. We evaluated whether zoder should (a) become a pure ACP client,
(b) launch a competing "supplantive" standard, or (c) something else.

### Validated facts (not assumptions)
- **WSS is not a rival to ACP — it is an ACP transport.** ACP is JSON-RPC over
  **stdio** (local) OR **HTTP/WebSocket** (remote; still maturing). "WSS+ACP
  unifying" is imprecise: it is simply "ACP over WSS." zeroclaw already hangs an
  ACP-over-WSS route off its daemon and supports ACPv1 + a draft RFD.
- **ACP is editor→agent scoped** ("the user is primarily in their editor")
  and **deliberately does NOT standardize configuration.** goose has this as an
  open, unsolved problem (block/goose discussions #7309, #4645, #4652, #6642,
  #7697): "the current goose config system needs a standardized way for clients
  to interact with it through ACP."
- A *pure* ACP client cannot be good today (tidux/Shane built zerocode because
  ACP was ~5% of what they needed).

## Decision

1. **Do NOT supplant ACP.** A rival standard from a small team loses to an
   incumbent with editor-vendor adoption (the LSP precedent). It also reverses
   our own recorded "not a special snowflake" decision.
2. **zoder/zerocode = reference SUPERSET that speaks ACP at the boundary.**
   ACP client for interop (drive goose/gemini/etc.); native zeroclaw surface
   (daemon RPC / WSS route + RFD extensions) for the other ~95%.
3. **Author a vendor-neutral Agent Descriptor standard** for the layer ACP
   omits: **(a) config surface** (knobs, types, defaults, secret policy) and
   **(b) connection descriptor** (transport, endpoint, auth, ACP-capable?).
   Complements ACP; does not compete. Modeled on **MIF** (mif-spec.dev — a
   vendor-neutral data model, static JSON Schemas with stable `$id`, reference
   crate, conformance levels; Jason is a MIF contributor). Same playbook, new
   domain.
4. **Static-first, codegen from source — no runtime-discovery chatter.** Derive
   the descriptor from zeroclaw config structs at build time (schemars-style).
   Same-source clients (zerocode<->zeroclaw) keep compile-time knowledge (zero
   chatter); foreign clients read the published descriptor once. Runtime
   discovery stays scoped to the already-dynamic subset (MCP config, live
   tools), which the descriptor *points to* rather than enumerates. Precedent:
   LSP `initialize` capability exchange (once), `package.json` (static
   manifest). Novel value vs ACP/LSP: **config knowledge BEFORE connecting.**
5. **Publish the Zeroclaw interface as a Rust crate** (tidux/Shane plan) = the
   reference implementation of the connection half.
6. **Upstream config-over-ACP as RFDs** into ACP where they overlap, rather than
   forking. The config surface is ACPs acknowledged gap → net-new, not xkcd-927.

## Next POC (goose first)

Phase 1 — prove locally (no goose PR yet):
- Draft the minimal descriptor schema (connection + config surface, 2
  conformance levels), codegen the zeroclaw descriptor from its config structs.
- Hand-author a goose descriptor: connection = ACP-over-stdio (`goose acp` /
  `goose-acp` crate); config surface = provider/model/env + `config.yaml`;
  recipes/scheduling as goose-specific extension fields.
- **zoder consumes both descriptors and drives goose + zeroclaw uniformly**
  (reuses the goose-engine work). Two conformant implementers = a real standard.

Phase 2 — engage goose upstream (discussion -> PR, in that order):
- Post the descriptor proposal into the existing goose ACP/config discussions,
  citing MIF as prior-art pattern (standing as a MIF contributor).
- If welcomed, PR a `goose` self-describe emitter (config-surface descriptor in
  the shared schema). Small, additive, ecosystem-wide value.
- Posture: fill goose PR template, no AI footer, Jason Perlow <jperlow@gmail.com>,
  human-paced (<=3-4 PRs/upstream/day), lead with "config-over-ACP for all ACP
  hosts," not "adopt my standard." Do NOT PR before Phase 1 proves the
  round-trip with a working consumer.

## Consequences
- zoder gains a clean multi-engine story: ACP client + per-engine config via
  descriptors (answers "how to support other agents config surfaces").
- We become a leader IN the standard (config layer) rather than a challenger to
  it — leveraging MIF credibility.
- Sequenced AFTER current zoder debt (CI-parity gate, oneshot/redeploy, billing).
