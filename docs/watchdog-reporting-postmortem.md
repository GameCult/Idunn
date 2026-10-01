# Watchdog reporting postmortem

Campaign `idunn-watchdog`, 2026-09-30 to 2026-10-01. This was the first Eureka
campaign whose state was typed in Huginn's mind (instance `eureka`) rather than
kept in a prose map. Document ids below are `idunn-watchdog:<kind>:<local>`.
Read them with `view`; this page does not restate them.

## Summary

Idunn is now a watchdog that reaches the operator on Discord.

- **Idunn** opens a durable incident at the site where a continuity budget is
  exhausted. The incident is written to `incidents.cc`, a world-readable file
  in its projection directory with a private 0600 lock, and closes when the
  target runs again and its restart window is no longer exhausted.
- **Bifrost's watchdog-notice reader** runs from a one-minute timer. It sends
  one opening DM and at most one closure DM per incident, keeps its own
  journal, and retries a failed send forever with capped backoff.
- **Scale:** 8 cuts across Idunn, Bifrost and gamecult-ops: 13 spec revisions,
  17 Hands reports, 17 Soul verdicts and 55 findings. Measured against the
  in-force estimates:

| Repo | Estimate (lines) | Landed (lines) |
|---|---|---|
| Idunn | +930/−101 | +2618/−354 |
| Bifrost | +246/−335 | +1182/−429 |
| gamecult-ops | +280/−35 | +505/−104 |

- **Installed on Yggdrasil:**
  - Idunn `a909f93` at 2026-09-30 23:21 UTC, then `cb60965` at 2026-10-01
    02:02 UTC.
  - The Bifrost reader at `160fac20` with CultLib `36ea08d3`, at 2026-10-01
    08:04 UTC.
- **First live notice:** the incident's first real input was raven-muninn,
  which exhausted its budget minutes after the incident cut was installed. Its
  opening DM was delivered at 08:04:41 UTC, and the operator confirmed it.
- **Still owed:** the closure DM for that same incident
  (`follow_up:closure-dm-observed`). It depends on raven-muninn recovering, and
  raven-muninn's own fault lies outside this campaign.

## Scope and invariants

Target `r4`. Its invariants are:

- `incident-owner`
- `record-before-delivery`
- `delivery-owner`
- `survival-independent`
- `one-per-incident`
- `no-sensitive-egress`
- `tick-lock-private`

The operator directions are `ruling:operator-watchdog-direction` and
`ruling:operator-bifrost-bridge`. The second made Bifrost the bridge to
Discord, so Idunn holds no credential and speaks no Discord API.

Excluded:

- **Odin's store condition and B5 `OperatorRequired`.** Neither can page yet.
  Odin's O2 cut is unbuilt, and B5 has no operator exit
  (`follow_up:b5-operator-required-no-exit`).
- **VoidBot's `notify_owner`.**
- **The Persona Discord crossing.**
- **Eve lowering.**

The earlier attempt was Bifrost `c063f27` (2026-09-04): an alarm was a command
written into Bifrost's store. It never ran, because no pump was deployed and
the deployed pump lacked the DM verb. It was also reversed by ruling:
`ruling:delivery-crossing` has Bifrost read Idunn's projection, so no write
crosses an owner boundary.

## Timeline

| Cut | Reports | Soul passes | Notable finding |
|---|---|---|---|
| ops-retire-alarm | h1 | 1 | none |
| bifrost-retire-alarm | h1–h2 | 2 | `silent-skip`: a stray `discord-dm` command sat pending forever |
| bifrost-suite | h1 | 1 | `suite-cultcache-scope` (pre-existing): 9 of 17 tests failed at base on a CultLib rename |
| bifrost-notice-reader | h1–h3 | 3 | `prune-keyed-on-presence`, `bridge-exit-is-failed`: duplicate DMs and blocked closures |
| bifrost-notice-retry | h1–h2 | 2 | `future-stamp-stalls-retry` |
| ops-notice-deploy | h1–h3 | 3 | `retry-window-five-minutes`: a 5-minute outage dropped a notice for good |
| idunn-incident | h1–h3 | 3 | `lock-stalls-tick` (Blocker); `lock-acl-world-openable` (High) |
| idunn-topology-lock | h1–h2 | 2 | `contended-publish-aborts`; the live stall it fixed was pre-existing |

## Structural delta

The landed code was about 2.5× its estimate, and the misses were not explained
cut by cut as the skill asks.

- **idunn-incident:** +1813 against +700. Its fix batches added the lock
  primitive, load-independent lock tests, and per-operation fault keys.
- **bifrost-notice-reader:** +891, against r3's estimate of +140. That r3
  figure described only the revision's own delta, not the cut, so r3 carried
  the wrong baseline.

Deleted:

- Bifrost's `publish-idunn-alarm` and the pump's `discord-dm` verb.
- gamecult-ops' dead `--operator-alarm-command` scripts.
- Idunn's `publish_projection_mode` and its 0644-lock test.
- The reader's prune-on-absence logic and its `WATCHDOG_NOTICE_NOW_MS`
  production clock seam.

Added:

- `idunn.operator_incident.v1` (`incidents.cc`) and
  `bifrost.watchdog_notice_execution.v1` (the journal).
- `bifrost-watchdog-notice.{service,timer}`.

Nothing was parked.

## What Soul caught

Green suites passed every defect below.

- **Survival and locking.**
  - Incident writes took a *blocking* flock inside Idunn's scheduler tick, on
    a world-readable lock. Any local uid could freeze continuity for every
    target, and `WatchdogUSec=0` meant nothing would restart Idunn
    (`s1.lock-stalls-tick`).
  - After the fix, the live directory's default ACL still made the lock 0644,
    so any uid could stop incidents opening. The test ran in a temp directory
    with no ACL (`s2.lock-acl-world-openable`).
  - The same probe exposed a pre-existing live hazard: `topology.cc.lock` 0644
    with a blocking write in the tick, measured at 5.07 s at base
    (`s2.topology-lock-stalls-tick`). The operator ruled it into the campaign;
    after the topology cut the tick takes 0.09 s.
- **Split authority.**
  - The reader pruned its "already sent" journal whenever a key was absent
    from Idunn's store. Restoring a backup would have re-sent old DMs.
  - The reader decided subject validity with its own grammar, narrower than
    Idunn's.
  - Retention was decided by two predicates in Idunn.
- **Lifecycle.**
  - One running observation closed an exhaustion incident. A release that
    crashes after each restart would have sent about 12 DMs an hour
    (`s1.flap-reopens`).
  - A bridge that died after Discord accepted a message was retried up to 5
    times, and the closure was then blocked forever.
  - Five attempts on a one-minute timer dropped a notice after a 5-minute
    Discord outage.
- **Decorative checks.**
  - A compare-exchange expected `None` on a key that embeds `now`, so it
    guarded nothing (`s1.cas-decorative`).
  - A 2-second wall-clock bound on a lock test flaked under load and counted
    equivalent mutants as caught.
- **The verifier itself.** Soul's s3 found that the stopgap forced one
  `CARGO_TARGET_DIR` on every job, so cargo-mutants' parallel workers tested
  each other's binaries. Two runs of the same two mutants disagreed
  (`s3.mutants-share-target-dir`). Every parallel mutation count reported
  through the stopgap before 2026-10-01 is unproven, in both sessions' work.

## Operator corrections

- **"Bifrost is indeed intended to be the bridge to external services like
  Discord."** The agents had found two delivery paths: VoidBot's
  `notify_owner` and Bifrost's command store. The ownership boundary was
  missing from both repos' summaries.
- **"Stewardship is currently universal…"** Self had filed a gap treating
  follow-up repos as needing stewardship. It was withdrawn; the rule was not
  written in the skill.
- **"…we seem to be saving some tokens… but we're very very slow."** Self
  blamed cold builds, measured, and was wrong. Cold builds took about 51 s and
  warm builds 29 s. The cost was slot queueing, repeated verify jobs, a test
  that hung a slot for 39 minutes, and a 4-hour job from the other session
  holding a fifth of capacity.
- **"Yep, fixing Mind issues is highest priority, immediately once this
  campaign closes"** (`ruling:mind-fixes-next`).

## Incidents

- **Network outages (ENOTFOUND).** Two outages killed three agents
  mid-pass. Each was resumed by `SendMessage` with context intact, and nothing
  was lost, because each agent's state was in commits and the mind.
- **A shared stopgap target directory** made mutation evidence noise. It is
  fixed in Eureka `a755a60`.
- **Self's own brief inflated runtime.** It told Hands to mutate serially to
  dodge a flake, and was corrected to skipping the flaky test by name.
- **A hung test held a verify slot for 39 minutes**
  (`s2.route-connect-test-hangs`). It is fixed.
- **A Hands test helper overwrote the container's `/usr/bin/node`** through a
  symlink. It was caught by the agent's own baseline and then confined
  (`s2.test-clobbers-usr-bin-node`).
- **A worktree directory stays locked** by a finished process
  (`F:\Projects\gamecult-ops-watchdog-deploy`, empty). Removing it is left to
  the operator.

## What worked

- **Soul attacks the layer where the rule fails, not the test.** Every
  High/Blocker finding came from a probe the shipped tests could not see: a
  separate-process lock holder, the live directory's ACL, or a restored store.
- **Point Soul at the load-bearing invariant in the brief.** Naming "can the
  lock hang the tick" produced the Blocker on the first pass.
- **Read before touching live state.** Self installed only after checking the
  installed revision, the in-flight transactions and the unit diff. The repo
  unit carried a BOM the live one did not.
- **One verify job per pass.** Once briefs required it, a reader fix batch
  took 11 minutes, where the first reader Hands took 38.

## What to change in the pipeline

Already changed in the skill:

- The substrate gaps moved from a table to the mind.
- Imagination owns the model page.
- Soul pays for each rerun once.
- The rust image uses sccache.
- The stopgap no longer forces `CARGO_TARGET_DIR`.

Still to change:

- **Explain every ledger miss in the next report's deviations.** This campaign
  ran at about 2.5× its estimate and nobody explained it per cut.
- **Make a revision's `estimate` the whole cut.** r3's estimate covered only
  its own delta.
- **Verify the merged tree of a long-lived branch before pushing.** Self did
  this once, for the incident cut over B5, and it should be the rule.
- **Briefs name the standard sibling build command.** The stopgap cannot
  provision siblings (`follow_up:gap-verify-no-sibling-repos`), and one
  hand-rolled build produced four false failures.

## Open follow-ups

Fifty follow-ups are in force. Twenty are substrate gaps (`gap-*`): the work
the operator ruled next. Query them with the substrate-gaps recipe in the
skill. Seven findings remain unresolved:

- **`cut-idunn-incident.s3.mutants-share-target-dir`.** It is fixed in the
  stopgap but awaits a Soul mutation run under the fixed script.
- **Six findings whose resolution keys exceed 64 bytes**
  (`follow_up:gap-unresolvable-finding-key`). Their substance is recorded in
  `follow_up:notice-retry-hardening`, `follow_up:ops-refusal-test-hardening`,
  `follow_up:bifrost-pump-input-hygiene` and
  `follow_up:contended-publish-bounds`.

Operational follow-ups that matter most:

- `closure-dm-observed`
- `notice-unknown-settle`
- `contended-publish-bounds`
- `idunn-load-flaky-tests`
- `cultcache-rs-lock-mode`
- `cultcache-ts-readonly-pull`
