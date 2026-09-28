# Route continuity and admission: target

Status: target, Self, 2026-09-29. The cut map will live in
`route-continuity-cut.md` once Imagination has produced it. This document states
the ends. It does not specify the means.

## Why

On 2026-09-27 and 28 an agent spent 24 hours failing to deploy StreamPixels'
web app and service through Idunn v2. The live evidence is on Yggdrasil:

- Admitted route continuity rewrites and reloads nginx whenever a route
  observation is older than 30 s. The config is usually byte-identical when it
  does so (`supervise_admitted_route` → `restore_admitted_membership` →
  `reload`, since `391a7e2` on 2026-09-04). The result is ~115 reloads/h per
  routed target when healthy. Once the StreamPixels routes were failing it
  reached ~3,000/h, and 475 nginx workers were left shutting down.
- Every reload moves nginx's UDP listener for Odin (`10.77.0.1:17871` →
  `127.0.0.1:17973`) to new workers. That breaks the CultNet RUDP sessions of
  Idunn's route observer and of every runtime-presence publisher. The
  timeouts fail the route checks, and the failed checks cause more reloads.
  Odin continuity restarts followed on 2026-09-28 at 16:26 and 21:27. Each
  left Odin refusing every provider until its new lease was picked up.
- The web app could not be admitted without the shared Odin RUDP path. It
  published signed presence to Odin, and its HTTP snapshot route returned 503
  whenever that publication timed out.
- The web transaction stayed in `Committing` for more than 12 hours. After
  fencing nothing has a deadline: errors are resumable forever,
  `AwaitingReady` waits forever, and no command recovers a continuity or
  stateful transaction.
- The weekend fixes worked around symptoms. `a148802` relaxes rollback to
  demote whatever activation is projected, and `9f00e7a` authenticates Ready
  and provider receipts at admission time instead of now.

## Invariants

1. **Observation never actuates routing.** Refreshing a route observation is a
   challenge through the stable endpoint and nothing else. Idunn writes an nginx
   fragment or reloads nginx only when the fragment on disk differs from the
   admitted or candidate membership, or when a route is installed or withdrawn.
   A healthy steady state performs zero reloads.
2. **Failure never accelerates actuation.** Repairing a failed route or unit is
   bounded and backs off. No failure mode can raise the reload rate or the
   restart rate above a fixed ceiling per target.
3. **A web app proves itself over its own HTTP route** (operator,
   2026-09-29: "web apps prove over HTTP; Odin can observe a web app, but a web
   app has no reason to be aware of Odin"). Its admission and continuity depend
   only on Idunn, its own stable HTTP route, and its declared host inputs. A web
   app needs no Odin client, no RUDP publisher and no Odin trust anchor. Odin
   may observe it. Observation is never admission authority.
4. **Route proof binds the exact incarnation and is fresh.** Traffic reaches only
   the admitted incarnation. The proof answers an Idunn challenge and is signed
   with the credential Idunn issued to that launch. Invariant 3 changes where the
   proof travels, not what it proves.
5. **Every post-fencing phase ends.** Each phase after fencing has an admitted
   deadline. When a deadline expires, Idunn resolves it: stateless targets abort
   to the incumbent. Stateful targets get an explicit terminal record with a
   named recovery. A granted lease that the process does not pick up within its
   window is a named failure with an Idunn-owned recovery. It is never a silent
   wait. Resumable-forever is gone.
6. **The admitted receipt describes the running incarnation.** Continuity
   commits update the admitted generation atomically, so the rollback path never
   meets drift between the receipt and the projection. Rollback demotes the exact
   admitted activation (`a148802` reverted). Readiness and provider checks
   authenticate current evidence at the current time (`9f00e7a` reverted).
7. **Idunn stays independent** (existing, `authority-map.md`): Idunn starts and
   recovers its own admitted state without Odin or any managed target.

## Scope

In:

- Admitted route supervision.
- nginx route actuation.
- The HTTP route proof for web targets.
- Post-fencing deadlines and resolution.
- Stuck-lease recovery.
- Reverting `a148802` and `9f00e7a`.
- Reviewing the other weekend commits: `ae8ac45` and `29946bb` in particular.
- The app-side cost of invariant 3 in CultLib's Idunn runtime-authority
  packages.
- The StreamPixels web binding that proves invariant 3 end to end.

Out, and not consumers of this change:

- **The verify-transaction campaign** (`verify-transaction-cut.md`, branch
  `idunn/cut1-fix5`). It is a separate foundation. Coordinate merges, do not
  bundle.
- **Heimdall's failed Idunn onboarding.**
- **The Windows host actuator.**
- **Operator-owned nginx settings** (`worker_shutdown_timeout`) and host
  retention of dead transient units and activation directories. These are ops
  follow-ups recorded in the map.
- **StreamPixels' public cutover.** It resumes after this lands, through the
  ordinary path.
