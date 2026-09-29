# Route continuity and admission: cut map

Status: cut map, Imagination pass 0b (Opus), 2026-09-29. Nothing has landed.
Ends are owned by route-continuity-target.md.
**2026-09-29, Self (Eureka session "Codebase audit"; the campaign was handed
over from the StreamPixels deployment session with the operator's
confirmation).** The operator ruled all six questions in section 3, one at a
time: **Q1 (b), Q2 (b), Q3 (b), Q4 (b), Q5 (a), and Q6 (a) with a Soul gate on
the weekend branch before the merge.** Next: S1 in Hands, and the Q6 Soul gate
on CultLib `codex/fix-node24-ajv-esm`. Behaviour cuts re-take their
`file:line` anchors in their own briefs. The operator has asked for no live
mitigation on Yggdrasil yet.

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

**RULED (b) by the operator, 2026-09-29.**

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

**RULED (a) by the operator, 2026-09-29.**

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
1. The legacy-transaction lift maps a pre-B1 continuity abort that issued an activation to
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

**B2 — Bounded, backed-off route and unit repair (Idunn).**

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

- Challenges back off exponentially per target on consecutive failures, with a
  floor at max age and a cap.
- Route actuations (write, reload, ufw) go through a per-target rolling
  ceiling kept in route supervision state, so the ceiling survives restarts.
- Continuity restarts go through a per-target backoff and ceiling that is not
  reset by a new generation (F9).
- Proof failure changes observation state only (Q5a).
- Verification: a timeline probe on Yggdrasil counts reloads per hour per
  target in both steady and failing states. It must show zero reloads when
  steady and no more than the ceiling when failing.

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
(Idunn).**
- Per Q2(b), `validate_selected_providers_current` and plan compile read the
  provider's current route observation within max age, and read capabilities
  from its signed presence.
- Unrouted providers use their Q1 class evidence, authenticated now.
- Revert the admission-time authentication.
- `ManagedReady` carries a class-tagged evidence digest.

**B5 — Every post-fencing phase ends (Idunn).**
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

### Ops follow-ups, out of scope

- `worker_shutdown_timeout`: the probe found none configured.
- Retention of dead transient units and activation directories.
- Odin's UDP listener proxied through nginx: every deploy-time reload still
  moves Odin's flows (F7). Invariant 1 removes the steady-state reloads, not
  the install-time ones. Whether Odin should bind its stable endpoint directly
  is a later route-driver question.
- Heimdall's repeated source and runner failures appear in `idunn status`, but
  that work is excluded.
