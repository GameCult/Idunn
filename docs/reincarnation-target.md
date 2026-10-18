# Reincarnation after reboot: target

Status: target, Self, 2026-10-18 (campaign `idunn-reincarnation` in Eureka's
mind). It states the ends. The body facts, model page and cut sequence will
live in `reincarnation-map.md`.

## Why

Yggdrasil rebooted at 11:46 UTC on 2026-10-18. Admitted bodies run as
transient systemd units under `/run/systemd/transient`, and the reboot wiped
them all. nginx failed at boot, because it bound `10.77.0.1:17871` before
`wg-gamecult` carried that address. Every stable route, Idunn's own
`127.0.0.1:8831-8833` included, stayed down until a manual
`systemctl restart nginx` at about 19:46.

What Idunn did:

- From 11:50 onward continuity spent streampixels-service's whole restart
  budget (6 per hour) and logged "The target is free for a deployment to
  replace it: systemd unit … is absent". The log repeated in hourly bursts as
  the window slid.
- When nginx returned, ghostlight and streampixels-web were re-incarnated, and
  streampixels-service was not.
- An operator-approved `idunn up streampixels-service` (tx-e69f5402) sat in
  Sealing for about 15 minutes. It then failed with "admitted incumbent failed
  before candidate fencing; deployment yielded to continuity". Continuity had
  freed the target for a deployment. The deployment handed it back to
  continuity. Neither actuated.
- At 20:46:43 continuity re-incarnated the service under generation
  tx-0804185a, once the window had slid to 4/6. So the outage was not ended by
  the deployment the operator approved. The restart window bounded it, a full
  hour after its cause had cleared.
- Throughout, several times a second: "preserved admitted … after rejecting
  topology observation: topology correlation does not bind the current
  activation", and "rejected admitted streampixels-service route continuity:
  admitted process write lease is no longer exact".

## Invariants

1. **A reboot is not a fault of the body.** If an admitted body's unit is
   absent because the host lost its transient units, continuity
   re-materializes it from the installed body. It needs no deployment, no
   operator, and no part of the restart budget for that absence.
2. **The restart budget measures the body.** A failure caused by a shared host
   dependency, such as nginx, the stable route's listener or Odin, never
   charges a target's restart budget. A dependency outage can never exhaust
   the budget of every target behind it.
3. **Exactly one authority owns a target's actuation at every instant, and
   the handover is explicit.** When continuity stops, it records that it has
   stopped, and from then on deployment owns the target. A deployment never
   yields to a continuity owner that has stopped. A deployment whose incumbent
   is absent replaces it, with nothing to fence. There is no reachable state
   in which continuity waits for a deployment while that deployment waits for
   continuity. A negative test proves it.
4. **Continuity is daemon survival; deployment is change** (`F:\Projects\CLAUDE.md`).
   Restarting the admitted installed body never needs a deployment and is
   never gated by a deployment brake. A deployment brake gates only artifact,
   revision, configuration, schema, unit or authority changes.
5. **Rejections of stale observations are not a log stream.** An observation
   that cannot bind the current activation is either resolved by the owner
   that can refresh it, or reported once as an incident. It is never
   re-rejected several times a second.
6. **Existing invariants hold.** `route-continuity-target.md` 1 to 7 (in
   particular no actuation from observation, bounded backoff, every
   post-fencing phase ends) and `authority-map.md` (Idunn's own recovery
   depends on no managed target).

## Scope

In:

- Idunn's continuity of admitted bodies across a reboot: absent units, the
  restart budget and what charges it.
- The handover between continuity and deployment, in both directions.
- Classifying the topology-correlation and write-lease rejections: the same
  fault, or a separate one with its own cut.
- Re-admitting streampixels-service on Yggdrasil. It is running again under
  tx-0804185a, and the closing proof checks that it survives the fix's
  install.

Out, and not consumers of this change:

- **nginx's boot order on Yggdrasil.** It is host configuration, owned by
  `gamecult-ops`. It gets its own operator-approved change and record, not an
  Idunn cut.
- **Ghostlight's degraded route** (53 consecutive failures, incident open) is
  in scope only if the map shows the same fault. Otherwise it is a follow-up.
- **Heimdall's onboarding failures and its environment.**
- **Retention of the ~130 dead transient units** on Yggdrasil. It is an ops
  follow-up already recorded under route continuity.
- **Luna's 2026-09-27 weekend patches** are not foundation to build on. Where
  this map meets them, it cuts them.
