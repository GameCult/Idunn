# Deployment provenance instead of an approval gate: target

Status: target, Self, 2026-10-19 (campaign `idunn-deploy-provenance` in
Eureka's mind). It states the ends. The map will live in
`deploy-provenance-map.md`.

## Why

Operator, 2026-10-19:

> The rule about the deployment brake was instituted by a cautious agent, but
> it doesn't actually suit how GameCult operates. The whole point of Eureka is
> to give the Mind, Persona and Self agency over its own Body. It's requiring
> my approval for every deployment that violates our principles. Yes, we
> certainly want to know which agent approved the deployment and why (this is
> the authority provenance which we might want to add to the transaction), but
> that's not the same as gating every deploy on my approval.

On the same day a StreamPixels deploy (tx-b40c7240) waited on the brake
because only the root-held operator identity could release it. The agent
running the deploy could neither proceed nor record why it should.

`F:\Projects\CLAUDE.md` says a deployment brake *may* gate artifact, revision,
configuration, schema, unit or authority changes. It does not say that
operator approval is the gate.

## Invariants

1. **No deployment waits on a per-transaction operator approval.** An agent
   acting for the Mind, Persona or Self may admit a deployment of a target it
   is authorized for.
2. **Every transaction carries typed authority provenance.** That is: who
   approved it (agent or instance, and faculty), under which session or
   ruling, and why. It is written before Fencing, it is immutable, and
   `idunn status` and history show it. It is part of the transaction
   document, not a log line.
3. **A brake that survives is a stop, not a gate.** The operator or an
   authorized agent can halt deployment of a target, and the halt is itself
   provenance-bearing. The deployment brake and the continuity lifecycle brake
   stay separate (`F:\Projects\CLAUDE.md` Infrastructure). Neither gates Idunn
   itself, and the deployment brake never gates continuity.
4. **Existing invariants hold.** These are `idunn-reincarnation` target r2,
   `route-continuity-target.md` 1-7 and `authority-map.md`.

## Scope

In:

- The deployment-brake release path in Idunn and `idunn-provision`.
- The provenance record on the transaction.
- `idunn status` and history projection of the provenance.
- The gamecult-ops runbooks that describe a per-transaction operator release
  (`heimdall-discord-launch.md`, `idunn-host-raven.md`, the StreamPixels
  runbook).

Out:

- `idunn-reincarnation`. This campaign is mapped after that campaign's cut 2
  merges, and coordinates with it rather than bundling.
- Harness permission rules on the workstation, which are the operator's.
- Heimdall's onboarding failures.
