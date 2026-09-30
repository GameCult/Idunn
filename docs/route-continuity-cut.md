# Route continuity and admission: cut map

Status: cut map, Imagination pass 0b (Opus), 2026-09-29. Nothing has landed. 2026-09-30: the Odin-lifecycle authority audit is folded in (sections 3, 4 and 5); see the Cut status block.
Ends are owned by route-continuity-target.md.
**2026-09-29, Self (Eureka session "Codebase audit"; the campaign was handed
over from the StreamPixels deployment session with the operator's
confirmation).** The operator ruled all six questions in section 3, one at a
time: **Q1 (b), Q2 (b), Q3 (b), Q4 (b), Q5 (a), and Q6 (a) with a Soul gate on
the weekend branch before the merge.** Next: S1 in Hands, and the Q6 Soul gate
on CultLib `codex/fix-node24-ajv-esm`. Behaviour cuts re-take their
`file:line` anchors in their own briefs. The operator has asked for no live
mitigation on Yggdrasil yet.

**B3 status, 2026-09-30 (Self).** Hands pushed B3 to `idunn/route-b3` at `5ca7760`; it is based on
B1's merge `a61540b`, not stacked on B2. The Opus Soul pass found:
- Full suite green: 252 passed, 2 ignored.
- `cargo-mutants --in-diff`: 31 caught, 4 missed.
- Soul's probe P1 drove a stateful route-proof target through the Engine, so the stateful path works; it
  was only untested.

Verdict: safe to merge for the StreamPixels web ship. The fix batch, in Hands, covers these findings:
- the evidence tag is a second class authority, and a pre-B3 transaction wedges Routing;
- the driver's rejections of a bad answer become gate waits instead of errors;
- the shortfall is prose;
- `OdinSelf` is decorative, and "is Odin" is still decided by name in five places;
- Warming is never driven past one step;
- seven unpinned checks.

Before `idunn up`, one read-only decode of a *copy* of the live `control.cc` checks for non-terminal
transactions of routed targets with no Odin declaration (operator approved, 2026-09-30).

**B3 fix batch, 2026-09-30.** Pushed `6ea824b` and `e11b457`; 275 tests pass.
- `ReadinessClass::of` is the only class authority. The evidence tag is demoted to `Voucher`.
- `ChallengeFailure::{Silent, Refused}` separates silence from a refused answer.
- `PresenceDisagrees` makes the capacity shortfall a typed value.
- The recipes are on `route/b3-declare`: Heimdall `d5acc94`, Muninn `2260853`.
- Still open, for a fresh Hands: three survivors (boot re-proof by tag, config error as silence, first-Odin
  check by name) and `cargo mutants --in-diff`.

**Live store, read-only decode of a copy, 2026-09-30 14:56 (operator-approved).**
- `control.cc` holds 5 envelopes, all `admitted_generation.v2` (pre-F0), and **no transactions**.
- `history.cc` holds 957 transaction v3 envelopes and 641 command v2 envelopes.
- Admitted generations, all on Odin receipts:
  - ghostlight: routed, declares Odin, stateful.
  - odin: routed rudp, provides the rendezvous.
  - raven-muninn: unrouted, no declaration, stateless. B3 class `Undeclared`, so **held**.
  - streampixels-service: routed, declares Odin, stateful.
  - streampixels-web: routed, declares Odin, needs `streampixels.service.api`.
- **Heimdall is not admitted.** It drops out of the ship window. Its declaring recipe still merges. Its
  first deploy will hit Soul pass 2's F2: a routed, Odin-correlated target must pass the stable-route
  proof, and Heimdall answers no challenge. Record that for its own deploy.

**B3 pass 2 (Soul, 86ee21d):** safe for the web ship. One same-window blocker:
- **F1:** supervision mints a held continuity over a dead held generation, and that continuity owns the
  target, so the declaring redeploy can never freeze. The fix is in Hands.
- F5, bad answers read as Silent over HTTP keep-alive and TCP, is in the same batch.

**B3 fix batch (`86ee21d..e8b4487`), Soul pass 3, 2026-09-30: merge with follow-ups.** Nothing here hurts a
live host on install. 294 passed, 2 ignored. `cargo mutants --in-diff`: 53 caught, 0 missed.
- **F1 holds.** There is one continuity mint site, and the hold check comes before it. A declaring redeploy
  replaces a dead held incumbent end to end.
- **F5 holds.** Zero bytes is Silent. Any byte then a stall or stop is Refused. Non-2xx responses and
  non-chunked Transfer-Encoding are Refused.
- **Follow-ups, in Hands on the same branch:**
  - the P1b test never reaches the yield branch (mutant M7 survives);
  - the chunked trailer loop is unbounded;
  - `+N` lengths and whitespace before the colon are accepted;
  - exact 1 MiB and one-byte-then-stop are unpinned;
  - the `Silent` doc is stale;
  - the dead-hold report goes to stderr only and never clears.

**Ruling, 2026-09-30 (operator): cancel accepts a held pre-fence Deploy.** Idunn still never aborts a held
record on its own. An operator can discard a held Deploy that has not reached the fence with `idunn cancel`.
Nothing is fenced, so the incumbent is untouched.

**B3 follow-ups landed on `idunn/route-b3` at `5c49646`, 2026-09-30.**
- Cancel accepts a held pre-fence Deploy. It shares `pre_fencing_abort_intent` with the scheduler.
- The P1b test is now in direct-supervision form, with a positive control.
- The reader is strict: trailers share the 32 KiB header budget; digits only; header names without whitespace.
- The `Silent` doc is corrected.
- Tests: 301 pass. `a_failing_step_is_retried_after_a_backoff_not_every_tick` is flaky under a full parallel
  run, on the base too.
- `cargo mutants --in-diff`: 24 caught, 0 missed.
- A narrow Soul pass gates the install.

**Ship log, 2026-09-29 (host clock).**
- B3 merged at `d32395a` and installed on Yggdrasil at 18:36 UTC, with sha `854aa351`. It migrated 5 control
  records. The store backup is `/root/idunn-store-backup-20260929T183604Z-pre-route-b3`.
- Nginx reloads fell from 82 per 10 minutes to 0. Odin continuity timeouts fell from 92 in 6 hours to 0.
- **Raven's actuator (built 2026-09-11) could not decode the new plan types.** A rebuild then found that the
  Windows `idunn-host` has not compiled since S1: `FROZEN_SOURCE_SYMLINK_TARGET_LIMIT` was defined for unix
  only. That is fixed at `d3db582`.
  - The new actuator (sha `f36bf1fc`) is swapped in on Raven. The old one is kept as
    `idunn-host.exe.prev-20260929T190006Z`.
  - Follow-up: nothing builds the Windows actuator in verification.
- raven-muninn was admitted at 19:02 by Odin correlation, replacing the held generation.
- **The streampixels-service deploy failed before fencing.** The S1 `bulk_fetch_objects` asks for bare blob
  ids with `git fetch --stdin`. In a `blob:none` clone, the connectivity check then fails with "bad revision
  <blob>". The fix, with a test, is in Hands on `fix/bulk-blob-fetch`: a promisor-style fetch with
  `--filter=blob:none` and `fetch.negotiationAlgorithm=noop`.

**Ship completed, 2026-09-29 (host clock).**
- **Bulk-fetch fix.** Merged at `583b2a7` and installed at 19:17. The Idunn sha is `01bde676`.
- **The first streampixels-service redeploy failed after fencing.** The incumbent was restored, and every restart
  was refused by Odin with "runtime presence publisher sequence was reordered". Cause: Odin compared publisher
  sequences across incarnations, and the TS publisher counts from 1 per process.
  - The operator ruled: fix Odin's rule.
  - Odin `db041ec` compares sequences only while the stored presence still authenticates under the current
    activation. Soul said deploy, with follow-ups; Odin was deployed at 19:4x.
- **Admissions.**
  - streampixels-service was admitted at 19:50 on s3-c2.
  - The web binding dropped `STREAMPIXELS_ODIN_CULTMESH_RUDP`, and gamecult-ops `37b5ed9` does the same.
  - streampixels-web was admitted with Odin-free readiness.
- **Public cutover at 19:54.** The vhost upstreams moved to `:8833` (web) and `:8832` (service). The backup is
  `/root/streampixels.gamecult.org.conf.bak-20260929-195415-pre-idunn-upstreams`.
  - `https://streampixels.gamecult.org/api/catalog` returns 200 (120,893 B), after being 502 since migration 013.
  - The overlay SSE stream answers `text/event-stream`, and the ops checker passes.
- **Legacy units.** `streampixels-web.service` is disabled and stopped. `streampixels-service.service` is
  disabled, and was already stopped.
- **Follow-ups.**
  - The C2 acceptance hour: sample the nginx reload counter at T0 and T0+60, with no rejected challenges.
  - Nothing builds the Windows `idunn-host` in verification.
  - Odin presence: pin the stored-at re-authentication time (Soul's M3 survived), and consider checking activation
    identity directly rather than any failure to authenticate.
  - The dead-hold report field (ruled).
  - B2 rework on B3.
  - Odin's CultLib pin bump, after the stray-packet merge.

**C2 acceptance hour passed, 2026-09-29 19:55-20:55 UTC (21:55-22:55 CEST).**
- 1 nginx reload, which was Odin's own route promotion during its `5c37860` deploy.
- 0 rejected StreamPixels challenges, 0 Odin continuity timeouts, and Idunn `NRestarts=0`.
- One running unit each for service, web and Odin. Public `/` and `/api/catalog` return 200.
- Also deployed in the window: Odin `5c37860`, carrying CultLib `3bf1c0c` (RUDP stray-packet hardening in every
  runtime), Sleipnir removed, and presence ordering decided by activation identity.
- Still open for Odin: it exits when its 64-slot RUDP session table fills, because its own heartbeat publish
  fails. Publishers that never Disconnect hold slots for 30 s. The fix is in Hands.
- The second run, with Odin broken on purpose, remains the operator's to schedule.

**Ruling, 2026-09-30 (operator): the dead-hold report is stored on the generation.**
- Add an optional `last_error` to `AdmittedGeneration` (new key 21), and make `status` render admitted
  generations.
- It is cleared when the generation is replaced or its hold is released.
- The `generation:<target>` readiness report gets the same home.
- It is queued after the ship, with the B2 rework.

**Odin diagnosis (Eyes, read-only, 2026-09-30).**
- The 109 Odin route timeouts are self-inflicted. Live Idunn (`8ae00a1`, pre-S1) reloads nginx inside
  `restore_admitted_membership` before every Odin challenge. The reload moves the reuseport UDP flow, so
  the challenge misses. All 109 have a reload 3.05-3.12 s before them.
- 2,977 reloads in 6 h. 479 shutting-down nginx workers, because the stream has no `proxy_timeout`.
- The route timeouts do not block Ready correlation.
- Odin has crashed twice right after a reload. A stray RUDP packet with a foreign connection id escapes
  `cultnet-rs` `rudp.rs:1217` (`require_connection`) and ends the process. Each crash costs about 75 s
  without a lease. The CultLib fix is in Hands on `hands/rudp-stray-packets`. Odin takes it at its next pin
  bump, after the ship.

**Operator ruling: Idunn first.** Install the new Idunn (B3 carries S1), then watch for about 15 minutes that
reloads stop and Odin stays up. Only then redeploy raven-muninn and StreamPixels.

**Ship sequence:**
1. B3 fix, then narrow Soul, then merge B3.
2. Merge StreamPixels `route/s3-c2` and the Muninn and Heimdall `route/b3-declare` recipes.
3. Build the Idunn release on its deploy path.
4. Back up `control.cc` and `history.cc`. F0's lift is irreversible.
5. Install Idunn.
6. `idunn up` raven-muninn with the declaring recipe.
7. `idunn up` streampixels (service, then web).

**Rulings, 2026-09-30:**
- A stored record whose evidence disagrees with its declared class is **held and reported, never aborted**
  by Idunn. B5's deadlines or the operator resolve it.
- **Heimdall and raven-muninn are redeployed with their declaring recipes in the same window as Idunn B3
  and the StreamPixels ship.**

**Ruling, 2026-09-30: the recipe declares readiness; Idunn infers nothing.** This settles the fork
Hands raised against a literal Q1(b). An unrouted target whose recipe declares neither a route nor an
Odin dependency is refused at admission with a typed error. It does not fall back to Odin correlation
because it has no route. `raven-muninn` and Heimdall both publish to Odin, so their recipes declare it.
The rejected alternative let Odin's availability gate Muninn's crash recovery through an undeclared,
inferred dependency.

**B2 status, 2026-09-30 (Self).** Hands pushed `idunn/route-b2` at `1786ddf`. The Opus Soul pass rejected
it for merge.
- **F1 (critical):** `ContinuityBackoff.deferral_reason` was added without a schema bump. B2 fails the
  canonical re-encode of every three-field v3 generation that F0, B1 or B3 wrote, so Idunn cannot read its
  own store.
- **F2:** the ceiling refuses a rollback, which leaves an unproven candidate on the stable route.
- **F3:** a failed challenge writes, and continuity's CAS then loses against a stale envelope.
- **F4:** neither window rolls; both are anchored at the first charge.
- **F5:** a clock stepped backwards stalls everything for the length of the step.
- **F6:** `record_proved_challenge` is unpinned.
- **F7:** the mount preflight is not gated.
- B2 also does not compile on top of B3, and after a one-line fix two tests fail, because the fixture target
  is now route-proof.

The rework is a fresh branch from `583b2a7`, not a rebase of `1786ddf`. Its map is under B2 in section 4.

**Rulings, 2026-09-30.**
- **The ceiling never refuses a rollback.** Restoring the admitted or incumbent route is survival, not
  deployment. It is always allowed and always counted. Only installs and other forward changes can be
  refused. A rollback that keeps failing is spaced by backoff, never refused.
- **First routed deployments are metered now.** The actuation window lives on the target, not on the
  admitted generation, so a target with no admitted generation is charged too. That schema change is folded
  into the store-version bump that F1 needs: a v4 generation, with a typed lift from v3.

**Rulings, 2026-09-30 (operator, on the B2 rework map).**
- **Q-B2-1 (a):** the continuity restart log moves into `TargetSupervision`; the restart ceiling reads the
  target's own log; the carry compensator is deleted.
- **Q-B2-2: fail at once.** A deploy refused by the route/actuation ceiling before the fence fails immediately,
  with the reopen time in the error.
- **Q-R1 (a):** the route declaration is the challenge declaration. Every routed target answers the
  stable-route challenge regardless of readiness class. R1 checks it before the fence; H1 gives Heimdall a
  responder. Operator note: the question's first framing was confusing; the cause is that Heimdall has no
  responder, not its class.
- **Q-B4: Idunn's own proof.** Routed providers by current route proof and not degraded; unrouted providers by
  admission plus a live workload observation. One clause is **held open**: "a current Odin non-Ready word
  still excludes a provider" is not ruled, pending the Odin-lifecycle authority audit (a separate Imagination
  pass). Operator: "the last few rulings on Odin are making my authority sense tingle, it feels like Odin is
  doing lifecycle work when that's very much Idunn's wheelhouse." B4 is blocked on that audit.

**Odin-lifecycle audit rulings, 2026-09-30 (operator).** The audit landed as section 5. Q-O1: Idunn proves
(supersedes Q1 (b); readiness classes end at A1 + A3). Q-O2: a private challenge endpoint for unrouted targets
(A2 + a Muninn responder). Q-O4: the Odin dependency is optional and does not gate a compile. Q-O5: restart on bad
auth (amends Q5 (a)). The held Q-B4 clause is struck; B4 becomes B4′. Q-O3 (redeploy order odin ->
streampixels-service -> ghostlight -> web with the operator present, vs a one-shot relabel) is **not yet asked**
and is needed before A1 installs. Odin keeps three permanent self-publication failures fatal until A3 + Q-O5 land
(temporary rule, section 4).

**Cut status, 2026-09-30.**
- **Ready for Hands:** B2 (rework map in section 4); D1 (after B2). **R1 is never to be written**, superseded by A1.
- **In progress:** `hands/idunn-b2-rework` (B2 rework); `hands/idunn-w1-t1-t2`: W1, T1, T2; X1 on Odin
  `hands/odin-x1-residue`; the temporary fatal rule on Odin `hands/odin-session-table`.
- **Next, in order:** A1 (after B2, and after Q-O3 is ruled), B4′, A2 with M1, A3 (irreversible store migration,
  back up first), O-R, L-R (section 4, Odin-authority cuts).
- **Open for Soul:** the continuity-restart stall probe and the journal check of the ghostlight/raven-muninn
  exclusion (section 5.5).
- **Unblocked:** B4 (now B4′). **Blocked:** H1 (Heimdall repo), on CultLib C1. A supervision change for Q-O5 has
  no cut yet.


Heads read for this map:

| Repo | Ref | Notes |
|---|---|---|
| `F:\Projects\Idunn` | `main` = `d719485` (code `8ae00a1`, trusted baseline `9ede4cd`) | `idunn/cut1-fix5` is 14 ahead / 25 behind `main` |
| `F:\Projects\StreamPixels` | `main` = `a665bce` | `vendor/CultLib` gitlink = `542ddd3` |
| `GameCult/CultLib` | `origin/main` = `268e0ef`; `origin/codex/fix-node24-ajv-esm` = `542ddd3` | the Idunn TS runtime package exists only on that branch (see F-C1) |
| `F:\Projects\Odin` | `379b826` | read only, for lease pickup |
| Yggdrasil | `/usr/local/bin/idunn` sha256 `5f257f2c…`, `idunn-yggdrasil.service` active | read-only probes, 2026-09-29 |

"Read" means the claim comes from source at those heads. "Probe" means a
read-only command on Yggdrasil on 2026-09-29. Nothing was written to the host.

---

## 1. Body facts

### 1.1 Admitted route supervision actuates on observation age

- F1. The scheduler tick (`run_scheduler_tick`, `control_plane.rs:3075`) runs
  `resume_one_transaction`, then `supervise_one_admitted_generation` (`:3293`),
  then freezes commands. The loop sleeps `poll_millis` (default 500 ms,
  `:1473`) between ticks. Read.
- F2. `supervise_one_admitted_generation` calls `supervise_admitted_route`
  (`:3606`) first for every admitted target that no transaction owns past
  Fencing. The call is made for every admitted target on every tick. Read.
- F3. `supervise_admitted_route` returns early only when all three hold: the
  fragment on disk equals the admitted membership, the route observation is
  younger than `topology_maximum_age_millis` (default 30 000 ms,
  `DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS`, `:61`), and no repair intent exists.
  Otherwise it CASes `route_repair_started_at_unix_millis = now` into the
  admitted generation and returns (`:3654-3673`). On the next tick it calls
  `restore_admitted_membership` and then `prove_stable_route_against`. It
  clears the intent only after a successful proof (`:3707-3722`). Read.
- F4. `restore_admitted_membership` (`drivers.rs:5455`) runs the following on
  every call, even when the fragment is byte-identical:
  1. `validate_candidate_in_private_mount`, which launches a transient
     `systemd-run --wait` unit running `nginx -t`.
  2. `admit_endpoint`, which runs `ufw allow`.
  3. `reload()` (`:5254`), which runs `nginx -t` and then
     `systemctl reload nginx`.

  The write is skipped when the bytes match (`:5467`). The actuation is not.
  If the reload fails, it deletes the fragment. Read.
- F5. Consequence: a healthy routed target reloads nginx about once every
  30.5 s. A target whose proof fails keeps the repair intent and runs
  restore and reload on every tick, limited only by the 3 s challenge timeout.
  Nothing records, rate-limits, or backs off these actuations. Read.
- F6. Probe: 551 `Reloaded nginx.service` in the last hour across four route
  fragments (`ghostlight`, `odin`, `streampixels-service`,
  `streampixels-web`). 460 processes named `nginx: worker process is shutting
  down`. The Idunn journal for the last 10 min shows route continuity
  rejections for `streampixels-service` (EAGAIN, 6), `streampixels-web`
  (EAGAIN, 4; HTTP 503, 1), and `odin` (RUDP connect timeout, 1).
- F7. Probe: `/etc/nginx/idunn-stream-routes/odin.conf` is the Idunn-rendered
  UDP stream proxy `listen 10.77.0.1:17871 udp reuseport;` →
  `server 127.0.0.1:17973;`. `render` emits that shape for
  `NginxStreamUdp`+`rudp` (`drivers.rs:5210-5217`). Odin's own route proof
  goes through that listener as a RUDP catalog snapshot
  (`request_runtime_presence_at`, `drivers.rs:5566-5574`). Every reload of any
  target therefore moves Odin's UDP flows. Read + probe.
- F8. An admitted generation is never withdrawn. The only writers of
  `AdmittedGeneration::TYPE` are commit (`:4749`), route supervision
  (`:3670`, `:3715`), and topology refresh (`:3780`). After three refused
  continuity restarts, supervision stops restarting (`:3522-3528`) but keeps
  supervising the route of a dead incarnation. Read.
- F9. The continuity restart limit counts refused restarts per generation
  (`is_refused_restart`, `:3469-3488`). A restart that succeeds admits a new
  generation and resets the count. A unit that starts, is admitted, and then
  dies is restarted without bound and without backoff. Read.

### 1.2 Web route proof today

- F10. Idunn's challenge is a CultNet `SnapshotRequest` sent to the stable
  endpoint. For `http` it is `POST /cultnet/snapshot` with MessagePack and an
  explicit `Content-Length`. There is a 3 s connect/read/write timeout, and
  `ConnectionRefused` is retried for 2 s (`29946bb`) (`drivers.rs:5524-5580`,
  `:5628-5720`). Read.
- F11. `authenticate_routed_presence` (`control_plane.rs:5185`) verifies the
  following through `authenticate_runtime_presence_claim` and
  `correlate_runtime_presence_claim`
  (CultLib `61549aa`, `runtime_authority_contracts.rs:1163-1230`):
  - the provider signature;
  - the activation signature, made with the per-launch key whose public half
    is in Idunn's activation;
  - the Expected, activation, runtime and endpoint binding;
  - `observed_at >= challenged_at`;
  - age at most 30 s;
  - `state == "active"`;
  - `detail == "route-observation:{message_id}"`;
  - the exact current write-lease sha.

  **The Idunn verifier already satisfies invariant 4 over HTTP. It needs no
  Odin input.** Read.
- F12. The app side couples that proof to Odin. `publishRouteObservation`
  (StreamPixels `vendor/CultLib/packages/cultnet-ts/src/idunn-runtime-authority.ts:286-298`)
  calls `publish("active", "route-observation:<id>")`. That call signs the
  presence and then **publishes it to Odin over RUDP**, waiting up to 5 s for
  accept and 2 s for ack (`publishDocument`, `:319-356`), before it returns
  the document. It signs `active` regardless of application health. Read.
- F13. The Next handler (`apps/web/app/cultnet/snapshot/route.ts`) maps any
  publisher failure to 503. The app-side RUDP wait (up to 7 s) exceeds Idunn's
  3 s read timeout, which produces the EAGAIN (`os error 11`) in F6. Read +
  probe.
- F14. The app requires Odin before it can prove anything:
  - `loadIdunnRuntimeAuthorityFromEnvironment` throws unless Expected carries a
    `shared-infrastructure odin.verse-rendezvous` dependency equal to
    `STREAMPIXELS_ODIN_CULTMESH_RUDP` (`idunn-runtime-authority.ts:114-118`).
  - `runIdunnRuntime` blocks startup for up to 30 s on Odin accepting the
    initial warming publication (`deployment/idunn/runtime-presence.mjs`,
    `publishInitialWarming`).
  - `deployment/idunn/web.toml` declares the Odin dependency and the env var.

  Read.
- F15. The Idunn runtime TS package (`idunn-runtime-authority.ts` and its
  test) does not exist on CultLib `origin/main`. It exists only on the
  unmerged branch `codex/fix-node24-ajv-esm`, which is 10 commits ahead of
  `origin/main` (`0bedcde`..`542ddd3`). StreamPixels pins that branch commit.
  Read.

### 1.3 Which Idunn decisions consume Odin for a non-Odin target

All of these are read:

| Decision | Site | Odin input |
|---|---|---|
| Provider selection at plan compile | `advance_sealing` `:3825` → `current_ready_provider_tokens` | admitted providers' Odin `ready` and `latest_odin_observation`; `ManagedReady` carries `odin_topology_correlation_sha256` and sequence (`deployment_plan.rs:136-145`) |
| Provider currency (Deploy only) | Sealing→Starting `:3880`, Starting `:3945`, Routing `:4527`, Committing `:4629`, `:4673` → `validate_selected_providers_current` | same receipts; since `9f00e7a`, authenticated at their admission time rather than now |
| Warming | `advance_warming` `:4028-4103` | only Odin correlation, except the `FirstOdinDirect` exception for `target == "odin"`. For a non-Odin target with no correlation it records a gate wait forever |
| Lease grant (stateful) | `fresh_warming_for_lease` `:4808` | new Odin sequence plus new signed presence |
| Ready | `advance_awaiting_ready` `:4423` | Odin correlation `is_semantic_ready` |
| Route admission | `advance_routing` `:4487` | admits the latest topology; Ready is refreshed and `require_current=true` |
| Commit | `advance_committing` `:4599` | topology admitted twice, once before and once after the final stable-route proof; `AdmittedGeneration` requires `ready`, `latest_odin_observation`, `odin_authority` and the cursor as non-optional fields (`:1322-1331`) |
| Admitted refresh | `refresh_admitted_topology` `:3726` | Odin correlation updates the admitted receipts |
| Graph gate | web recipe's `odin.verse-rendezvous` dependency | Odin must be admitted and Ready to compile the plan |

- F16. **Commit livelock (source-read, supported by live history).**
  1. `advance_committing` admits the topology, proves the stable route, then
     calls `admit_latest_topology(&ready_current, …)` again.
  2. If a new Odin sequence arrived meanwhile, that call persists it, the
     envelope changes, and the function returns `Ok(())` without committing
     (`:4658-4666`).
  3. The web's route proof itself publishes a fresh `active` presence to Odin
     (F12), and Odin re-stamps its correlation when the presence changes
     (comment `:5473-5478`).
  4. Every commit attempt therefore tends to manufacture the evidence that
     aborts it.

  Probe: `up-efb85890…` (`tx-5fe84313…`) is now `Complete`/`Admitted` at Odin
  cursor 4008. It had been `Committing` for more than 12 h at cursor about
  3680. A commit that eventually wins a race fits this mechanism. No
  reproducer has been written yet.

### 1.4 Post-fencing phases and leases

- F17. When a transaction errors at or after Fencing, it is aborted only if
  `candidate_is_permanently_stopped`. Otherwise `record_resumable_error` runs
  (`:3107-3131`). There are no deadline fields on `DeploymentTransaction`
  (`:651-731`). Read.
- F18. `advance_awaiting_ready` returns `Ok(())` for as long as no new Ready
  correlation exists. Only candidate death interrupts it
  (`observe_candidate_before_waiting`). Routing and Committing wait the same
  way. Read.
- F19. Lease pickup windows are owned by the consumer, not by Idunn:
  - Odin accepts a lease only if its `warming_presence_sha256` names a warming
    proof Odin issued within `WARMING_PROOF_LIFETIME_MILLIS = 60_000`
    (`odin-daemon/src/main.rs:73`, `acquire_process_write_lease` `:886-911`,
    `try_activate` `:225-263`).
  - The TS publisher accepts any of its last 64 proofs with no time bound
    (`assertLeaseMatches`, `idunn-runtime-authority.ts:687-706`).
  - Idunn neither knows the window nor observes adoption, other than through a
    later Odin correlation.

  Read.
- F20. `cancel` (`:1725`) is an operator CLI process that writes
  `post_fencing_abort` directly into a live daemon-owned transaction by CAS
  (`8ae00a1`). The only cases it accepts are Deploy, past Fencing,
  `SkippedStateless`, no completion. Continuity and stateful transactions have
  no recovery command. Read.

### 1.5 Continuity drift between the admitted receipt and the projection

- F21. `from_continuity` (`:783`) reuses the incumbent's Expected, so the
  continuity candidate projects under the same key `{target}@{expected sha}`.
  `publish_observed_activation` replaces any activation under that key
  "without comparison" (`drivers.rs:4927-4955`). The admitted generation keeps
  the old activation until commit. Commit replaces the admitted generation and
  the transaction in one CAS (`:4745-4770`), so a **successful** continuity
  leaves no drift. Read.
- F22. **Drift source.** Both abort intents set `topology_reconciliation =
  Skipped` for `CommandKind::Continuity` (`begin_pre_fencing_abort`
  `:5822-5828`, `post_fencing_abort_intent` `:6224-6230`). A failed continuity
  therefore leaves its own candidate activation projected under the shared
  key while the admitted receipt names the older activation. Two things then
  refuse, because `ProjectedIncarnation::read` rejects any projected activation
  that differs from the one passed in (`drivers.rs:4293-4300`):
  - supervision's pre-restart demotion (`:3538-3556`, logged and ignored);
  - a later deploy's post-fencing `demote_to_expected_only`.

  `a148802` made rollback demote whatever activation is projected, provided it
  verifies against Idunn's anchor
  (`demote_current_activation_to_expected_only`, `drivers.rs:4500-4543`). The
  cause is the skipped reconciliation. The coherent cut is for the continuity
  abort to demote its own exact activation to Expected-only. Read.
- F23. `9f00e7a` exists because admitted receipts only advance when Odin emits
  a new sequence (`refresh_admitted_topology`). Provider publications to Odin
  were failing (F7, F12), so a provider's receipts aged past 30 s and
  dependents could not select it. The drift source is that dependency currency
  is sourced from Odin receipts at all (see Q2). Read.

### 1.6 Weekend commits

| Commit | Read | Classification |
|---|---|---|
| `a148802` recover fenced rollback from current activation | diff | **Revert** in Cut B1, together with the F22 fix. Delete `demote_current_activation_to_expected_only` and its test. Restore the doc paragraph to the exact-activation rule |
| `9f00e7a` replay provider receipts at admission time | diff | **Revert** in Cut B4, together with its replacement currency source (Q2). A revert alone re-breaks dependency selection |
| `ae8ac45` process title as launch evidence | diff | **Keep.** `command_line_sha256` stays launch-time evidence; Node's `process.title` rewrite is not identity drift |
| `29946bb` wait for stable route listener | diff | **Keep.** After Cut S1 it covers only install-time listener handoff, which is its honest scope |
| `8ae00a1` cancel live stateless candidates | diff | **Delete** in Cut S2 once deadlines exist. It is a second writer into daemon-owned transaction state (F20) |
| `8651906` dynamic primary group | diff | **Keep.** Independent of this campaign |
| `55e5d55`/`7a61c77`, `4200330`/`e0f5147` | log | Self-reverted pairs; net zero |

---

## 2. Identity, lifecycle, authority

An empty or "none" cell is a finding. It is marked **(gap)**.

| Kind | Identity | Lifecycle | Authority |
|---|---|---|---|
| **Admitted generation** (`idunn.admitted_generation.v2`, `control.cc`) | key = target; injective per target. `generation_id = generation-{tx}` | **Created/superseded** by the commit CAS (transaction + generation atomic). **Revised** by route supervision (routing receipt, repair intent) and topology refresh (Odin receipts, cursor). **Withdrawn:** never **(gap, F8)**. **Replayed:** read on every tick; `validate_durable_authority` at boot | Owner: Engine commit. Three in-process writers (commit, `supervise_admitted_route`, `refresh_admitted_topology`), all CAS. Forbidden: CLI, Odin, drivers |
| **Route observation / receipt** (`RoutingEvidence::Promoted` inside transaction and generation) | `route_id` + `runtime_instance_id` + `membership_sha256` + `signed_presence_sha256`; injective per incarnation | Created at Routing. Re-created by each successful admitted challenge. Stale at 30 s, which today **triggers actuation** (F3). What is in force afterwards: the last successful receipt, even while proofs fail | Owner: route supervision (observe only, after S1). Forbidden: reload path, Odin |
| **Route repair intent** (`route_repair_started_at_unix_millis`, gen key 18) | per admitted target; a single timestamp | Set when stale or drifted. Cleared on proof. **Attempt count, backoff, ceiling: none (gap)** | Owner today: supervision. Replaced in F0 by *route supervision state* (below) |
| **Route fragment on disk** (`binding.config_path`, `/etc/nginx/idunn-stream-routes/<route>.conf`) + **ufw allow** | path is injective per binding (`validate_route_binding_set` `:2386-2393`); first line `# Idunn Expected <sha>` | Written by `install` (Routing), `restore_admitted_membership` (supervision), and `restore` (abort, rollback). Removed on failed restore reload or last withdrawal. ufw is re-admitted on **every** restore (F4) | Owner should be the route driver, acting only on membership change. Forbidden (after S1/B2): observation freshness, proof failure |
| **nginx reload** (process actuation) | none; not a record **(gap: no durable actuation ledger, so the invariant 2 ceiling cannot survive an Idunn restart)** | Unbounded (F5, F6) | Owner: route driver. Rate authority: **none (gap)**. F0 adds it |
| **Deployment transaction** (`idunn.deployment_transaction.v3`) | `tx-{uuid}`; one live per target (`blocks_new_target_mutation`) | Phases Sealing…Complete. Aborts pre and post fencing. Archived to history when terminal. **Phase deadlines: none (gap, F17-F18)**. Post-fence error = resumable forever | Owner: Engine. **Forbidden writer present: `idunn cancel` CLI (F20)** |
| **Activation** (in transaction/generation + projection `idunn.runtime_activation` in `topology.cc`) | projection key `{target}@{expected sha}` is **not injective across continuity**: every restart of one Expected shares it (F21) | Published after workload observation. Replaced without comparison. Demoted on death or abort. **Continuity abort leaves its own activation projected (gap, F22)** | Owner: transaction for its own activation; generation for the admitted one. Forbidden: rollback adopting an activation it did not issue for this transaction (`a148802`) |
| **Write lease** (file `record_path` + `.lock`) **+ projection** | exact record binding Expected, activation, warming sha, epoch | Prepared → Granted → (adopted by process) → revoked at fence or abort. **Adoption is not observed by Idunn and has no pickup deadline (gap, F19)** | Owner: Idunn lease driver. Pickup-window contract: owned by consumers (Odin 60 s, TS 64 proofs). **No Idunn-side owner (gap)** |
| **Runtime presence doc** (`gamecult.runtime_presence_health.v2`) | target + `runtime_instance_id` + `publisher_sequence`; the challenge nonce rides in `detail` | Minted by the app per challenge and per heartbeat. Idunn persists only `signed_presence_sha256` (route receipt) or the full bytes (`FirstOdinDirect`). Odin persists its own copy | Owner: the service (provider key + Idunn-issued activation key). Forbidden: Idunn manufacturing it; publication to Odin being a precondition of the route answer (F12) |
| **Odin correlation topology** (`/var/lib/gamecult/odin/idunn-runtime-topology.cc`) | `{target}@{expected sha}`, publisher sequence per signer | Written by Odin whenever facts change. Idunn persists admitted sequences into the transaction/generation and advances a per-target cursor | Owner: Odin (discovery). Consumer authority today: warming, Ready, dependency currency for **every** non-Odin target (§1.3). After this campaign: observation for route-proof targets, never admission |
| **Continuity restart history** | derived by counting failed continuity transactions per generation (live + history) | Resets on each new generation (F9). **Per-target ceiling and backoff: none (gap)** | Owner: supervision |
| *New in F0:* **readiness evidence** | enum on transaction and generation: `OdinCorrelated(TopologyEvidence)` or `RouteProof(RuntimePresenceEvidence)`; the class is derived from Expected (Q1) | Replaces the mandatory `ready`, `latest_odin_observation`, `odin_authority` and cursor for route-proof targets | Owner: Engine. Forbidden: a route-proof target's admission reading Odin |
| *New in F0:* **route supervision state** | per admitted routed target: `last_challenge_at`, `consecutive_failures`, `next_challenge_at`, and a rolling `actuations` window (count + window start) | Revised only by supervision. Carried across continuity commits. Replaces `route_repair_started_at` | Owner: supervision. Forbidden: any reload path that does not consult it |
| *New in F0:* **phase deadline** | per transaction: `phase_entered_at` + `deadline_at`, from binding-owned durations frozen into the compiled plan (Idunn defaults) | Set on every phase transition. Expiry is resolved by the deadline resolver (B5) | Owner: Engine transition primitive. Forbidden: resume paths extending it |
| *New in F0:* **lease adoption evidence** | the candidate's signed presence carrying `write_lease_sha256 == granted sha`, obtained directly or through Odin | Recorded once in Leasing/AwaitingReady. Its absence at the deadline means the lease was not adopted | Owner: Engine |
| *New in F0:* **stateful terminal record** | completion variant naming the recovery (`RestartAdmitted`, `RestoreIncumbent`, `OperatorRequired{reason}`) | Terminal, archived like other completions | Owner: deadline resolver |
| *New in F0:* **continuity backoff** | per target: attempts in window, `next_restart_at` | Carried across generations. Replaces per-generation counting for the ceiling | Owner: supervision |
| *HTTP route proof* | **no new kind.** It reuses the presence v2 contract and `RouteObservation` (F11). `RuntimePresenceEvidence` gains a route-proof use beside `FirstOdinDirect` | — | — |

---

## 3. Operator questions

**Q1. Readiness authority for a CultMesh-aware service such as
`streampixels-service`.**

**RULED (b) by the operator, 2026-09-29. HISTORY, not live design: superseded 2026-09-30 by Q-O1 (Idunn proves; readiness classes end at A1 + A3). The text below is kept as the record of the ruling.**

Options:
- (a) Every non-Odin target proves Warming and Ready directly through Idunn's
  challenge. Odin only observes.
- (b) A target's class follows its own declaration. A target that declares a
  `shared-infrastructure odin.verse-rendezvous` dependency is Odin-correlated,
  as today. A target that declares none is route-proof.
- (c) A new explicit binding field chooses the class.

Recommendation: **(b)**. It reads the ruling literally: GC infra that
advertises into the Verse is correlated by Odin, and a web app that declares
no Odin dependency cannot be gated on one. It needs no new field. A repository
choosing route proof gains no privilege, because both classes are signed by
the same Idunn-issued launch key.

Depends on it: the F0 enum shape, the B3 scope, whether the service recipe
changes (under b it does not), and whether the F16 livelock matters for
Odin-correlated targets (B5 covers that).

**Q2. Where a dependent's provider currency comes from** (the `9f00e7a`
replacement).

**RULED (b) by the operator, 2026-09-29.**

Options:
- (a) The provider's Odin receipts, authenticated now. This is a strict revert,
  but the web then depends on Odin transitively through `streampixels-service`,
  which breaks invariant 3.
- (b) Idunn's own current route observation of the admitted provider: a
  successful challenge within max age, with capabilities taken from the signed
  presence in that proof. Unrouted providers fall back to their Q1 class
  evidence.
- (c) Always the provider's own class evidence.

Recommendation: **(b)**. Idunn already challenges every routed target, and
under invariant 1 that challenge becomes the steady-state heartbeat. It is
current, it is signed by the provider's launch key, and it needs no Odin.

Depends on it: B4, the `ManagedReady` shape (the Odin sha and sequence become
a class-tagged evidence digest), and plan compatibility with existing admitted
dependents.

**Q3. How a stateful post-fencing deadline resolves once the candidate may
have written.**

**RULED (b) by the operator, 2026-09-29.**

Options:
- (a) Always a terminal `OperatorRequired` record.
- (b) Split on lease adoption (F0 evidence):
  - Not adopted: revoke, stop, and resolve like stateless. A deploy restores
    the incumbent through the fence. A continuity counts one refused restart.
  - Adopted: a deploy gets a terminal `OperatorRequired{adopted-lease}`
    record. A continuity gets a terminal record and the admitted release
    restarts under the continuity backoff, because it is the same release and
    state contract.
- (c) Automatic incumbent restore whenever the state cut is declared
  reversible.

Recommendation: **(b)**. Adoption is signed evidence that the candidate may
have written. (c) trusts a declaration over evidence.

Depends on it: B5, the recovery variants of the terminal record, and the
replacement for `idunn cancel`.

**Q4. The operator's recovery verb after `8ae00a1` is deleted.**

**RULED (b) by the operator, 2026-09-29.**

Options:
- (a) Delete it outright and let deadlines resolve everything.
- (b) Replace it with `idunn expire <command>`. The command writes a typed
  expiry request that the daemon consumes, so the phase resolves through the
  same deadline resolver. The CLI never writes transaction fields.

Recommendation: **(b)**. It keeps a human escape for long deadlines and
removes the second writer.

Depends on it: S2 scope and the CLI surface.

**Q5. Whether a sustained route-proof failure on an admitted target restarts
its unit.**

**RULED (a) by the operator, 2026-09-29. Amended 2026-09-30 by Q-O5: an answer that does not authenticate as the admitted incarnation restarts the process under the continuity meter; silent answers stay degraded-only.**

Options:
- (a) No. The route is marked degraded, dependents stop selecting it (Q2b),
  and challenges back off. Restart happens only on workload death, as today.
- (b) Yes, after N consecutive failures, under the continuity backoff.

Recommendation: **(a)** for this campaign. A process that answers 503 while
alive is a health-policy question, and making it restart authority mixes
observation back into actuation.

Depends on it: B2.

**Q6. Landing the CultLib Idunn TS runtime on `main`** (F15).

**RULED (a), with one Soul pass on the ten weekend commits before the merge by the operator, 2026-09-29.**

Options:
- (a) Merge `codex/fix-node24-ajv-esm` into CultLib `main` as-is, then cut C1
  on `main`.
- (b) Cut C1 on the branch and merge once.
- (c) Rewrite the package on `main`.

Recommendation: **(a)**. It makes the shared substrate honest before changing
it. This is a CultLib foundation merge, so the operator should rule on it.

Depends on it: C1 and the StreamPixels gitlink in C2.

**Q-B2-1. Does the continuity restart log move into `TargetSupervision` too?**

**RULED (a) by the operator, 2026-09-30.**

Options:
- (a) Move it. `ContinuityBackoff`'s own doc (`:907-908`) already says it "belongs to the target, not the
  generation". On the generation it survives only through the copy in `from_transaction` (`:2074-2076`).
  Moving it deletes that carry and key 20. The mint CAS pins the target record instead of racing the
  generation, which is the F3 shape. One record holds both of a target's meters.
- (b) Keep it on the generation, reshaped to the sliding log in place. The route meter still needs
  `TargetSupervision` for first deploys, so the meters split across two records and the carry stays.

Outcome: the continuity restart log lives in `TargetSupervision`; the restart ceiling reads the target's own
log; the carry compensator in `from_transaction` is deleted.

Depends on it: B2 (see the B2 rework map in section 4).

**Q-B2-2. When the route ceiling refuses a first or forward deploy before the fence, does `idunn up` fail or
wait?**

**RULED: fail at once, by the operator, 2026-09-30.**

Options:
- (a) Fail. The deploy is aborted pre-fence with "route actuation ceiling reached for <target>; reopens at
  <time>", and the command reports failed. The target is free at once.
- (b) Wait. A gate wait until `reopens_at`. The transaction owns the target for up to an hour, and nothing ends
  that wait before B5's deadlines.

Outcome: a deploy refused by the route/actuation ceiling before the fence fails immediately, with the reopen
time in the error.

Depends on it: B2.

**Q-R1. Does a routed target answer the stable-route challenge whatever its readiness class?**

**RULED (a) by the operator, 2026-09-30.** The question's first framing was confusing. The cause is that
Heimdall has *no responder*, not its readiness class.

Mechanism (read at `583b2a7`): readiness class decides Warming, Ready and whether Routing reads Odin. It never
decides whether the stable route is challenged. Routing installs and then requires `prove_stable_route` for
every target with `expected.route` (`:6147-6190`), Commit proves it again (`:6272-6290`), and supervision
challenges every routed admitted generation (`:5171-5270`). Heimdall's recipe (`d5acc94`) declares
`odin.verse-rendezvous` (Odin-correlated) and is routed, but never answers a route challenge. Heimdall is
stateful, so a failed proof leaves the candidate route installed for fail-closed retry (`:6171-6175`), which
past the fence is resumable forever (F17) until B5. Its first `idunn up` would hold the target with its lease
granted, and the failure would surface at the most expensive point.

Options:
- (a) Yes. The route declaration is the challenge declaration. That is today's rule, made legible and checked
  before the fence (cut R1). Heimdall gains a responder in owning-repo cut **H1**, the same shape as
  StreamPixels C2: bump `vendor/CultLib` to at least C1 (`30ee8b9`), answer `POST /cultnet/snapshot` through
  the signer's `answerRouteObservation`, and make `reportHealth` its single health owner. Idunn gains no
  schema change, and every routed provider keeps a signed route proof, which Q2(b)/B4 currency needs.
- (b) No, the class decides. An Odin-correlated routed target is admitted by membership only. That needs a new
  `RoutingEvidence` variant (a transaction and generation schema bump), class branches at Routing, Commit
  and supervision, and loses the proof that the admitted process answers through the stable listener. It also
  applies to streampixels-service, whose provider currency would then come from Odin, which is the C2 run 2
  failure made permanent and contradicts Q2(b).

Outcome: every routed target answers the stable-route challenge regardless of readiness class. R1 checks it
before the fence. H1 (Heimdall repo, needs CultLib C1) gives Heimdall a responder.

Depends on it: R1, H1, and Heimdall's first deploy.

**Q-B4. For an admitted, running provider, what makes it current enough to satisfy a dependent's deploy?**

**RULED in part by the operator, 2026-09-30: Idunn's own proof. One clause is held open.**

Options:
- (a) A live readiness proof from the provider's readiness authority. Odin-correlated providers need a current
  Ready correlation from Odin, so with Odin down or confused no dependent deploy is possible and C2 run 2
  fails by design.
- (b) Idunn's own current observation, whatever the class.
- (c) Admission plus workload alive, for every provider, with no current readiness evidence.

Ruled, from (b):
- A **routed** provider is current by Idunn's route proof, and not degraded (B2's Q5(a) state). This is Q2(b)
  as ruled, confirmed to override class.
- An **unrouted** provider is current by its admission plus a live workload observation of the admitted
  incarnation.
- Capabilities come from the signed presence in the route proof (routed) or the admitted Ready receipt
  (unrouted).

**Held clause, STRUCK 2026-09-30 (operator, in Q-O1): "a current Odin non-Ready word still excludes a provider" is rejected. B4 becomes B4′ (section 4, Odin-authority cuts). The original held text follows as history.** The recommendation's clause that "a current Odin non-Ready word still excludes a
provider" (a stale one would not). The operator: "the last few rulings on Odin are making my authority sense
tingle, it feels like Odin is doing lifecycle work when that's very much Idunn's wheelhouse." That clause stays
open pending the Odin-lifecycle authority audit, a separate Imagination pass. Until it is ruled, B4 carries
no Odin-veto rule either way.

Depends on it: B4′, no longer blocked (the audit landed as section 5).

**Q-O1. Who proves readiness for a target that declares the Odin discovery dependency?**

**RULED (a) by the operator, 2026-09-30: Idunn proves.** Idunn's own route challenge always proves readiness. A
discovery declaration (`odin.verse-rendezvous`) never picks a voucher. This re-opens and **supersedes Q1 (b)**;
the Q1 text above is history, not live design. Readiness classes disappear at A1 + A3.

Context (audit, section 5): Q1 (b) turned "declares `shared-infrastructure odin.verse-rendezvous`" into "Odin vouches
for readiness", so a discovery declaration selected a lifecycle authority. That reached daemon survival: a
continuity restart of ghostlight or streampixels-service waits in Warming with no end until Odin speaks (I2), and
Idunn's own boot re-proves every stored Odin receipt against Odin's key (I12). `F:\Projects\CLAUDE.md` forbids that
coupling. Every Odin-correlated target except raven-muninn is routed, so Idunn already owns the channel it needs.

Options:
- (a) Idunn's own challenge, always. The declaration stays a discovery and graph fact. A1, then A3. Kills I2's
  survival gap, strikes the Q-B4 clause, supersedes R1.
- (b) Keep Q1 (b). Keeps I1-I14 and O1-O3; continuity of ghostlight and streampixels-service needs Odin alive and
  correct; Idunn's boot needs Odin's anchor and key.

Outcome: (a). Depends on it: A1, A3, B4′, and the demotion of `ReadinessClass`.

**Q-O2. How does Idunn prove an unrouted target (today only raven-muninn)?**

**RULED (a) by the operator, 2026-09-30: a private challenge endpoint.** The binding declares a challenge endpoint
without a stable route, and Idunn challenges it directly. Cut A2, plus a Muninn responder (M1).

Options:
- (a) A binding-declared private challenge endpoint (A2 + M1).
- (b) Workload-alive only: no readiness proof; dependents can never see degradation, which contradicts Q5 (a).
- (c) Keep Odin correlation for unrouted targets only: A3 is then impossible.

Outcome: (a). raven-muninn is held today and has no continuity restarts, so there is no live regression while it
waits. Depends on it: A2, M1, and raven-muninn leaving the held state.

**Q-O3. The three admitted Odin-voucher generations once A1 installs.** **OPEN. NOT YET ASKED.** Needed before A1
installs.

Admitted generations on Odin receipts that A1 will hold as `WrongVoucher`: odin, streampixels-service, ghostlight
(streampixels-web is Odin-free already).

Options:
- (a) Redeploy each immediately after install, in the order odin -> streampixels-service -> ghostlight -> web, with
  the operator present for the window. No new code (the existing rule: "reported and held, never repaired"). Each
  is continuity-less for its redeploy window; held generations mint no continuity (B3 F1). Audit recommendation.
- (b) A one-shot relabel: supervision replaces the Odin receipts with a fresh stable-route proof once. No
  continuity gap, but it adds the repair path the hold rule was written to forbid.

**Q-O4. Does `shared-infrastructure odin.verse-rendezvous` still gate a dependent's plan compile?**

**RULED (b) by the operator, 2026-09-30: the Odin dependency does not gate a dependent's compile; declare it
optional.** It is a discovery need that never gates (I11). The service must tolerate Odin's absence at runtime and
publish when Odin returns.

Options:
- (a) Yes. Odin is an ordinary provider judged by Idunn's own proof of Odin (B4′); an Odin outage blocks deploys,
  not continuity, of Verse-advertising services.
- (b) No. Services declare it `optional`.

Outcome: (b). Depends on it: the ghostlight, streampixels-service and raven-muninn recipes (flipped inside A1's
window), and the I11 graph gate.

**Q-O5. A live process whose challenge answer does not authenticate as the admitted incarnation.**

**RULED (a) by the operator, 2026-09-30: restart on bad auth.** Supervision restarts a live process whose
challenge answer does not authenticate as the admitted incarnation (bad signature, wrong activation or instance,
lease not held), under the continuity restart meter in `TargetSupervision`. Silent answers stay degraded-only.
**This amends Q5 (a)** narrowly; it applies to every routed target, not only Odin.

Rationale (audit): a refused-as-unauthenticatable answer is Idunn's own evidence that what runs is not what it
admitted. That is survival authority, not health policy. It lets Odin's temporary fatal rule (section 4) be
deleted. Rejected: (b) Q5 (a) unchanged, with the temporary rule kept permanently.

Depends on it: a supervision change on B2's meter (no cut drawn yet; see the Cut status), and the deletion of
Odin's temporary rule.

---

## 4. Draft cut list

Order is deletions first, then one foundation, then behaviour cuts, then the
owning-repo cuts. Each cut is one Hands brief and one Soul pass.

### Subtraction

**S1 — Observation stops actuating (Idunn).**
- Delete the stale-observation → repair-intent → `restore_admitted_membership`
  path from `supervise_admitted_route`.
- Supervision becomes challenge-only: an exact fragment plus a stale
  observation yields exactly one `prove_stable_route_against`, with no write,
  no `nginx -t`, no private-mount `systemd-run`, no ufw and no reload.
- `restore_admitted_membership` runs only when the fragment on disk differs
  from the admitted membership. It stops re-admitting ufw when the fragment is
  unchanged.
- The `route_repair_started_at` field stays readable but its decision power is
  deleted. F0 retires it.
- Negative checks:
  - A failed proof with an exact fragment performs zero actuations.
  - A byte-identical fragment never reloads.
  - Drifted bytes reload once.
- It touches `supervise_admitted_route` and `NginxRouteDriver`. It does not
  touch the source-freeze and digest regions that `cut1-fix5` rewrites. The
  merge hazard is limited to the `drivers.rs` test module and imports.

**S2 — Delete the CLI post-fencing cancel (Idunn), after B5.** Remove
`cancel_is_safe_for_live_stateless` and the live-transaction branch of
`cancel` (`8ae00a1`). Pre-freeze command cancellation stays. The Q4 expiry
request replaces the rest.

**S3 — The web drops Odin (StreamPixels), after C1.** Remove the
`odin.verse-rendezvous` dependency and `STREAMPIXELS_ODIN_CULTMESH_RUDP` from
`deployment/idunn/web.toml`. Remove the web branch's Odin publisher,
`publishInitialWarming`, and the heartbeat publication from
`runtime-presence.mjs`. The service keeps its own under Q1(b).

### Foundation

**F0 — Typed readiness, deadlines, and actuation state (Idunn schema: one
migration).**
- Bump `idunn.admitted_generation` to v3 and `idunn.deployment_transaction` to
  v4, carrying:
  - the readiness-evidence enum;
  - route supervision state, replacing `route_repair_started_at`;
  - continuity backoff;
  - phase deadlines;
  - lease adoption evidence;
  - terminal recovery variants.
- The readiness class is derived from Expected (Q1).
- Migrate every existing v2 generation as `OdinCorrelated` with its current
  receipts, so behaviour does not change at this cut.
- Deadline durations become optional binding fields with Idunn defaults, and
  are frozen into the compiled plan.
- No decision reads the new fields yet, apart from S1's retirement of the old
  one.
- Tests: migration round-trip over a copy of the live store's shape. A v2
  record must decode to the same decisions.

### Behaviour

**B1 — A continuity abort cleans its own projection, and `a148802` is
reverted (Idunn).**
- The pre- and post-fencing abort intents set `topology_reconciliation =
  Pending` for Continuity.
- The resolution demotes the shared key to Expected-only with the
  transaction's **own** activation (exact), and never withdraws the Expected.
- Delete `demote_current_activation_to_expected_only`. Rollback demotes the
  admitted generation's exact activation.
- Negative check: a failed continuity followed by a deploy that fences and
  aborts resolves with the exact admitted activation, and no path adopts an
  activation the transaction did not issue.
- F0 is not required. This cut could land before F0 if Self wants the drift
  closed first.

**B1 status, 2026-09-29 (Self).** Landed on `idunn/route-b1` (`87fa57d` revert of `a148802`,
`4427769`, `4a9fe87`); 177 tests pass. **Soul (Opus): do not close.** The rule held (one
rule, one resolution, exact activation, the Expected never withdrawn, foreign activations
refused, the revert exact). The transition failed:
- **High, CONFIRMED:** a continuity abort persisted under the old rule (`Skipped` although
  an activation was issued) fails the new validation (`control_plane.rs:1031-1044`).
  `ControlSnapshot::read` fails the whole store on one record, so a resident record stops
  Idunn from booting, for every target.
- **Medium, CONFIRMED:** supervision's pre-restart demotion failure is only logged
  (`:3557-3576`). A continuity failing between `prepare_activation` and
  `publish_observed_activation` then wedges its abort on "substituted" every tick. Drift
  left by a pre-B1 failed continuity wedges a later deploy abort the same way.
- Low: `docs/deployment-authority.md` says the projection "never drifts".

**Self's ruling for the B1 fix batch, which runs on top of F0 because F0 owns the legacy lift:**
1. ~~(superseded below)~~ The legacy-transaction lift maps a pre-B1 continuity abort that issued an activation to
   `Pending`. The new single resolution then demotes its own activation, which also cleans
   the residue that record left behind.
2. Supervision owns the Expected-only precondition. It does not mint a continuity while its
   pre-restart demotion fails; the failure is recorded, not merely logged.
3. A one-time boot reconciliation demotes a projected activation only when its issuing
   transaction is a failed transaction in history (exact identity). It never adopts.
4. Engine-layer tests for both abort paths, using Soul's EngineFixture probe shape (candidate
   cleanup already Complete, so no systemctl runs). cargo-mutants found 6 Engine-path mutants
   missed.
5. Correct the doc's "never drifts".
Before any deploy, check the live host for resident continuity abort records. That is a read
of `control.cc`, and the operator's to authorise.

**B1 fix batch 1, Soul pass, 2026-09-29: do not close. Self's corrected ruling replaces item 1 above.**
Hands implemented item 1 by reopening terminal pre-B1 records at `Starting`. Soul confirmed the
cost:
- If the target also holds a live transaction, or a second resident pre-B1 abort, the reopened
  record fails `ControlSnapshot::read` and Idunn does not boot.
- The migration has already written the record as v4, so the old binary cannot roll back.
- The reopen is a second owner for residue that boot reconciliation already cleans.
The defect was in Self's ruling: it set a state on a record without saying what happens to
terminal records.

**Corrected ruling:**
- Nothing is reopened.
- A terminal pre-B1 abort keeps its phase. Validation accepts its shape under a **typed legacy
  marker** that only the lift sets.
- **Boot reconciliation is the single owner of legacy residue.**
- In-flight pre-B1 aborts may lift to `Pending` only if they cannot collide with another live
  transaction.

**Also fixed in batch 2:**
- A failed pre-fence abort goes to `record_resumable_error`, never to
  `begin_post_fencing_abort`. Before this, one wedged abort failed every scheduler tick for
  every target.
- Errors are isolated per transaction, including `archive_terminal_transaction`.
- `serve`'s boot wiring is pinned by a test.

**Moved to B2:** the continuity restart ceiling reads only the target's restart log in
`TargetSupervision`, never `history.cc`. Otherwise an unreadable history file stops crash recovery for every
target. Deferral and backoff show in `idunn status`.

**B2 — Bounded, backed-off route and unit repair (Idunn). READY FOR HANDS; rework map below.**

*Widened 2026-09-29 by Self, from Soul's S1 pass.* S1 closed the healthy-target storm, and
Soul found two storm paths S1 does not claim. Both are B2's:
- **A failing reload deletes the fragment** (`restore_admitted_membership`,
  `drivers.rs:5472-5478`). The next tick reads the file as missing, treats that as drift, and
  restores again: one `systemd-run`, ufw, `nginx -t` and reload every 500 ms, unbounded.
  This was probed over 4 ticks. A broken global nginx config does the same through the
  private-mount unit.
- **`NginxRouteDriver::install` (`drivers.rs:5402`) runs ufw, `nginx -t` and reload
  unconditionally.** The Routing phase re-runs it on every resume, and a post-fence proof
  failure resumes forever (F17), so a candidate failing its proof reloads every tick. A
  stateless candidate reloads twice (install, then rollback). The post-fence abort's
  `withdraw_candidate_membership` then calls `restore`, which does the same.

So B2's actuation ceiling covers **`install`, `restore` and `restore_admitted_membership`**,
not only supervision. B2 also adds the seam Soul specified: the route actuator program paths
and `preflight_root` on `RuntimeOptions`, defaulting to today's paths and used at every
`NginxRouteDriver::new` site, plus a routed `Stored<AdmittedGeneration>` fixture beside
`EngineFixture`. Deleting `supervise_admitted_route`'s body currently survives every test
(cargo-mutants), and B2's reload-count timeline test needs the same seam. A failed proof
marks the route degraded (Q5 a). Today nothing is written and the next tick re-challenges
with no backoff.

**Rework map, 2026-09-30 (Imagination pass, Opus).** Every `file:line` in this block is against Idunn
`583b2a7` (the code under main `94c7691`, installed on Yggdrasil). The salvage source is
`origin/idunn/route-b2` (`97f1ad2`, +1243/-200; merged with B1 at `1786ddf`). The rework is a fresh
branch from `583b2a7`, not a rebase of `1786ddf`. Rulings Q-B2-1 (a) and Q-B2-2 (fail at once) are in
section 3.

#### B2 rework, A. What B3 superseded, and what survives from `97f1ad2`

**Delete; do not port. B3 or this rework owns each of these now:**

- `routed_generation`, `hanging_up_listener`, `RoutedFixture`, `routed_transaction`,
  `store_seeded`, the `seeded_record` split, `route_proof_evidence()` hand evidence, and
  `DeadWorkload`.
  - B3's `RoutedWorld` (`control_plane.rs:14255-14660`) already builds a real route-proof
    admission, with a `RuntimeStub` at both endpoints and a `hang_up` switch (`:14068`).
  - `SwitchWorkload::kill` (`:11932-11945`) is the dead workload.
  - These are why B2 "does not compile on B3, and two tests fail because the fixture target is
    route-proof".
- `Engine::update_admitted_generation`, the re-read-then-CAS helper. It existed because B2
  charged the ceiling to the generation, so every other writer had to re-read to avoid losing
  the charge. The meter moves off the generation (section B), so writers CAS on the envelope
  they read. Tests use the existing `edit_incumbent` (`:12065`).
- `ActuationWindow::charge`: a fixed window anchored at the first charge. That is Soul's F4.
- The first-deploy exemption in `charge_route_actuation` (`let Some(current) = admitted_for
  else return Ok(())`). The 2026-09-30 ruling removes it.
- `ContinuityBackoff.deferral_reason` added as a v3 field. That is Soul's F1. The reason moves
  to the new record (section B).

**Port, reshaped as sections B-E say:**

- In `drivers.rs`:
  - `RouteActuators` and its `Default`;
  - `NginxRouteDriver::with_actuators`, with `new` delegating to it;
  - the gate trait threaded through every mutating method;
  - the `Unmetered` test gate;
  - `a_refusing_gate_stops_every_route_actuation_before_it_starts`, split in two (section E).
- In `control_plane.rs`:
  - `RuntimeOptions.route_actuators`;
  - `Engine::route_driver`, used at every construction site;
  - `EngineRouteGate`;
  - the `RouteStubs` fixture and `EngineFixture::routed`;
  - `RouteSupervisionState::{is_waiting, record_failed_challenge, record_proved_challenge}`;
  - the `supervise_admitted_route` restructure, where a failed repair or proof records state;
  - `render_supervision` plus the status loop;
  - the continuity ceiling replacing the history count;
  - the restart counted in the minting CAS;
  - the tests `continuity_restarts_are_backed_off_and_bounded_per_target`,
    `status_renders_…`, `a_deployment_yields_to_a_restart_only_while_the_target_has_restarts_left`,
    and the storm tests, rebuilt on `RoutedWorld`.
- Constants, kept from B2 because Soul did not dispute them:
  - `CONTINUITY_RESTART_ATTEMPTS = 6` per `CONTINUITY_RESTART_WINDOW_MILLIS = 3_600_000`;
  - `CONTINUITY_RESTART_BACKOFF_MILLIS = 5_000`, doubling;
  - `ROUTE_ACTUATION_CEILING = 12` per `ROUTE_ACTUATION_WINDOW_MILLIS = 3_600_000`;
  - `ROUTE_CHALLENGE_BACKOFF_CAP_MILLIS = 600_000`.

#### B2 rework, B. Owner map (ownership changes)

**Route actuation rate.**
- **Owner:** `Engine::charge_route_actuation(target, RouteActuation)`, over a new per-target record,
  `TargetSupervision.route_actuations`.
- **Inputs:** the target's record from a fresh control snapshot (absent means empty), `now`, and
  the kind: `Forward` or `Survival`.
- **Output:** `Ok(())`, or the typed `RouteActuationRefused { target, used, reopens_at }`.
  - Only `Forward` can be refused.
  - `Survival` is always recorded and always admitted (ruling: never refuse a rollback).
- **Demoted:**
  - `RouteSupervisionState.actuations` is dead, deleted in v4.
  - `RouteSupervisionState::for_new_incarnation` (`:885-893`) is dead. A new incarnation's
    route state is `default()`.
- **Forbidden writers:**
  - the driver, which only asks;
  - `AdmittedGeneration::from_transaction`, which no longer carries a meter;
  - the commit CAS.
- **Shared paths:**
  - Every `NginxRouteDriver` is built by `Engine::route_driver`. The seven `::new` sites are
    `:5211`, `:5701`, `:6151`, `:6280`, `:6985`, `:7323` and `:7652`.
  - Every mutating driver method takes `&dyn RouteActuationGate`, so none can be called
    without the meter. The compiler enforces it.

**Continuity restart pacing** (Q-B2-1 ruled (a)).
- **Owner:** the mint in `supervise_one_admitted_generation`, over
  `TargetSupervision.continuity_restarts` and `continuity_deferred_until`.
- **Forbidden writers and inputs:**
  - `history.cc`: the `history_for_decision` call at `:5003` goes;
  - the generation id: the count is no longer per generation;
  - `from_transaction`'s backoff carry at `:2074-2076`.
- **Demoted:** `AdmittedGeneration.continuity_backoff` (key 20) is dead, deleted in v4.

**Route challenge pacing** stays per incarnation, on the generation.
- **Owner:** `supervise_admitted_route`, over `RouteSupervisionState` without `actuations`.
- `route_repair_started_at_unix_millis` (key 18) is dead, deleted in v4.
  - Its last readers are the validate block at `:2139-2145` and the unrouted guard at
    `:5173-5176`.
  - Its last writer is the clear at `:5263`.

**One generation write per supervision iteration** (structural fix for F3). Any write to a
target's generation or to its `TargetSupervision` ends that target's iteration:
`progressed = true; continue`.
- `refresh_admitted_topology` already follows this rule (`:4827-4835`).
- The route challenge is the violator. Today a failure propagates as `Err` after B2's state
  write, and the restart path then CASes a stale envelope.

**New persistent type, `TargetSupervision`** (`idunn.target_supervision`, schema v1, in
`control.cc`, keyed by target).
- **Owner:** the meter primitives above.
- **Live consumers:**
  - the route gate (every install, preflight, rollback, withdraw and repair);
  - the continuity mint;
  - `status`.
- **Invariant:** a target's meters outlive every generation and exist before the first one.
- **Why no existing owner can serve:**
  - The generation does not exist for a first deploy (the ruling).
  - The transaction dies with each attempt.
  - Brakes are operator-signed and are not Idunn's to write.
- **What it replaces:** two carry compensators, `for_new_incarnation` and the backoff copy in
  `from_transaction`, plus a history scan.

#### B2 rework, C. Per file

The branch is fresh from `583b2a7`.

**Deletes first.**

`control_plane.rs`:
- `:63`: `CONTINUITY_RESTART_ATTEMPTS: usize = 3`. It is replaced by the constants in section A.
- `:876-893`: `ActuationWindow` and `for_new_incarnation`.
- `:906-926`: `ContinuityBackoff`. Q-B2-1 (a) moves it into `TargetSupervision`.
- `:1986-1988`: key 18.
- `:1992-1993`: key 20.
- `:2043-2047` and `:2074-2076`: the carries.
- `:2139-2145`: the key-18 validation.
- `:4974-5020`: the history-counted `is_refused_restart` and the `history_for_decision` read.
- `:5062-5071`: the deferral check on the generation.
- `:5173-5176`: the key-18 guard. Keep the `SkippedUnrouted` half.
- `:5263`: the key-18 clear.

Tests:
- `:9755-9801`, `continuity_gives_up_on_a_release_that_will_not_start`. It tests a copy of the
  production closure, so it proves nothing.
- `:10564`, `:11528-11542`: the F0 assertions on `ActuationWindow` and `attempts`. Rewrite them
  against `TargetSupervision::validate`.
- `:14622-14645`, `RoutedWorld::promote_by_hand`, and its 9 callers (`:14651`, `:14691`,
  `:14722`, `:14767`, `:14927`, `:14949`, `:15041`, `:15299`, `:15433`). They become
  `routed.step()`: the stub actuators let Routing run for real, so route-proof Routing gets
  tested for the first time.

**Schema: generation v4, one bump.**
- `ADMITTED_GENERATION_SCHEMA` (`:55`) becomes `"idunn.admitted_generation.v4"`. The macro at
  `:1939` changes to match.
- `:56`: add `ADMITTED_GENERATION_SCHEMA_V3`.
- The v4 layout:
  - keys 0-17 are unchanged;
  - 18 is a gap;
  - 19 is `Option<RouteSupervisionState>` without `actuations`;
  - 20 is a gap;
  - **21 is `last_error: Option<String>`**, the dead-hold ruling's field. It is written by D1,
    and B2 carries the slot so D1 needs no second bump.
- The cultcache derive encodes slots positionally, so a gap is a nil and any added slot changes
  the array. That is why F1's unbumped field broke the canonical re-encode (`decode_record`,
  `:2811-2821`), and why every change here rides one bump.
- **`LegacyAdmittedGenerationV3`**: move today's `AdmittedGeneration` (`:1936-1995`),
  `RouteSupervisionState` and `ActuationWindow`, and `ContinuityBackoff` verbatim under
  `Legacy*` names beside `LegacyAdmittedGeneration` (`:2967-3049`).
  - Its `into_current()` drops key 18, `actuations`, `window_started_at`, `attempts` and
    `next_restart_at`.
  - It **refuses** a v3 record whose `actuations.count != 0` or `attempts != 0`.
  - Why that is lossless (read): no released binary ever wrote those fields. On `583b2a7` the
    only non-test writes are the carry at `:890`, which carries a default, and the ≤30 s
    deferral `next_restart_at` at `:5154`. B2 was never installed.
  - So the lift creates no `TargetSupervision`. Every target starts with an absent record, and
    the refusal makes any future contradiction loud instead of silently dropped.
- The v2 lift (`:3016-3048`) targets v4 directly: the same body without the retired fields.
  Keep it; section H explains why this is not a question.
- `read_generation_record` (`:3102-3121`): add a v3 arm.
- The migration filter at `:3147-3148` adds `ADMITTED_GENERATION_SCHEMA_V3`.

**New record, `TargetSupervision`**, beside `AdmittedGeneration`:

```rust
struct TargetSupervision {           // idunn.target_supervision / .v1, key = target
    #[cultcache(key = 0)] schema_version: String,
    #[cultcache(key = 1)] target: String,
    /// Newest ≤ ROUTE_ACTUATION_CEILING actuation times, ascending.
    #[cultcache(key = 2)] route_actuations: Vec<u64>,
    /// Newest ≤ CONTINUITY_RESTART_ATTEMPTS restart times, ascending.   (Q-B2-1 (a))
    #[cultcache(key = 3)] continuity_restarts: Vec<u64>,
    #[cultcache(key = 4)] continuity_deferred_until: Option<u64>,
    #[cultcache(key = 5)] continuity_deferral_reason: Option<String>,
}
```

The record threads through several places:
- `ControlSnapshot` (`:2564-2568`) gains `targets: Vec<Stored<TargetSupervision>>`.
- The `read` match at `:2580-2610` gains its arm. Without it, the match's "foreign document"
  bail at `:2609` refuses the store.
- `validate_relations` checks unique targets.
- `target_supervision_envelope` goes beside `admitted_envelope` (`:3198`).

**Meters.** A sliding log, not a window (fixes F4):
- `charge(log, now, window, limit, kind)`:
  1. Clamp any entry greater than `now` to `now` (fixes F5; see below).
  2. Count the entries within `window`.
  3. For `Forward`, if the count is at least `limit`, refuse with
     `reopens_at = log[len - limit] + window`.
  4. Otherwise push `now` and keep only the newest `limit` entries.
- Keeping the newest `limit` entries is exact for "at most `limit` Forward in any `window`",
  even after Survival pushes past the limit.
- The restart log uses the same primitive. The next restart is due at
  `last + BACKOFF << (n - 1)`, where `n` is the number of entries in the window.

**Clock steps.** Fixes F5, and collapses one helper.
- `is_waiting(now, not_before)` (`:3351-3353`) becomes `is_waiting(now, not_before,
  longest_wait) = now < not_before && not_before - now <= longest_wait`.
  - A due time further ahead than any wait this owner can set means the clock stepped back,
    so the attempt is due.
- Callers:
  - resume backoff at `:4537`, with `RESUME_BACKOFF_CEILING_MILLIS`;
  - the route challenge, with `ROUTE_CHALLENGE_BACKOFF_CAP_MILLIS.max(max_age)`;
  - the restart wait, with `BACKOFF << (ATTEMPTS - 1)`;
  - the deferral, with `CONTINUITY_DEFERRAL_MILLIS`.
- The log clamp bounds a backwards step to one window, never the length of the step.

**`drivers.rs`.**
- Port `RouteActuators` from `97f1ad2` at `:5049-5068`.
- The trait becomes `RouteActuationGate::admit(&self, kind: RouteActuation) -> Result<()>`,
  with `pub enum RouteActuation { Forward, Survival }`.
- Gate the methods:

| Method | Kind |
|---|---|
| `preflight` (`:5293`), before `validate_candidate_in_private_mount` at `:5315` | **Forward** (F7) |
| `install` (`:5338`), before the write at `:5360` | **Forward** |
| `restore` (`:5268`) and `fail_after_rollback` (`:5279`) | **Survival** |
| `withdraw_candidate_membership` (`:5409`) | **Survival** |
| `rollback` (`:5555`) | **Survival** |
| `restore_admitted_membership` (`:5420`), after the exact-bytes return at `:5430` | **Survival** |

- `install`'s own rollback, through `fail_after_rollback`, therefore charges a Survival even
  when it follows a Forward.

**`control_plane.rs` Engine.**
- `RuntimeOptions` (`:2160`, `:2202`): add `route_actuators`.
- Add `route_driver` and `route_gate` as in `97f1ad2`.
- `charge_route_actuation`:
  - reads a fresh snapshot;
  - CASes the target's record, with `current: None` when absent (the first deploy);
  - is typed as described above.
- **A refused Forward is an error of its phase.**
  - Warming's preflight (`:5690-5711`) comes before the fence, so the refusal becomes a pre-fence abort
    (`:4587-4593`) whose error names `reopens_at` (Q-B2-2 ruled: fail at once).
  - Routing's install comes after the fence, so it stays resumable. The resume backoff spaces
    it, and no actuation runs.
  - Until B5 lands, a post-fence candidate whose install keeps being refused waits for the
    window. That wait is bounded by the window, and it runs no programs.

**`supervise_one_admitted_generation` (`:4787-5145`)**, in order:

1. Hold (`:4960-4973`, unchanged).
2. Blocker yield (`:5025-5043`): `refused_restarts < ATTEMPTS` becomes
   `!restarts_exhausted(now)`.
3. Exhausted: `eprintln` once through a `ReportOnce` keyed `continuity:<target>`, then
   continue.
4. Waiting or deferred: continue.
5. Demotion (`:5072-5098`). On failure, write `continuity_deferred_until` and the reason to
   `TargetSupervision`, then `progressed = true; continue`.
6. Lifecycle brake (`:5105`, unchanged).
7. Mint. The CAS at `:5114-5142` gains the `TargetSupervision` expected envelope, `None` when
   absent, and its next value with `now` charged to the restart log and the deferral cleared.

A brake-parked target spends no attempt.

**`supervise_admitted_route` (`:5171-5270`).**
- At the top: if `route_supervision.is_waiting(now, cap)`, return `Ok(false)`.
- Port B2's challenge closure: the Survival-gated restore, prove, and re-observe.
- On `Err`:
  - write `record_failed_challenge` to the generation, CAS on `current.envelope`;
  - print once through a `ReportOnce` keyed `route:<target>`;
  - return `Ok(true)`. This is the F3 fix.
- On `Ok`: write the receipt and `record_proved_challenge`, return `Ok(true)`.
- If the Survival repair itself charged `TargetSupervision`, that is a second record, not the
  generation, so the generation CAS still holds. The iteration ends either way.

**`status` (`:3668-3756`).**
- After the command loop, print one block per target: the union of admitted generations and
  `TargetSupervision` records.
- Port `render_supervision`, reading the restart and route logs from the target record:
  - `route actuations N/12 in window, reopens-at …`;
  - `continuity restarts N/6 … next-restart-at … deferred: <reason>`.
- A metered target with no generation prints its meters alone. That is the first-deploy case.

#### B2 rework, D. B2 verification

Each ruling or finding is pinned by a test that fails under its own mutation. All tests are
Engine-level on `RoutedWorld` with `RouteStubs`, `#[cfg(unix)]`, except where marked unit.

| Ruling / finding | Test | Mutation it must kill |
|---|---|---|
| Ruling: never refuse a rollback | `a_full_ledger_still_restores_the_admitted_and_incumbent_route`. The ledger holds 12 entries inside the window. (i) A post-fence abort's withdraw runs `systemctl reload` once and pushes an entry. (ii) Supervision repairs a drifted admitted fragment the same way. (iii) A Routing proof failure's `rollback` does too. | Survival treated like Forward (refused), which drops the reload; Survival not recorded, which drops the entry |
| Ruling: always counted | the same test, asserting the ledger's newest entry is `now` after each Survival | skipping the push for Survival |
| Ruling: first deploys metered | `a_first_routed_deploy_is_metered_and_refused_at_the_ceiling`. `RoutedWorld` seeds with no incumbent (`:14352`). (i) `run_to_routing` then one step creates `TargetSupervision` with 1 entry. (ii) A world whose record is pre-filled to 12: install fails with `RouteActuationRefused`, and the stub log has no `ufw`, `nginx` or `systemctl`. | the B2 early return "no admitted generation means unmetered" |
| F7 | `a_refused_preflight_starts_no_private_mount_unit`, a full ledger: the Warming preflight runs no `systemd-run` and the transaction pre-fence aborts with the reopen time | preflight's gate call deleted |
| F4 rolling | unit, `the_actuation_log_slides`: 12 charges at t, t+1…t+11. At t+window only one slot reopens (the 13th charge succeeds, the 14th is refused until t+1+window). | fixed window anchored at the first charge |
| F5 | unit, `a_clock_stepped_back_stalls_nothing_past_one_window`: log entries and due times at T, with `now = T − 1 day`. The Forward reopen is ≤ now + window; the route challenge, restart and resume waits read as due. | the clamp removed; the `longest_wait` guard removed |
| F3 | `a_failed_challenge_and_a_dead_unit_in_one_tick_do_not_lose_a_cas`. A drifted fragment, a `hang_up` stub and a killed `SwitchWorkload`. Tick 1 returns `Ok` and records the failure; tick 2 mints the restart. | a route failure returns `Err`, or falls through without `continue`: the mint CAS loses and the tick errors |
| F6 | `a_proved_challenge_clears_the_degradation`: `hang_up` on, one tick (degraded, failures 1); `hang_up` off, wait made due, one tick (failures 0, next None, degraded None) | `record_proved_challenge` call deleted |
| F1 | `a_v3_generation_lifts_to_v4_once_and_reencodes_canonically`, over **new golden fixtures** `generation-v3-odin.hex` and `generation-v3-route-proof.hex`, cut with the `583b2a7` encoder (add them to the fixture README). The migration counts 1, then 0. After it, `decode_record` accepts the bytes, and the decisions are equal apart from the retired fields. A v3 fixture edited to `attempts = 2` is refused. | the v3 schema dropped from the migration filter; the refusal removed |
| Restart ceiling reads no history | rename `unreadable_history_stops_continuity…` (`:13516`) as B2 did: a corrupt history still restarts, and the target's own log exhausts it | re-adding `history_for_decision` |
| Backoff and window | port `continuity_restarts_are_backed_off_and_bounded_per_target` on `SwitchWorkload` | halving the wait; resetting the log on a new generation |
| Storm (a) | port `a_failing_reload_that_deletes_the_fragment_backs_the_route_off`: 200 ticks give one reload | the `is_waiting` gate deleted |
| Storm (b) | port `a_candidate_whose_route_proof_fails_reloads_at_most_the_ceiling`: Forward installs are capped at 12 | the install gate deleted |
| Driver gates | split B2's refusing-gate test in two: Forward refused runs no program; Survival under a gate that "refuses" Forward still runs | a gate call moved after the write |

- Run `cargo mutants --in-diff` on the cut. The target is 0 missed in the meters, the
  migration and the supervision edits.
- `a_failing_step_is_retried_after_a_backoff_not_every_tick` is flaky under a full parallel run
  on the base too. Soul should not charge it to this cut.
- The live timeline probe, read-only, before and after install:
  - `journalctl -u nginx --since -1h | grep -c 'Reloaded nginx'` stays at the C2 baseline of
    about 0 per hour while steady;
  - `idunn status` shows every routed target with `route actuations 0/12` and a healthy route.

#### B2 rework, E. Live-deploy relevance (read from migration code, not from the store)

**What is live now (ship log plus code).**
- `control.cc` generations are v3: F0's lift (`:3131-3181`) ran at the B3 install and migrated
  5 records.
- Transactions are v4.
- `history.cc` holds v3 and v4 transactions and is never migrated (`:3388-3391`).
- B2 changes no transaction schema.

**What the B2 install does.**
- At boot, the migration rewrites every v3 generation as v4, each by CAS against its exact
  envelope. That is the same machinery F0 used.
- It writes no `TargetSupervision`: the lift is lossless for the reasons in section C.
- The first route actuation after install, a deploy or a repair, creates each target's record.

**It is irreversible.**
- The pre-B2 binary refuses a v4 generation (`:3120`).
- It also refuses a `TargetSupervision` envelope: `ControlSnapshot::read` bails "foreign
  document" at `:2609`.
- Binary rollback therefore means restoring the backup.
- Ship steps: back up `control.cc` and `history.cc`, install, then check that `idunn status`
  renders every target.

**A lift refusal fails boot for every target.** It cannot happen from any released binary. If it
does, the backup plus the pre-B2 binary is the recovery, and the error names the record.

**Raven.**
- `idunn-host` decodes plans, not generations, so B2 needs no actuator rebuild.
- W1 is still the check that the actuator builds at the sha that ships.

#### B2 rework, F. Subtraction estimate (B2)

Production code in `control_plane.rs` and `drivers.rs`:
- **Removed:**
  - about 45 lines of history ceiling;
  - about 40 lines of `ActuationWindow`, carry and backoff;
  - about 20 lines of key-18 and key-20 plumbing.
- **Added:**
  - about 90 lines for `TargetSupervision`, its snapshot arm and its validation;
  - about 60 lines for the meter primitive and the clock guard;
  - about 60 lines for the v3 legacy layout, mostly moved rather than written;
  - about 60 lines for the gate plumbing and `RouteActuators`;
  - about 50 lines for status.
- **Net production:** about +250.

Tests:
- **Removed:** about 46 lines of the tautology test and about 24 lines of `promote_by_hand`.
- **Net tests:** about +350. The 9 hand promotions become real Routing steps.

Compared with `97f1ad2`, which was +1243/-200, about 600 of B2's lines are not ported.

Structural delta:
- One record type added.
- Two carry compensators and one history dependency deleted.
- Two generation keys retired.
- No targets, crates or dependencies.
- The build and test matrix is unchanged, apart from W1's check.

**Not asked, decided.** The v2 generation lift stays, retargeted to v4. Its only consumer is a
restore of `/root/idunn-store-backup-…-pre-route-b3`, and deleting it would force rewriting
about 10 tests built on `FIXTURE_GENERATION` for a saving of about 80 lines. Retire the v2 and v3
generation layouts together in one later cut, once the operator retires the pre-B3 and pre-B2
backups.


**D1 — The dead-hold report lives on the generation (Idunn; operator ruling, 2026-09-30). READY FOR HANDS, after B2** (B2 carries key 21 and `render_supervision`).

**Owner:** `Engine::report_generation_hold(current, detail)`. It is the only writer of
`AdmittedGeneration.last_error`.
- It writes only when the text differs.
- It prints once through the existing `ReportOnce` map (`:3865`), keyed by target.
- It ends the iteration, per the rule in section B.

**Writers today, by site.**
- `note_fault("holds admitted generation", "generation:<t>")` at `:4793-4799` hands a
  generation key to `record_last_error` (`:4472-4495`). That function looks it up among
  *transactions*, finds nothing, and so the report is stderr only. It also never clears.
- The dead-hold note at `:4965-4969` behaves the same way.
- **Both go.** They are replaced by `report_generation_hold`, with the readiness disagreement,
  or with "is not running; declare readiness in its recipe and redeploy" when dead.

**Clearing** (the ruling: replaced, or hold released).
- Replacement clears by construction. `from_transaction` sets `None`, so it needs no code.
- Release: at the hold check, `readiness().is_ok() && last_error.is_some()` writes `None`, calls
  `clear_fault`, and continues.
  - This covers an Idunn upgrade that reclassifies a generation.
  - `last_error` has no other writer: the continuity deferral reason lives in
    `TargetSupervision`, and route failures live in `RouteSupervisionState`.

**Status.** `render_supervision` prints `  held: <last_error>`.

**Tests.**
- Extend `a_held_generation_that_dies_mints_no_continuity_and_leaves_its_target_free`
  (`:13842`). It currently reads the in-memory `fault_reports` at `:13858-13859`; it must read
  `last_error` from the stored generation instead: first the held text, then, after
  `SwitchWorkload::kill`, the dead text.
- Add `a_released_hold_clears_its_report`: seed `last_error` on a generation whose readiness is
  OK; one tick clears it, and a second tick writes nothing, with the envelope unchanged.
- Mutations it must kill:
  - dropping the clear;
  - writing on every tick;
  - routing back to `record_last_error`.

**Size.** About +40 production lines and about +60 test lines. It deletes the two
`generation:`-keyed `note_fault` calls.

**Live.** No schema change. On install, the first tick writes `last_error` on any held generation
(none per the 2026-09-30 decode, now that raven-muninn is admitted).

**T1 — Pin the candidate-cleanup rule by removing its free arguments (Idunn; Soul follow-up on B3). IN PROGRESS on `hands/idunn-w1-t1-t2`.**

**The defect.**
- `pre_fencing_abort_intent` (`:7991-8005`) and `post_fencing_abort_intent` (`:8007-8039`)
  each call `candidate_cleanup_requirement(activation.is_some(), workload.is_some())`
  (`:8041-8050`).
- The unit test (`:9806-9820`) never covers `(false, true)`, and no test drives either intent
  with exactly one of the two set.
- So swapping or constant-folding either argument at either call site survives.

**The cut.**
- Delete the helper.
- Add `DeploymentTransaction::candidate_cleanup_owed(&self) -> CleanupEvidence`, used by both
  intents.
- Rewrite the unit test as a four-row table over the method.
- Add one test that both intents take the method's answer for `(Some, None)` and `(None, Some)`.

**Mutations it must kill:**
- `||` becoming `&&`;
- either operand becoming `false`;
- either intent hardcoding `Skipped`.

**Size.** About −10 production lines and about +8 test lines. Not live-relevant.

**T2 — The chunk-size line is exactly RFC 9112 (Idunn; Soul follow-up on B3). IN PROGRESS on `hands/idunn-w1-t1-t2`.**

**Probe** (Yggdrasil, throwaway commit `ce98e9b` on `583b2a7`): the HTTP reader accepts `" 3"`,
`"3 "`, `"3 ;x"` and `"3"` alike. The cause is `size.trim_matches([' ', '\t'])` in
`drivers.rs:5816-5821`.

**What RFC 9112 7.1 allows.** `chunk-size [ chunk-ext ] CRLF`, where
`chunk-ext = *( BWS ";" … )`. So whitespace is allowed only before a `;`, and never before the
digits.

**The cut.**
- Split at the first `;`.
- Trim trailing SP and HTAB only when an extension follows.
- Never trim leading whitespace.
- `parse_digits` (`:5846-5851`) stays the digit authority.

**Tests.** Add rows to `an_http_answer_that_frames_its_body_wrongly_is_refused` (`:8611`):
`" 3"` and `"3 "` are refused. Add to
`a_chunked_answer_must_end_its_trailers_and_terminate_each_chunk` (`:8799`) that `"3 ;x"` and
`"3\t;x"` are taken.

**Mutations it must kill:**
- trim both ends, which lets `" 3"` pass;
- no trim, which refuses `"3 ;x"`.

**Size.** About +4 production lines and about +8 test lines. It is live-relevant only as
stricter parsing of StreamPixels and Odin answers. Node's `http` emits bare hex, so there is no
expected behaviour change.

**Note.** Neither T1's nor T2's text is recorded in the map at `94c7691`. Both were taken from
the brief, and the defects were confirmed from code and the probe.

**W1 — Every verification checks the Windows actuator compiles (Idunn map and the eureka stopgap image). IN PROGRESS on `hands/idunn-w1-t1-t2`.**

**Probe** (Yggdrasil, rust image):
- `rustup target add x86_64-pc-windows-gnu; cargo check --locked --target
  x86_64-pc-windows-gnu --bin idunn-host`.
- At `d32395a` (B3 merge, before the fix) it **fails** with four `E0425` errors, "cannot find
  value `FROZEN_SOURCE_SYMLINK_TARGET_LIMIT`" (`drivers.rs:1461-1477`). That is exactly the
  Raven break.
- At `583b2a7` it **passes** in 26 s. No mingw linker is needed, because `check` does not link,
  and no dependency compiles C for the target.

**The cut.** No Idunn code changes.
1. `~/.claude/skills/eureka/tools/stopgap/rust.Dockerfile` gains
   `RUN rustup target add x86_64-pc-windows-gnu`, so no job downloads `rust-std`. Rename the
   file over itself; the image is re-tagged by the file's hash.
2. The map's "Build and verification path" makes the check a required step for every Idunn
   cut, beside `cargo test`.
   - It must not use `-D warnings`: the Windows target currently emits 9 warnings (unused
     cfg-split code).
3. When the Idunn verify recipe exists (verify campaign), the same step moves into it, and
   this stopgap line dies with `ygg-verify.sh`.
4. **The release build stays a documented step, not a verification.**
   - `gamecult-ops/runbooks/idunn-host-raven.md:23` already names Starfire for
     `cargo build --release --bin idunn-host`. Add: "at the exact Idunn sha being installed on
     Yggdrasil; record the exe sha256 in the ship log".
   - That is Windows-only work. It fits the load budget: one job, no burners.

**Honesty limit.**
- W1 catches compile rot, the class that broke Raven.
- It does not catch link failures or runtime behaviour on Windows. Q-V5 rules out a Windows
  verify host.
- It does not catch version skew, where an old actuator cannot decode new plan types. That was
  the other half of the Raven incident. Only the runbook's "rebuild at the installed sha" step
  covers it. A version handshake at hub attach would be the structural fix; it is noted and not
  mapped here.

**Size.** One Dockerfile line and one map line. It adds no targets.

**R1 — NEVER TO BE WRITTEN: superseded by A1 (2026-09-30, Q-O1).** Every routed target is proved by Idunn's own challenge whatever it declares, so the class guard below has nothing to guard. H1 (Heimdall responder) is still needed. The text is kept as history.

**R1 (superseded) — A routed target proves it answers the route challenge before its fence (Idunn; Q-R1 ruled (a)).**

- **Owner:** Warming's last step, the transition to Fencing (`:5742`).
- **Inputs:**
  - the candidate endpoint;
  - `ReadinessClass::of(expected)`;
  - `warming`: `RouteProofDirect` and `FirstOdinDirect` are already a direct answer.
- **Rule.** If `expected.route.is_some()` and `warming` is `OdinTopology`, then
  `challenge_candidate(&current.value, &["warming", "active"], None)` (`:7313`) must answer
  before `transition(Fencing)`:
  - `Silent` becomes `record_gate_wait`, since the candidate may still be binding;
  - `Refused` becomes `Err`, which resume turns into a **pre-fence abort** (`:4587-4593`) with
    "routed target <t> does not answer the stable-route challenge; a routed recipe must serve
    CultNet snapshot challenges";
  - an `Answered` result is not persisted. It is not readiness, and passing Warming is the
    record.
- **Deletes:** none. This is a check moved earlier, not a second authority. The post-fence
  proofs at Routing and Commit stay the route's admission.
- **Size:** about +20 production lines, no schema change.
- **Tests** (Engine, B3 `RoutedWorld` with `provides_odin = false` and an Odin-correlated
  recipe):
  1. The stub's `reply` answers 404: the transaction pre-fence aborts from Warming, no lease is
     granted, and nothing is fenced.
  2. `hang_up` on: a gate wait, not an abort. Then `hang_up` off: it reaches Fencing.
  3. An honest stub reaches Fencing exactly as today.
  - Mutations it must kill: the class guard widened to `RouteProof` only (test 1 then reaches
    Fencing); `Silent` treated as `Refused` (test 2 aborts).
- **Live:** nothing changes for ghostlight or streampixels-service, which answer today. Heimdall
  (not admitted) now fails in Warming instead of wedging post-fence.

**B3 — Route-proof readiness (Idunn).** For a route-proof target:
- Warming is a direct candidate-endpoint challenge. This generalizes the
  `FirstOdinDirect` path and `request_candidate_runtime_presence` beyond Odin.
- Ready is a direct candidate challenge returning `active`.
- Routing and commit use the stable challenge.
- Commit writes `RouteProof` readiness.
- No Odin read occurs anywhere on the path, including the graph gate.
- Odin-correlated targets are unchanged.
- Negative check: with Odin stopped, a route-proof target admits, restarts, and
  keeps its route. An Odin correlation about it has no effect on any of its
  decisions.

**B4 — Provider currency from Idunn's own observation; `9f00e7a` reverted
(Idunn). Now B4′ (section 4, Odin-authority cuts), unblocked by the audit and Q-O1: its unrouted Odin-class branch and the Odin-veto clause are gone.**
- Per Q2(b), `validate_selected_providers_current` and plan compile read the
  provider's current route observation within max age, and read capabilities
  from its signed presence.
- Unrouted providers use their Q1 class evidence, authenticated now.
- Revert the admission-time authentication.
- `ManagedReady` carries a class-tagged evidence digest.

**B4 status, 2026-09-30: was blocked on the Odin-lifecycle authority audit; now B4′ after A1** (a separate Imagination pass), not on
Q-B4. Q-B4 is ruled in part (section 3): routed providers are current by Idunn's route proof and not
degraded; unrouted providers by admission plus a live workload observation. The clause "a current Odin
non-Ready word still excludes a provider" is held open. B4 also takes B2's degraded state as input.

**C2 run 2 failure mechanism (Eyes, 2026-09-30)**

The code was read at `583b2a7`. The Idunn journal on Yggdrasil was read read-only; no state store
was read.

**Journal evidence.**
- 21:15:18: "excluded non-current provider streampixels-service: admitted provider's latest
  receipt is not Ready at its admission time".
- 20:44 and 21:15: "…streampixels-web: admitted generation of streampixels-web is not
  Odin-correlated".
- ghostlight and raven-muninn are excluded with the "latest … not Ready" text on every compile
  since 19:45, with Odin up.

**The path.**
1. Plan compile (`advance_sealing`, `:5375-5385`) builds its provider list only from
   `current_ready_provider_tokens` (`:7398-7415`), which calls `rehydrate_admitted_ready`
   (`:7417-7470`).
2. `rehydrate_admitted_ready`:
   - requires Odin receipts (`odin_receipts()`, `:7422`). A route-proof provider fails here with
     `:2019`, so **no route-proof target can be a provider today**;
   - requires the admitted `latest_odin_observation` to be semantically Ready (`:7453-7456`).
3. `refresh_admitted_topology` writes *every* fresh authenticated correlation as `latest`, Ready
   or not (`:5320-5325`). Only a Ready one also moves `ready`.
   - So one non-Ready word from Odin removes the provider until Odin says Ready again.
   - Such a word can come from the provider's presence ageing out in Odin, including through
     Odin's own faults, for example its 64-slot RUDP session table.
   - With Odin stopped, that last word is frozen.
4. `managed_ready_provider_refs` (`deployment_plan.rs:264-316`) takes capabilities from Odin's
   record. `select_dependencies` then bails "no expected provider satisfies
   streampixels.service.api http.v1 v1" (`deployment_plan.rs:447-455`).
5. The same rehydration gates currency again at Starting, Routing and Committing, through
   `validate_selected_providers_current` (`:7472-7534`). So a deploy that compiled can still
   fail later on Odin's word.

**This is the unlanded B4.** Q2 was ruled (b): "Idunn's own current route observation of the
admitted provider … Unrouted providers fall back to their Q1 class evidence".
- streampixels-service is routed, so under B4 its currency is Idunn's route proof. C2 run 2
  would admit.
- "Both runs must admit" is therefore **blocked on B4**, not wrong.

**One B4 brief item found here: challenge cadence against max age.**
- S1 challenges a healthy route only once its observation is older than
  `topology_maximum_age_millis` (30 s) (`:5213-5221`).
- So a healthy route's observation ranges from 0 to about 30.5 s old.
- A B4 check of "within max age", repeated at compile, Starting, Routing and twice at Commit,
  would sometimes refuse a healthy provider.
- B4 must define currency as "the route is not degraded, and the last proof is within 2 × max
  age", or challenge on demand. That is a brief decision, not an operator one.

**B5 — Every post-fencing phase ends (Idunn).** *2026-09-30: B5 loses its Odin arm (the Odin part of the F16 fix) once A3 lands; route-proof targets never entered that path.*
- A deadline resolver runs before `advance_transaction`.
- For stateless targets, expiry aborts to the incumbent through the existing
  post-fence path.
- For stateful targets, expiry resolves per Q3.
- A granted lease with no adoption evidence at its deadline becomes the named
  failure `lease-not-adopted`.
- Delete resumable-forever: `record_resumable_error` after Fencing only
  annotates until the deadline.
- Fix the F16 livelock. Commit from the topology admitted **after** the final
  proof when it is still Ready, instead of returning. Route-proof targets never
  enter that path.
- Add the Q4 expiry request.
- Negative checks:
  - An AwaitingReady state with no evidence resolves at its deadline.
  - An unadopted lease resolves without an operator.
  - Commit completes while Odin re-stamps every tick.

### Odin-authority cuts (Imagination audit, 2026-09-30)

Source: the Odin-lifecycle authority audit (anchors Idunn `94c7691`/`583b2a7`, Odin `5c37860`), folded in as
section 5. It ends B4's "blocked on the audit" state. Sequenced against the B2 rework map. Line counts are
estimates from function spans.

| # | Cut | Repo | Blocked by | Subtraction / addition | Live relevance |
|---|---|---|---|---|---|
| **X1** | Delete the dead lifecycle residue: O12 (`idunn.rs` plus the `idunn.*` schemas in `documents.rs`), O13 (JS coordinator, its tests, `package.json start`, `start-/restart-odin.ps1`), O14 (the other services' deploy, restart and health scripts), O15 (doc sections rewritten to the live body); `gamecult-ops/compose/odin.yggdrasil.yaml`; the `idunn-deployment-targets.ps1` references to Odin scripts. **IN PROGRESS on Odin `hands/odin-x1-residue`** | Odin, ops | nothing | about **-7,000** lines (672 + 2,814 + 140 + 3,208 + docs), +~40 of docs | none. Muninn keeps compiling at its pin; its next bump must drop `IdunnDaemonHealthRecord` (follow-up **M2**) |
| **A1** | Every routed target is proved by Idunn's own challenge, whatever it declares. `ReadinessClass::declared`: `routed -> Direct`. OdinSelf and OdinCorrelated apply only to unrouted targets until A2. Odin itself is routed, so its FirstOdinDirect path generalizes; delete the "is Odin" arms in Warming (`:5614-5640`). The recipes' Odin dependency flips to `optional` (Q-O4) | Idunn (+ recipes) | Q-O3; after B2 | about -60 / +20 | admitted ghostlight, streampixels-service and odin carry Odin receipts and become `WrongVoucher`-held on install (Q-O3). Continuity of ghostlight and service no longer needs Odin (fixes I2 for them). **Replaces R1**, which is never written (its class guard has nothing to guard). H1 (Heimdall responder) is still needed |
| **B4′** | B4 as mapped, with its rule collapsed: provider currency is Idunn's own proof, not degraded, last within 2 x max age. `ManagedReady` carries a direct-evidence digest. Capabilities come from the challenged presence. **The held Q-B4 clause is struck** | Idunn | A1 (or with it) | B4's own estimate, minus the unrouted Odin branch | C2 run 2 admits |
| **A2** | Unrouted targets get a challenge endpoint. The binding declares a private challenge endpoint without a stable route; `request_runtime_presence_at` (`drivers.rs:5488`) is reused against it; Expected carries it digest-bound. **M1** (Muninn): raven-muninn answers `SnapshotRequest` for its presence on that endpoint (the StreamPixels C2 and Heimdall H1 shape) | Idunn, Muninn, ops binding | Q-O2 (ruled) | about +80 Idunn, +~60 Muninn | raven-muninn gains a proof path and leaves the held state after a redeploy |
| **A3** | Delete Odin consumption from Idunn entirely: every forbidden writer in section 5.3. Schema bump: transaction and generation lose `latest_odin_observation`, `odin_publisher_sequence_cursor` and `odin_authority`, plus the Odin enum variants. Boot stops requiring the Odin anchor. **B5 loses its Odin arm**: the Odin part of the F16 fix is deleted from B5's brief | Idunn | A1, A2, B4′; every admitted generation re-admitted Direct | about **-1,100** production and **-1,500 to -2,000** test lines; +~100 of negative tests | **store migration, irreversible; back up `control.cc` and `history.cc` first.** Fold the v2/v3 lift retirement in if the operator has retired those backups |
| **O-R** | Odin stops originating readiness. Delete `ready`, `dependency_evidence` and `dependency_permits_ready` (O1-O3), and O7's re-verification. If nothing reads the correlation after A3, retire its publication, its signer and dedupe (O8), and `OdinTopologyIdentity` enrolment in `idunn-provision` | Odin (+ Idunn provision) | A3 | about -150 (verdict only) to about -600 (the correlation retired) | none after A3 |
| **L-R** | Retire the `odin.runtime_topology_correlation.v2` contract in cultnet-rs (`runtime_authority_contracts.rs:455-~1100`: dependency evidence, record, authenticator), under CultLib's per-runtime rules. Only cultnet-rs implements it | CultLib | O-R | about -400 | none |

**Order.**
1. X1 lands any time (in progress).
2. Then B2 as mapped, then A1 (Q-O1, after Q-O3 is ruled), and the redeploys for Q-O3. H1 is still needed; R1 is
   never written.
3. Then B4′, then A2 with M1, then A3, then O-R, then L-R.
4. Meanwhile S3 (the web drops Odin) proceeds unchanged. B5 loses its Odin arm. T1, T2, W1 and D1 are unaffected.

**Net estimate.** About -9,000 lines across Odin, Idunn and CultLib, of which about 7,000 is dead residue. Plus 1
Idunn schema bump (A3), about 200 lines added in A2 and M1, and about 100 lines of new negative tests.

**Verification gates (Soul, per cut).**
- **A1.** With Odin stopped, a continuity of a routed target that declares Odin reaches Complete. An Odin
  correlation that says not-Ready about it changes nothing. A held Odin-voucher generation mints no continuity
  (existing F1 rule).
- **A3.** `rg -i odin src/` in Idunn returns only strings: projection doc text, and the capability name inside test
  recipes. Boot succeeds with no Odin anchor file. A store with a v4 Odin-voucher generation is refused or held,
  never re-proved.
- **O-R.** No Odin code path computes a boolean named or used as Ready for another target.

**Temporary rule, Odin (deletion line: A3 plus Q-O5's supervision restart).** Until A3 and the Q-O5 restart land,
Odin keeps three permanent self-publication failures fatal, as `WriteLeaseLost` is: no runtime authority,
signer/anchor mismatch, and the fail-closed stored-presence branch (`lib.rs:945-954`). Without it `eeb6812`'s
`survive` would swallow them, Odin's self-correlation would freeze stale, and by O2/I8/I9/I2 every dependent would
be excluded and stall in Warming, deploys and continuity alike; before `eeb6812` the process exited and a restart
cleared it. It is not a second lifecycle owner: it is Odin reporting its own death through the only signal Idunn
acts on today, and `CONTINUITY_RESTART_ATTEMPTS` bounds the loop. Being added on Odin `hands/odin-session-table`.

### Owning-repo cuts

**C1 — Local route proof in the CultLib TS runtime (CultLib, after Q6).**
- Split `idunn-runtime-authority.ts` into:
  - authority loading and signing, with no Odin endpoint, no Odin dependency
    check, and no RUDP;
  - an optional Odin presence publisher for CultMesh-aware services.
- `publishRouteObservation` becomes `answerRouteObservation(request, state)`.
  It signs locally and returns the document immediately, with no network. The
  state comes from the app's current health (`warming` until first healthy)
  rather than a constant `active`.
- The lease pickup contract is unchanged.
- Tests are in the package. Verify under the node image on Yggdrasil.

**C1 status, 2026-09-29 (Self).** Landed on CultLib `idunn-ts/route-c1` (`795fd10`). Soul closed
it on conditions, and a fix batch is in Hands:
- stateful, lease-bound Rust vectors;
- the signer owns health (`reportHealth`), starting at `warming`;
- the signer records every warming it signs, so a route-proof stateful target can take its lease;
- one signer per authority, a frozen authority, and a subpath export without the Odin publisher.
**Two consequences for later cuts:**
- **C2 covers the StreamPixels service too.** `apps/service/src/app.ts:155` answers route
  challenges through `publishRouteObservation`, which C1 removed, so the gitlink bump breaks
  the service unless C2 moves it to `answerRouteObservation` and `reportHealth`.
- **B3 surfaces the typed capacity shortfall** on the route path. Today it reads as a
  generic "disagrees with current authority" (`control_plane.rs:5190`).

**S3 + C2 code status, 2026-09-29 (Self).** Landed on StreamPixels `route/s3-c2` (`8e6d891`),
after Soul and a fix batch:
- `vendor/CultLib` is at `30ee8b9`, and the web has no Odin dependency or env.
- Both apps answer through the signer, with one health owner, `driveRuntimeHealth`.
- CI now builds the vendored gitlink, not a sibling `main`.
- A production-named launcher test runs `runIdunnRuntime`. It caught a dropped `publish`
  argument that would have broken the service's write lease.
- Checks: 172 tests, typecheck, the web build and the release assembly.
**Not merged to StreamPixels `main` until B3 lands.** Idunn cannot yet admit a web with no
Odin dependency, so a deploy of `main` would fail readiness.
Follow-up: the production fixtures are generated by applying
`test-data/idunn-runtime/fixture.patch` to CultLib's `idunn_runtime_fixture.rs`. The generator
should take the target, contract and dependencies as input, so no consumer patches it.
Recorded: web `/api/healthz` is constant `{ok:true}`, so the route proves "HTTP answers", not
store health.

**C2 — StreamPixels web binding proves invariant 3 end to end (StreamPixels,
after C1, S3, B3).**
- Point the gitlink at the C1 commit.
- The Next `/cultnet/snapshot` handler answers from `answerRouteObservation`.
  No publisher is registered and no Odin env is read.
- Run the web tests and build in the node image on Yggdrasil.
- Live acceptance:
  1. Deploy the web through `idunn up` with Odin reachable, then repeat with
     Odin's UDP route broken on purpose, or with Odin stopped under the
     lifecycle brake.
  2. Both runs must admit.
  3. Both must show zero steady-state reloads over one hour.
  4. Both must show repeated successful signed challenges.
   Run 2 (Odin stopped) is blocked on B4. The failure on 2026-09-29 21:15 is the unlanded B4,
   not a C2 defect.
- Public cutover is out of scope and resumes afterwards through the ordinary
  path.

### Build and verification path

- Idunn tests use `ygg-verify.sh F:/Projects/Idunn <rev> rust '<cmd>'`. The
  command must keep its own exit status last. Heavy builds must not run on
  Starfire.
- CultLib TS runs from `packages/cultnet-ts` in a node image.
- StreamPixels runs pnpm in a node image.
- Live timeline probes (reload counts, worker counts, journal) are read-only
  commands on Yggdrasil, taken before and after each deploy.
- Each Idunn cut names its focused tests. The whole suite runs once per cut on
  Yggdrasil, not per edit.
- Every Idunn cut also runs `cargo check --locked --target x86_64-pc-windows-gnu --bin idunn-host` (W1),
  without `-D warnings`.

### Ops follow-ups, out of scope

- `worker_shutdown_timeout`: the probe found none configured.
- Retention of dead transient units and activation directories.
- Odin's UDP listener proxied through nginx: every deploy-time reload still
  moves Odin's flows (F7). Invariant 1 removes the steady-state reloads, not
  the install-time ones. Whether Odin should bind its stable endpoint directly
  is a later route-driver question.
- Heimdall's repeated source and runner failures appear in `idunn status`, but
  that work is excluded.

---

## 5. Odin in Idunn's lifecycle: authority audit (Imagination, 2026-09-30)

Operator concern, 2026-09-30: "it feels like Odin is doing lifecycle work when that's very much Idunn's wheelhouse."
The operator is right. Source-read at Idunn `94c7691` (`src/` byte-identical to `583b2a7`), Odin `5c37860`
(deployed; unmerged `hands/odin-session-table` = `eeb6812` is flagged "D"), CultLib `af7209a2`, gamecult-ops
`7d5749f`. Nothing was probed on Yggdrasil. Rulings: Q-O1 to Q-O5 in section 3; cuts in section 4.

**The live seam.** Idunn delegates readiness to Odin for every target that declares the Odin discovery dependency,
and Odin originates `ready`, including a transitive dependency-freshness verdict. Idunn reads that word at Warming,
lease grant, Ready, Routing, Commit, admitted refresh, provider selection, provider currency, and at its own boot.
Worst consequence: a continuity restart of ghostlight or streampixels-service waits in Warming with no end until
Odin speaks (I2). **The dead residue:** Odin's repo carries a complete keepalive/restart/deploy planner, about 30
`idunn.*` lifecycle schemas, a retired JS coordinator, and about 3,200 lines of deploy/restart/health actuators for
other services. None is deployed; the docs still name it as Odin's authority.

**What was missing.** Idunn's only way to hear a signed presence is `request_candidate_runtime_presence`
(`drivers.rs:5473-5486`), which needs `Expected.route`. A target with no route had no Idunn-owned eye, so Odin
(which received every presence for discovery) was drafted as the eye (`ReadinessClass`), then grew the verdict
(`ready`, Odin `lib.rs:773-777`), then the dependency policy (`:1002-1103`, `:1192`). Every Odin-correlated target
except raven-muninn is routed, so for them Idunn already owns the channel.

Classes: 1 = observation Idunn can equally get itself; 2 = Odin relaying a provider's self-report; 3 = a legitimate
Odin concern that merely looks lifecycle-ish; 4 = Odin making a lifecycle judgment it should not own. "Gate" marks a
site that decides, blocks or delays an Idunn outcome.

### 5.1 Idunn sites (`src/control_plane.rs` unless named)

| # | Site | What Odin decides | Class | Gate |
|---|---|---|---|---|
| I1 | `ReadinessClass::declared/of` `:632-670`; `CompiledDeploymentPlan::readiness_class` `deployment_plan.rs:645-661` | A discovery dependency makes Odin the readiness voucher | 1 (root conflation) | chooses every row below |
| I2 | `advance_warming` Odin branch `:5597-5700`, gate text `:5653` | Warming waits for Odin's correlation. Only OdinSelf is exempt (`:5614-5618`); **Continuity is not exempt** for anyone else | 1 | Warming, deploy **and continuity** |
| I3 | `is_semantic_warming` `:8098-8128`, `warming_disagreements_match_incumbent` `:8130-8145` | Interprets Odin's `present`, state and disagreements as Warming | 2 | Warming |
| I4 | `fresh_warming_for_lease` OdinTopology arm `:6500-6544` | A newer post-fence Odin sequence is required before the write lease is granted | 1 | lease grant |
| I5 | `advance_awaiting_ready` `:6050-6089` | Ready is Odin's `ready`, which Odin originates (O1, O2) | 1 + 4 | Ready |
| I6 | `advance_routing` `:6096-6135` | A non-Ready latest word is `Err` post-fence (`:6116-6119`); the deploy resumes forever until B5 | 1 | Routing |
| I7 | `advance_committing` `:6220-6330` | Topology admitted twice around the final proof (F16 livelock). Commit copies `current_odin_authority` onto the generation (`:6343`) | 1 | Commit |
| I8 | `refresh_admitted_topology` `:5272-5339` | Supervision stores Odin's latest word, Ready or not (`:5320-5325`); only Ready moves `ready` | 1 / 2 | provider exclusion |
| I9 | `current_ready_provider_tokens` / `rehydrate_admitted_ready` `:7398-7470`; `managed_ready_provider_refs` `deployment_plan.rs:264-316`; `ManagedReady` odin fields `:138-148` | Provider eligibility is Odin's latest word; capabilities are the provider's presence **as relayed by Odin** | 2 + 1 | plan compile (C2 run 2) |
| I10 | `validate_selected_providers_current` `:7472-7534`, called at `:5466`, `:6140`, `:6253`, `:6327` | The same, re-checked at Starting, Routing and twice at Commit (Deploy only, `:8074-8080`) | 1 | deploy abort or resume |
| I11 | Graph gate: every non-optional dependency needs a provider (`deployment_plan.rs:447-455`); the Odin dependency is `shared-infrastructure` | Odin must be admitted and current to *compile* any Verse-advertising target | 3 in intent, gate in effect | compile |
| I12 | Boot: `Engine::open` requires the bootstrap Odin anchor file (`:3900-3902`); `validate_durable_authority` re-proves every Odin receipt against the admitted Odin's key (`:4110-4225`); `boot` refuses on any failure (`:4355`) | An Odin-owned key and anchor gate **Idunn's own startup** | 4-by-proxy (doctrine violation) | Idunn boot |
| I13 | `current_odin_authority` `:4041-4054`; `AdmittedOdinAuthority` `:1905-1935` | Bootstrap key until Odin is admitted, then the admitted Odin's key. Odin's own commit copies the *current* authority (test `:14920-14936`), so Odin's topology key can never rotate through a deploy | 3 (trust anchoring), exists only for I1 | all of the above |
| I14 | `odin_publisher_sequence_cursor`, `max_odin_sequence` `:2762-2808`, `sequence_requires_admission` `:8052-8072` | Idunn's anti-replay depends on Odin's persisted watermark. If Odin loses state, every Odin-correlated target gets "replayed or reordered" as `Err` | 3, exists only for I1 | Warming, refresh |
| I15 | Topology projection: `CultCacheTopologyDriver` `drivers.rs:4238-4250`, `publish_*`, `demote_to_expected_only` | Idunn publishes Expected, activation and lease; Odin and the workloads read them | 3 (correct direction) | none. Its doc comment ("only Odin may correlate ... into Present/Ready", `drivers.rs:4239-4241`) is the misplaced doctrine in prose |
| I16 | `AdoptionSource::OdinTopology` `:786-795` | No production writer (only tests, `:11502`, `:11692`). B5's adoption slot | dead variant | none |
| I17 | `validate_live_providers_for_deploy` `:8074-8080` | Continuity skips provider currency. The only continuity exemption; I2 and I4-I7 have none | n/a | n/a |

### 5.2 Odin sites (`crates/odin-daemon/src/lib.rs` at `5c37860` unless named)

| # | Site | Mechanism | Class |
|---|---|---|---|
| O1 | `ready` `:773-777` | `presence.state == "active"`, relayed, and no disagreements, and every dependency permits Ready. Odin originates the verdict Idunn acts on (I5-I10) | 2 + **4** |
| O2 | `dependency_evidence` `:1002-1103` (`ready` `:1098`, D `:1108`) | Authenticates the *provider's* stored correlation against **now**, 30 s window, plus capacity. A provider whose correlation is not re-stamped for 30 s flips every dependent to not-Ready: a transitive freshness verdict | **4** |
| O3 | `dependency_permits_ready` `:1192` (D `:1202`) | Kind policy (`optional` and `external-operator-binding` never block) lives in Odin. It already bent recipes: Ghostlight `c5bf090` dropped its Heimdall dependency to dodge it (`gamecult-ops/runbooks/ghostlight-dungeon-yggdrasil.md:49-55`) | **4** |
| O4 | `admit_presence` `:654-676`, `select_presence` `:886-897` | A presence is admitted only for an incarnation Idunn projects. Odin consumes Idunn's truth | 3, keep |
| O5 | `same_publisher` `:929-956` (D `:961`/`:964`) | Anti-replay within one activation. Surfaced as a lifecycle outage only because of O1 -> I8/I9 | 3, keep |
| O6 | Stored presence re-authenticated at its receipt time `:960-988`; 30 s max age and 5 s skew `:62-71` | Odin's admission window | 3 |
| O7 | `correlate_write_lease` `:1126-1157`, `classify_runtime_authority` `:1196-1324`, `classify_current_lease` `:1326-1356` | Odin re-verifies Idunn's own artifacts. Redundant with Idunn | 1 |
| O8 | Correlation dedupe and watermark `:541-575`, `:776-792` | Publisher discipline toward I14. Dies with the correlation | 3 |
| O9 | `main.rs` `stored_snapshot` `:466-490`; `raw_snapshot` `:290-310` (D `main.rs:298-316`) | Filtered relay of provider bytes. Odin answers challenges to itself with `signed_presence_document`, the same signer and authority its self-publication uses | 2/3, keep |
| O10 | `main.rs` activation waits for Idunn's lease, 300 s (`:67`, `:580-589`) | Odin as a stateful Idunn target | correct direction |
| O11 | `survive` (D `main.rs:671`, `eeb6812`) | Every error except `WriteLeaseLost` is transient, including permanent self-publication failures | see the temporary rule in section 4 |

**Dead residue** (class 4 in content; nothing deployed; cut by X1):

| # | Residue | Status |
|---|---|---|
| O12 | `crates/odin-core/src/idunn.rs` (672 lines), including `plan_keepalive` `:14-173`; `documents.rs:28-59`, about 30 `idunn.*` lifecycle schemas | Uncalled. `odin-daemon` does not depend on `odin-core`. Muninn and Sleipnir (Muninn workspace) still emit `IdunnDaemonHealthRecord` via `odin-core` pinned at `3e96c6c`; no live Idunn reads it |
| O13 | JS coordinator `src/` (2,814 lines) plus tests (140 lines): `probes.cjs` docker and adb probes, `state.cjs` mints service states, `provider-ingress.cjs` 120 s TTL and hard-coded `active` | Not deployed (`state/map.yaml:30`). Still reachable by `package.json start`, `scripts/start-odin.ps1`, `scripts/restart-odin.ps1`, and `gamecult-ops/compose/odin.yggdrasil.yaml` |
| O14 | `scripts/` (3,208 lines): deploy, restart and health actuators for heimdall, streampixels, repixelizer, stonks, vili, weksa, nightwing and voidbot | Deploy scripts dead but armed (`IDUNN_ACTUATOR=1`, referenced by `gamecult-ops/scripts/idunn/idunn-deployment-targets.ps1:178-216`). Restart scripts target the retired Windows Idunn. Health scripts are stubs or orphans |
| O15 | `docs/architecture.md` `:49-62`, `:97-109`, `:218-228`; `README.md` `:71-105` | Claim process lifecycle, health and "Rust lifecycle logic" for Odin, and name `crates/idunn-daemon`, which does not exist |

**Recipes and ops.**
- G1: Muninn `2260853` `raven-muninn.toml` declares the Odin dependency *to obtain a readiness class* (its own
  comment says so). Unrouted. Held `Undeclared` on the live store, so it has no continuity restarts today.
- G2: Ghostlight `c5bf090` and the ops runbook `:49-55`: a real dependency was removed to escape O2 and O3. Odin's
  gate is shaping the dependency graph.
- G3: `gamecult-ops/scripts/idunn/idunn-deployment-targets.ps1:94` labels "odin-correlated-runtime-presence";
  display-only.

### 5.3 Target authority map

**The decision.** Is this admitted incarnation, or this candidate, Warming, Ready, or current enough to serve a
dependent? And when does its absence restart it?

- **Owner.** Idunn's phase engine and supervision, acting on **Idunn's own challenge** of the incarnation's
  challenge endpoint (the candidate endpoint pre-promotion, the stable endpoint after), answered with a presence
  signed under the Idunn-issued launch authority; and on Idunn's own workload observation (systemd or host
  actuator). One proof path serves every target; there is no class.
- **Inputs.** Expected, activation and lease (Idunn's own); the challenged signed presence (the provider's own
  words, authenticated by Idunn end to end); workload observation; brakes; B2's route state, degraded or not;
  `TargetSupervision` meters.
- **Outputs.** Transaction evidence (`Direct` warming, `Direct` Ready); admitted generations; provider currency
  (latest proof not degraded and within 2 x max age, B4′'s rule); capabilities from the challenged presence; the
  topology projection (I15), unchanged, which Odin and the workloads read.
- **Derived or demoted.**
  - `ReadinessClass` is **dead**. What survives is "does the binding give Idunn a challenge endpoint?", and the
    refusal when it does not.
  - Odin's correlation, if Odin keeps publishing anything, is **display and discovery only**. Idunn never reads it.
  - `ODIN_RENDEZVOUS_CAPABILITY` is an ordinary capability string; Idunn has no reason to name it.
  - "Is Odin" is a question Idunn never asks.
- **Forbidden writers.** None of these decides any lifecycle outcome:
  - `CultCacheTopologyDriver::receive` and the `odin_correlation_store` option;
  - `admit_latest_topology`, `refresh_admitted_topology`, `rehydrate_ready_token`, `rehydrate_warming_token`'s Odin
    arm, `rehydrate_admitted_ready`'s Odin form;
  - `is_semantic_warming` and `is_semantic_ready`;
  - `current_odin_authority`, the bootstrap Odin anchor, `AdmittedOdinAuthority`;
  - `odin_publisher_sequence_cursor`, `max_odin_sequence`, `sequence_requires_admission`;
  - `WarmingEvidence::{OdinTopology, FirstOdinDirect}`, `ReadinessEvidence::OdinCorrelated`,
    `AdoptionSource::OdinTopology`;
  - boot's re-proof of Odin receipts;
  - in Odin: `ready`, `dependency_evidence`, `dependency_permits_ready`.
- **Shared paths.** The same `challenge_*` primitive and authentication serve deploy and continuity Warming, the
  lease-grant freshness check, Ready, Routing, Commit, supervision and provider currency, and boot re-proof of
  Idunn's own direct evidence only. Deploy and continuity differ only in the brake they ask and in the
  provider-currency skip (I17).
- **Deletion line.** Delete every forbidden writer above, plus the `latest_odin_observation` and `odin_authority`
  fields and the `--odin-trust-anchor` and `--odin-correlation-store` options, before any new currency or health
  behaviour is added on top.

**Odin, in the target.** Odin consumes Idunn's projection (O4) to know which incarnations exist and which activation
to trust; admits presences for discovery with anti-replay (O5, O6); serves the catalog, routes and Eve surfaces. It
may *display* whether a provider looks alive; nothing downstream gates on that display. Odin's own lifecycle is
Idunn's (O10). Schema ownership for `idunn.*` documents leaves Odin.

### 5.4 Odin's own health (Soul input on `eeb6812`)

`survive` swallows every error except `WriteLeaseLost`, including three permanent self-publication failures. Idunn
restarts only on workload death (`control_plane.rs:4990-5050`), and Q5 (a) marked a failed route proof degraded
without restart. What Idunn should observe is its own stable-route challenge of the admitted Odin
(`supervise_admitted_route`, no new signal): Odin answers through `signed_presence_document`, so "no runtime
authority" and "signer/anchor mismatch" make the challenge fail visibly. The fail-closed store branch breaks only
Odin's catalog entry for itself: a discovery defect Odin should show on its own Eve surface, not exit over. Q-O5
rules what Idunn does with an unauthenticatable answer; the interim is the temporary rule in section 4.

### 5.5 Open items for Soul

- **Continuity-restart stall probe** (not yet run). `build_routed_world` with the recipe's `[[dependencies]]` kept
  (OdinCorrelated), `Odin::Unreachable` or an absent store, seeded at Warming as Continuity. Assert it stays in
  Warming and `stub.candidate_hits == 0` even though the candidate would answer. Confirms I2 before A1 removes it.
- **Journal confirmation** of the exclusion seen since 19:45 CEST. Ghostlight and raven-muninn have been excluded
  ("latest ... not Ready") on every compile with Odin up. Unconfirmed hypothesis: by O2, their Odin-computed `ready`
  is false whenever Odin's correlation about itself is more than 30 s old, and at `5c37860` Odin's self-presence
  crosses its own RUDP session table (the fault `a9ed585`, unmerged, fixes); so O2 plus the self-presence
  session-table bug produce the exclusion. Confirm from the `dependencies[].ready` of their latest correlation via
  Idunn status or journal text, not a store read.
- The Idunn status and Eve rendering of Odin fields was not audited. It is display, and dies with A3.
