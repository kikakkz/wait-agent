# AD-0002: Relay phase 1 — self-hosted CS star topology

- status: accepted
- date: 2026-10-01

## Context

Cross-site host interconnect (nodes behind NAT) needed a design.
Alternatives considered: direct dial only (status quo, fails across NAT),
full mesh, and CS star via a publicly reachable relay. Peer-to-peer NAT
traversal was evaluated as a phase-2 transport replacement, not a phase-1
requirement.

## Decision

Phase 1 is a client–server star: every node keeps one persistent
outbound TLS connection to a self-hosted relay; cross-node traffic
multiplexes over it as ordered reliable streams behind a `PeerConnection`
seam, so existing session protocols run unmodified and the relay only
ever sees frame headers (node-to-node inner TLS stays end-to-end).

Key sub-decisions, recorded 2026-09 (design doc is the full detail):

- Product positioning: self-hosted; the product name stays **relay**;
  a public cloud version (with its own naming, accounts, and billing) is
  explicitly deferred.
- Enrollment: invite-based (`relay invite` / `relay join`), device
  identity = self-signed certificate SPKI fingerprint, mutual pinning
  happens in one flow with no manual fingerprint transcription.
  Revocation = `relay remove <fingerprint>`.
- Liveness: heartbeat every 10s; three missed beats (30s) = offline.
- Management plane: a WebUI whose web service enrolls as a special node
  (browser terminal reuses OpenMirror/RawPty/Resize); CLI shrinks to the
  bootstrap minimum; trust model is self-hosted — the web service sees
  plaintext like today's console host; browser-side E2E is deferred with
  the public cloud version.
- Capacity: a calibrated single-relay capacity model (max_nodes,
  max_streams, throughput threshold) is built in phase 1 because it is
  the phase-2 cluster scheduler's input.
- Data policy: the relay forwards only and persists no session or traffic
  data.

## Consequences

- `docs/relay-design.md` is the governing design; the phase-1 split is
  tracked as GitHub issues under the "relay phase 1" milestone (AD-0001).
- Phase 2 candidates (not commitments): P2P via relay rendezvous, relay
  clusters with a capacity-based admission scheduler, public cloud with
  accounts/billing and browser-side E2E.

## Options rejected

- P2P as the phase-1 transport: much larger protocol and security scope
  before any relay-mode session exists.
- Cloud multitenant relay first: forces accounts, billing, and E2E
  decisions onto a product that has no self-hosted users yet.
