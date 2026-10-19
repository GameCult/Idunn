# Reincarnation after reboot: map

Status: map, Imagination, 2026-10-18 (campaign `idunn-reincarnation`, session
`idunn-reincarnation-2026-10-18`). The ends are in
[`reincarnation-target.md`](reincarnation-target.md). Questions, rulings, the
cut spec and follow-ups live in Eureka's mind; this page holds what has
no kind: body facts, the model page, the authority map's rationale and the
cut sequence. Line numbers are at `74a7008`; `src/` is byte-identical to
`c2477d4`, so the earlier anchors still hold.

Cut 2 pass, Imagination, 2026-10-19 (same campaign and session): body facts
B13-B18, the cut-2 rows of the model page and the cut-2 parts of sections 3-5
are anchored at `9a0a6a2`, whose `src/` is byte-identical to `ffb4293` (cut 1's
head). The B1-B12 anchors above stay at `74a7008`.

Read the in-force set with the rulings-in-force and open-follow-ups recipes
for root `idunn-reincarnation`; the cut in flight is the specs-with-no-report
recipe. The first spec (`cut-c1-durable-stop.r1`) was withdrawn when the
operator ruled that continuity never stops; its replacement is cut
`c1-never-stop`. Cited from `idunn-watchdog`: `exhaustion-close-rule-op` and
`paging-conditions`.

## 1. Body facts

Evidence: the `idunn-yggdrasil` journal from 11:40 UTC (94,670 lines), the
per-unit journals of the streampixels-service candidates and of Odin
(`idunn-odin-7cfe5cd1…`), `idunn status` and `idunn status --command <id>`
on Yggdrasil, `journalctl -u nginx`, and the source at `c2477d4`. All times
UTC, 2026-10-18.

**B1. The reboot did not stop continuity from re-materializing anything.**
Every streampixels-service continuity attempt minted a fresh transient unit
from the installed release `sha256-df663f0a…`: 54 continuity commands from
`continuity-85887ef2` (11:46) to `continuity-52bbbd66` (19:57), most listed by `idunn status` as
`continuity-… failed systemd unit idunn-streampixels-service-<hash>.service
is not running`. A continuity transaction is a full `DeploymentTransaction`
built by `from_continuity` (`control_plane.rs:1678`) from the admitted plan,
sealed release, installed release and Expected. It runs every phase:
Starting, Warming, Fencing, Leasing, AwaitingReady, Routing, Committing. The
absent admitted unit `461c6443…` was never the problem. Each new candidate
was.

**B2. Every candidate died of a dependency, not of itself.** The
streampixels-service process exits about 30 s after start when Odin does not
admit its initial presence. Unit `2bc02905…` at 11:47:21 logged "StreamPixels
service failed to enter its Idunn incarnation: Error: Odin did not admit the
initial Idunn runtime presence ... Timed out waiting for CultNet RUDP response
after 5000 ms." The same is logged by `42ecf363…` at 13:47:13, `03acf44d…`
at 19:47:14, `45276008…` at 19:49:54 and `599b705b…` at 19:51:18. Odin's
RUDP endpoint is `rudp://10.77.0.1:17871`, which nginx proxies. nginx failed
at 11:46:26 ("bind() to 10.77.0.1:17871 failed (99: Cannot assign requested
address)") and started at 19:46:24. Between 11:46 and 19:44 Odin's journal
shows no presence message from streampixels-service at all: none arrived.
After nginx returned, the first one arrived at 19:49:25 and was rejected:
"runtime presence has no exact current activation and provider anchor". The
attempts minted at about 19:53 and 19:55 got past Fencing and then failed
"the Leasing phase passed its deadline" (FailedAfterFencing, RestoreIncumbent).
Other early failures: "observed write lease is not the current Idunn grant"
(11:46:47, the first attempt after boot), and three "reading process stat
/proc/N/stat: No such file or directory" races.

**B3. Every attempt was charged at mint.** `record_restart(now)` runs inside
the mint CAS (`control_plane.rs:6156-6157`). Nothing reads a continuity's
outcome back into the log. The only exemption anywhere is `HostUnobservable`
(`:5944`), and it applies to an observation, not to a candidate. So the 6-per-
hour window (`CONTINUITY_RESTART_ATTEMPTS` = 6, window 3,600,000 ms, `:71-72`)
filled at 11:50:52 and refilled every hour as entries slid out. The bursts are
in the journal at :47, :48, :49, :51, :52 and :53 of every hour, 39 times
"Idunn stopped restarting admitted streampixels-service". This answers Q2:
the nginx outage counted against the body's budget, and the place that decides
it is the unconditional charge at `:6156`.

**B4. The deployment was killed by a window slide, not by a live
continuity.** `up-3ad6b05b` (release `sha256-67e227b2…`, a new release) was
recorded at 20:24:51. It was still before Fencing at 20:46:41, when it was
completed `FailedBeforeFencing` with "admitted incumbent failed before
candidate fencing; deployment yielded to continuity: systemd unit
idunn-streampixels-service-461c6443….service is absent". Two seconds later
continuity minted `tx-0804185a` with the OLD release `df663f0a…`, and it was
admitted. The restart that left the window was minted at about 19:46:40 (its
unit `03acf44d…` started at 19:46:42), exactly one window before the abort.
The mechanism, from the source:

- `:5805-5821`: a live transaction for the target is the `blocker`. From
  Fencing on (or Starting for stop-then-start) supervision skips the target
  entirely.
- `:6033-6061`: with a blocker present, continuity never restarts and never
  reports. It may abort the blocker, and the predicate for that is
  `Deploy && !supervision.restarts_exhausted(now) && phase < Fencing && no
  abort && not held` (`:6045-6050`).
- `:6062-6084`: only with no blocker does an exhausted continuity open the
  incident and say "The target is free for a deployment to replace it".

So from 20:24:51 the deployment owned the target (`owns_target_authority`,
phase != Complete, `:1772`). Continuity deferred to it through the blocker
`continue` at `:6060`. The deployment deferred to continuity whenever the
window had a free slot, through `:6046`. "Stopped" existed only as a durable
incident (incidents.cc, a projection) and a report. The ownership decision
read the window. When the window gained a slot, continuity reclaimed the
target with no record that it had resumed. This answers Q3: the two owners are
the live `DeploymentTransaction` (it owns while not Complete) and
continuity-as-window (it owns whenever the window is not full). Continuity
defers to the deployment while any transaction is live. The deployment defers
to continuity while the window has room. The "free for a deployment" message
names the first ownership, and the code acts on the second.

**B5. Fencing an absent incumbent already works.** `advance_fencing`
(`:6869-6952`) stops the exact incumbent and revokes its lease. The systemd
driver's `stop` returns `Ok(())` for an absent unit (`drivers.rs:4003-4007`).
A deployment that is not aborted therefore replaces an absent incumbent with
nothing to fence, which is target invariant 3's last clause, already true.
`exact_incumbent` (`:7556-7569`) fails "target incumbent changed after
transaction creation" if a continuity commit replaced the generation during
the deployment. That is why a phase-split handover needs a rebase
(`handover-pre-fencing`).

**B6. The two rejection streams are consequences of a dead admitted
generation, not separate faults.** "preserved admitted streampixels-service
after rejecting topology observation: topology correlation does not bind the
current activation" was logged 46,271 times, from 11:46:49 to exactly
20:46:48, about 5,300 an hour. Ghostlight's 42,482 identical lines ran to
19:56:29. Since 20:47, raven-muninn, newly exhausted, shows the same line
621 times. `refresh_admitted_topology` (`:6395-6457`) authenticates Odin's
correlation against the admitted generation's activation. When that
incarnation is dead and Odin's latest record names another one (a
continuity candidate's, or a stale one), authentication fails with an `Err`
rather than `Ok(false)`, and `:5834-5840` prints it on every pass. The pass
order is what does it: route supervision (`:5822`) and topology refresh
(`:5834`) run before the workload is observed (`:5891`). So a dead
generation is "supervised" every tick. "rejected admitted
streampixels-service route continuity: admitted process write lease is no
longer exact" (3,615, from 20:02:23 to 20:46:48) followed the two
post-fencing aborts at 19:54 and 19:57. Fencing revoked the dead incumbent's
lease, "restore incumbent" cannot re-grant a lease to a process that does not
exist, and `supervise_admitted_route` (`:6250-6255`) then refuses the
admitted generation on every challenge. Both streams stopped the moment
`tx-0804185a` was admitted. This answers Q4: both streams follow from one
model gap, that a dead admitted generation is treated as a live incumbent.
That gap is cut 4. They are not the deadlock.

**B7. Ghostlight is a separate fault.** Its continuity `tx-039b91e9` is
wedged in a post-fencing abort. It was "lease-not-adopted" at its AwaitingReady
deadline, and the abort withdraws the candidate's lease (`:8895-8928`) before
it stops the candidate (`:8929-8946`). The candidate unit `6c51e35e…` has
been running since 19:55:15 and holds the lease's shared lock, so every
attempt fails "process write lease is held by a shared-lock consumer"
(`drivers.rs:4179/4224`). The transaction owns the target, so continuity is
locked out and the route degraded (53 consecutive failures). This is follow-up
`ghostlight-abort-lease-before-stop` (route-continuity invariant 5), not the
stopped-owner deadlock.

**B8. Unexplained, recorded.** Idunn's record of Odin's stable route reads
"degraded-since 11:46:31, consecutive-failures 1, next-challenge-at 11:47:01"
about nine hours later. It has not been re-challenged since. This is follow-up
`odin-route-record-stale`. It blocks cut 3. Under `budget-never-stop`, a
failure attributed to a declared dependency must not grow the backoff, and the
only dependency-health record Idunn holds for Odin's stable route is this
one.

**B9. Luna's weekend commits on this path (Q5).** None of them caused the
deadlock. The yield predicate dates from the route-continuity and watchdog
campaigns, not from the weekend. Where they touch:

| Commit | On the path? | Status |
|---|---|---|
| `9f00e7a` replay provider receipts at admission time | Yes: the deployment's Sealing compile (`advance_sealing` → `current_ready_provider_tokens`) logged "excluded non-current provider ghostlight / raven-muninn: admitted provider's latest receipt is not Ready at its admission time" at 20:24:53 (`:8588`, `:8609`) | Still live: route-continuity B4 has not landed. It did not block this deployment |
| `29946bb` wait for the stable route listener | Yes: the route proofs during the outage were "Connection refused" on 127.0.0.1:8831-8833 | Kept by route-continuity (honest scope) |
| `8ae00a1` cancel live stateless candidates | No: no cancel ran | Still present (`:2888`); route-continuity S2 owns its deletion |
| `ae8ac45`, `8651906`, `e976d32`, `5e94eb8` identity and launch evidence | Only in `observe` and `stop`; nothing here drifted | Kept |
| `a148802` | No | Reverted (`87fa57d`) |

The target forbids building on them. Cut 1 touches none of their lines.

**B10. The stop was the only door a deployment had into a crash loop.**
Source read at `74a7008`. A queued `idunn up` becomes a transaction only in
`freeze_command`, which refuses while any transaction for any of its targets
`blocks_new_target_mutation` (`:5741-5747`); the command just stays queued.
The scheduler tick runs `retire_one_terminal_transaction` (returns early when
it retires), then `resume_one_transaction`, then
`supervise_one_admitted_generation`, then `freeze_one_queued_command`
(`:5414-5423`). Today's doubling wait is anchored at the previous attempt's
**mint** (`next_restart_at`, `:1163-1170`). So once an attempt lasts longer than
its wait, the tick after its transaction is retired mints the next attempt
before the freeze runs, and the queued deployment never finds the target
free. At `c2477d4` that ended when the window filled and continuity stopped.
Delete the stop without opening another door and a crash-looping release can
never be replaced. The door the cut uses is kubelet's own anchor
(`docs/research/restart-budget-prior-art.md` §1.1, the exited container's
`FinishedAt`): the wait runs from the first supervision pass that finds the
target dead **and free**. That leaves at least one wait (5 s or more) with no
live transaction, and the freeze runs every tick. This is reasoning over the
source, not a probe. The cut pins it with a tick-level test.

**B11. The yield cannot outlive the stop.** With the window term gone from
`:6046`, the yield aborts every pre-fencing Deploy whenever the incumbent is
dead (`:6045-6059`). For a crash-looping release the incumbent is always
dead, so the deployment that would fix it is aborted at the first
supervision pass after it is frozen. That is the 2026-10-18 failure: on that
day the operator's `idunn up` existed to replace a release that would not
start. Ruling `handover-phase-split` already says nothing yields before
Fencing. So cut 1 deletes the whole yield. Continuity keeps deferring to any
live transaction for its target (`:6033`, the `continue`). Cut 2 keeps
the incumbent alive alongside the build. Between them, a dead incumbent stays
down while a pre-fencing deployment over it builds. That is bounded by the
deployment's own deadlines, and the deployment then fences it as nothing
(B5).

**B12. The doubling function already exists once.** `backoff_wait`
(`:4000-4007`) doubles from a base up to `RESUME_BACKOFF_CEILING_MILLIS`, and
has one caller (`back_off`, `:5621`). Continuity's wait is a second spelling
of the same rule (`5_000 << (live.len() - 1)`, `:1169`). The cut gives
`backoff_wait` a ceiling parameter and deletes the second spelling.
`#[cultcache(key = N, default)]` is how an absent key decodes as its default
(precedent `DeploymentTransaction` key 34, `:1552-1557`). New
`TargetSupervision` keys use it, because every record on Yggdrasil predates
them.

**B13. Two live transactions for one target make the control store
unreadable.** Source read at `9a0a6a2`. `ControlSnapshot::validate` refuses
"multiple transactions claim current-incarnation authority for one target"
over `owns_target_authority` (`:3245-3254`), and that predicate is just
`phase != Complete` (`:1896-1898`). Every reader of the store goes through it:
the scheduler tick, `idunn status`, cancel, expire. So if supervision minted a
continuity beside a pre-fencing deployment today, the next read would fail
the whole engine. Coexistence needs the identity rule changed first, and that
is the cut's first commit.

**B14. One predicate answers four questions, and the phase rule lives in
one caller.** `blocks_new_target_mutation` (`:1903-1909`, live or post-commit
cleanup pending) is read by the freeze busy check (`:5934-5942`), the
supervision blocker (`:5998-6001`, a single `.find`), candidate-port
reservation (`:7997`) and status (`:4613`). `owns_target_authority` is read by
validation (`:3249`) and by sibling ordering inside one command (`:3384`).
Neither looks at the phase. The only phase-aware ownership rule is inline in
supervision (`:6002-6012`, owns from Fencing, or from Starting for
stop-then-start). The freeze refusal is why a `profile:` command needs every
target free in one tick (follow-up `profile-deploy-joint-gap`).

**B15. What a pre-fencing deployment reads about its incumbent.** Warming's
semantic warming check uses the incumbent's lease digest
(`:6904-6905`, `is_semantic_warming` `:9530-9560`: Odin reports the candidate's
one disagreement as the lease the incumbent holds). Route preflight uses the
incumbent's route observation (`:7026-7047`). Isolation is proven against the
incumbent's workload (`:7050-7076`). Each goes through `exact_incumbent`
(`:7768-7781`), which bails "target incumbent changed after transaction
creation" once a continuity commit replaced the generation. Sealing, and
Starting for candidate-then-promote, read no incumbent. Stop-then-start's
Starting stops it (`:6857-6863`), which is already the owner's act. The
Sealing-to-Starting write is a `replace_transaction` at `:6806-6808`, not
`transition` (`:8914-8927`), so the two edges at which a transaction can take
a target are written by two different functions today.

**B16. The commit already guards the incumbent; nothing guards the
handover.** `advance_committing` expects the admitted envelope it read
(`:7701-7702`, CAS `:7743-7764`), so a deployment can never commit over a
generation that moved under it. Entering Fencing is an unguarded
`replace_transaction` of the transaction alone. Continuity's mint CAS
(`:6371-6400`) guards its command, transaction and supervision record, and no
other transaction. The store accepts read-only expectations: an expected
entry with no replacement only guards (CultLib
`packages/cultcache-rs/src/lib.rs:1327-1368`). One engine holds the store
(`ProcessLock::acquire`, `:5514`), but the CLI writes transactions too:
`cancel` sets a pre-fencing abort on a live deployment (`:3014-3083`). So a
handover decided by one CAS over the target's records is exact, and a tick
order is not.

**B17. What coexisting candidates share, and what they do not.** Continuity
reuses the admitted plan, so its candidate port is the admitted generation's,
already reserved (`:7988-7992`); a deployment picks another. Topology is keyed
per incarnation (`:8495-8498`): a continuity publishes under the admitted key,
a deployment under its own. No candidate holds a write lease before its own
Leasing, which follows Fencing, so two pre-fencing candidates cannot both
write. Route actuation by a continuity is always `Survival`, never refused
(`:4870-4876`); a deployment's `Forward` cannot starve it. The Odin replay
floor `max_odin_sequence` (`:3399-3435`) spans every transaction of the target
and the admitted cursor; incumbent refresh and candidate warming already share
it during every deployment today, so the replay check (`:9498-9501`) gains no
new exposure. Stop-then-start is declared only by route-less host-actuator
bindings (`deployment.rs:756-764`, `:1245-1249`), and no control-plane test
builds one (grep `StopThenStart` in `src/control_plane.rs`: the definition
site only). It is live on Yggdrasil: raven-muninn declares it
(`git grep stop-then-start` in `gamecult-ops`:
`idunn/yggdrasil/bindings/raven-muninn.toml.in`). So the Starting edge of the
handover carries production traffic, and its engine-level proof is owed
before the closing install (follow-up `stop-first-handover-fixture`).

**B18. A pre-fencing error is an abort.** `resume_candidate` turns any `Err`
from a pre-fencing step into `begin_pre_fencing_abort` unless it is
`LockContended` (`:5768-5783`). So anything the cut makes a deployment wait on
must return `Ok` (a gate wait, `record_gate_wait` `:8170-8182`), or the wait
becomes the yield that ruling `handover-phase-split` forbids. The same path is
how a superseded continuity ends itself, which is wanted.

## 2. Model page

One row per persistent kind the campaign touches.

| Kind | What names it | Lifecycle | Who decides |
|---|---|---|---|
| Paging log: `TargetSupervision.continuity_restarts` (control.cc, `idunn.target_supervision.v1`, key 3) | key = target | The newest 6 charged attempt times, ascending, read through a sliding 1 h window. Outlives every generation. After cut 1 it is the paging threshold only (`restarts_page`): it opens and holds the ContinuityExhausted incident and is shown in status. It gates no mint, wait or yield | Continuity supervision, pushed in the mint CAS |
| Attempt streak (new, cut 1: key 7, `default`) | target | Charged attempts since the admitted body last ran a sustained run. +1 in each charged mint CAS. Reset to 0 when the dead generation was admitted after the last charged attempt and ran ≥ 600 s before it was found dead. Saturating, and the wait is capped, so it can be unbounded | Continuity supervision only |
| Backoff anchor (new, cut 1: key 8, `default`) | target | Written on the first pass that finds the admitted generation dead and the target free after an attempt. The next attempt is due at anchor + `backoff_wait(5 s, streak - 1, 300 s)`. Cleared by every mint. Stale, and ignored, once older than the admitted generation's `admitted_at` | Continuity supervision only |
| Re-materialization bound (new, cut 1: key 9, `default`) | target | Set by the one attempt that is not a backoff step, when the driver reports a reboot loss. Spent while it is not older than the admitted generation's `admitted_at` | Continuity supervision, from the workload driver's evidence |
| Projection deferral (keys 4-5) | target | Unchanged: a failed demotion defers the next mint 30 s. It spaces a re-materialization too | Continuity supervision |
| Admitted generation (`idunn.admitted_generation`) | key = target, `generation_id` = `generation-<tx>` | Replaced atomically by every committed deployment or continuity. A dead incarnation keeps its record until replaced | The committing transaction |
| Incident `ContinuityExhausted` (incidents.cc) | condition + subject (target) | Opened at 6 charged attempts in the hour while the target is dead and free. Closed when the target runs and the window no longer holds 6 (`exhaustion-close-rule-op`). Retired after retention. The name is the wire name and stays, though nothing is exhausted any more | Idunn's incident writer, a notification projection: never read by ownership |
| Transaction and pre-fencing abort (`idunn.deployment_transaction.v4`, history.cc once terminal) | `tx-<uuid>` and its `command_id` (`up-…` or `continuity-…`). Keyed by transaction, so a continuity and a deployment for one target are two records with no shared key | Sealing … Complete. A pre-fencing abort completes `FailedBeforeFencing`, a post-fencing abort `FailedAfterFencing` with a named recovery. Retired to history. After cut 2, per target at any instant: at most one live Deploy, at most one live Continuity, at most one owner (validated by `ControlSnapshot::validate`, B13) | The scheduler advances it. After cut 1, continuity supervision never aborts a transaction; the transaction's own steps do. After cut 2, a continuity whose target was taken, or whose source generation was replaced, aborts itself (superseded) |
| Target ownership (cut 2: derived, no record) | target | A transaction owns its target from `takes_target_at()` (Fencing, or Starting for stop-then-start) until Complete; a Complete transaction with pending post-commit cleanup drains it. Entered only through the handover CAS (`Engine::take_target`) | Derived from the phase. The handover CAS is the one writer of a phase entry into `takes_target_at()` |
| `incumbent_generation_id` on a transaction (key 9, unchanged format) | the generation's id | Deploy: set at freeze; before the handover it names the generation its incumbent-relative evidence was proven against and is rebased when the admitted generation moves (clearing that evidence); pinned by the handover CAS, exact from then on. Continuity: its source generation, never rebased | Deploy: its own pre-handover rebase step and the handover CAS. Continuity: the mint |
| Unit materialization (systemd transient unit `idunn-<target>-<hash>`) | the activation's hash | Created by Starting. Lost on a host reboot (`/run/systemd/transient`). A failed unit stays loaded; a cleanly stopped one is collected | The workload driver actuates. Whether a loss was a reboot is the driver's evidence (`WorkloadPort::lost_to_host_reboot`, cut 1) |

No cell is open. Both forks are ruled (`budget-never-stop`,
`handover-phase-split`). The numbers that `budget-never-stop` left open are
the defaulted ruling `continuity-backoff-defaults`. Cut 2 opens one question,
`fencing-over-owning-continuity` (does a deployment ready to fence wait for a
continuity that already owns the target, or preempt it); the spec is written
for the recommended answer, wait.

## 3. Authority map: continuity and deployment

The authority map for cut 1 is in its spec. This section is the
campaign-level map, and cut 1 is one step of it.

- **Owner.** Exactly one authority actuates a target at each instant. A live
  `DeploymentTransaction` owns the target from Fencing on, and a stop-first
  rollout owns it from Starting. Before Fencing (`handover-phase-split`), a
  deployment owns only its private candidate, and continuity owns the
  incumbent slot. Continuity owns the admitted generation's survival and
  never stops. When it next acts is a function of its own record: the
  streak, the backoff anchor and the re-materialization bound.
- **Inputs.** For continuity: the admitted generation (id, `admitted_at`,
  workload, plan), the workload observation and the driver's reboot
  evidence, its own record, and whether a live transaction blocks the
  target. After cut 3 it also reads Idunn's dependency-health observation.
  For a deployment: its own plan, its brake, and at Fencing whatever
  generation is admitted at that moment (cut 2).
- **Outputs.** Continuity: mints, either charged or the one re-materialization;
  backoff anchors; deferrals; and, as notifications only, the
  ContinuityExhausted incident and its once-report. Deployment: a new admitted
  generation, or an abort that names its recovery.
- **Derived state.** `restarts_page(now)` (6 charged attempts in the hour) is
  derived from the paging log and is notification-only: incident open and
  close, and status. The next attempt's due time is derived from the
  anchor and the streak. The incident and `idunn status` are projections.
- **Forbidden writers.** The paging window gates no mint, no wait and no
  yield. Continuity supervision never aborts a deployment:
  `begin_pre_fencing_abort` has no caller in it. No deployment path writes
  continuity's fields. The incident store decides nothing. No stopped state
  exists: no field, and no branch that ends a pass because a count is
  reached.
- **Shared paths.** A crash restart, a restart after a sustained run, and a
  reboot re-materialization all mint through the one CAS (`:6158-6183`). The
  dead-and-free pass is the only writer of the backoff anchor. Every
  deployment, operator or scheduled, enters through `freeze_command` and
  advances through `advance_transaction`.
- **Deletion line.** Cut 1 deletes the yield (`:6034-6059`), the
  exhaustion `continue` and its "free for a deployment" text (`:6062-6085`),
  and the mint-anchored wait (`:1160-1184`). Cut 2 (anchors at `9a0a6a2`)
  deletes `owns_target_authority` and `blocks_new_target_mutation`
  (`:1894-1909`), the inline phase rule and the single-blocker `.find` in
  supervision (`:5998-6012`), the blocker `continue` (`:6229-6233`), the
  freeze busy check (`:5934-5942`), the one-live-transaction-per-target
  validation (`:3245-3254`), and the "target incumbent changed after
  transaction creation" text (`:7779`).

### Cut 2: the two slots and the handover

Each target has one **incumbent slot**: its `AdmittedGeneration`, the body
running it, its write lease and its stable-route membership. Each live
transaction has its own **candidate slot**: its incarnation key, activation,
candidate port and unit.

- **Owner.** Before any transaction takes the target, continuity supervision
  owns the incumbent slot: it alone decides to mint a restart. From the
  handover CAS until Complete, the one transaction that took the target owns
  it, fencing, leasing, routing and committing. Its commit hands the slot
  back to supervision as a new generation. A transaction owns its own
  candidate slot from mint or freeze until it is terminal. A pre-handover
  deployment owns nothing else.
- **The handover.** `Engine::take_target` is the only writer of a phase entry
  into `takes_target_at()` (Fencing, or Starting for stop-then-start), for
  both kinds. It is one CAS. It expects the transaction's own envelope, the
  target's admitted envelope (the one `incumbent_generation_id` names, or
  absent), and every other non-terminal transaction of the target, each
  unchanged, and it writes only the transaction. Of two transactions
  entering from one view, exactly one CAS succeeds. The loser's next step
  re-reads: a deployment gate-waits for the owner, and a continuity aborts
  itself as superseded.
- **Inputs.** Continuity: as cut 1, plus whether any transaction owns or
  drains the target, or a continuity is live for it (the mint gate). A
  pre-handover deployment: its plan, its brake, and the admitted generation as
  it is now, which it rebases onto. The handover: the target's records above.
- **Outputs.** The handover writes the new phase. A rebase rewrites
  `incumbent_generation_id` and clears the evidence proven against the old
  incumbent: `warming`, `route_preflight`, `isolation`. A superseded continuity
  writes its own pre-fencing abort.
- **Derived state.** `owns_target()` is derived from the phase and
  `takes_target_at()`. `drains_incumbent()` is derived from the post-commit
  cleanup. The freeze refusal is derived from "a live Deploy for this
  target". `incumbent_generation_id` on a pre-handover deployment is the
  generation its evidence was proven against, not a pin.
- **Forbidden writers.** No path enters `takes_target_at()` except
  `take_target`. Continuity supervision aborts nothing, and mints nothing
  while another transaction owns or drains the target or a continuity is
  live. A deployment never aborts because another transaction owns the
  target: it gate-waits (B18). A continuity is never rebased. Freeze reads no
  continuity state. No new persistent record decides ownership: it is the
  phase.
- **Shared paths.** Candidate-then-promote's Warming-to-Fencing and
  stop-then-start's Sealing-to-Starting go through one phase-entry seam,
  which calls `take_target` exactly when the entered phase is
  `takes_target_at()`. Deployments and continuities use the same CAS. A
  single-target and a `profile:` command use the same freeze rule.

Demotions, named: `incumbent_generation_id` is no longer a pin before the
handover; it is derived from the admitted generation by the rebase.
`blocks_new_target_mutation` is no longer an owner; it splits into
`is_live`, `owns_target` and `drains_incumbent`, each read by the consumer
that asks that question. The freeze busy check is no longer a
target-ownership gate; it guards only one live deployment per target.

Demotions, named: `restarts_exhausted` is no longer an owner of anything. It
is renamed `restarts_page`, and it opens and holds the incident. The
ContinuityExhausted incident was never read as "stopped", and there is now no
stopped state for it to stand for. The "free for a deployment" report is
deleted: every target is free for a deployment between attempts (B10).

## 4. Rationale

The fault is a split brain, not a missing rule. Continuity's "I have stopped"
was written in two places, a durable incident and a log message, and neither
was what the ownership decision read: the yield re-derived "stopped" from a
clock. The withdrawn first spec moved "stopped" into control state. The
operator's ruling is better, and it is also smaller: there is no stopped
state. The window it was derived from becomes a pager. Every case the stop
existed for is then handled by an owner that already exists. A broken release
is replaced by a deployment, which gets in through the gap after each
attempt (B10) and is never yielded (B11). A host-dependency outage costs one
attempt every five minutes or so, and pages.

Why the wait is anchored at "found dead and free" rather than at the mint:
the anchor is the door. A mint-anchored wait can be swallowed by a long
attempt, and then no tick ever sees the target free (B10). Kubelet anchors
CrashLoopBackOff on `FinishedAt` for the same reason, so this is the
established spelling, not a new one. A streak is stored, not recomputed from
the paging log. The log holds only six entries and slides by the hour, while
the streak must outlive both. Reading the backoff from the window count was
today's spelling, and it is exactly the mutant the cut's tests must kill.

The reset, "admitted after the last charged attempt and ran at least 600 s
before it was found dead", needs no new writer. Supervision observes the
admitted generation on every pass, so the first pass that finds it dead is
within one poll of the death. A death hidden behind a live transaction or an
unobservable host overstates the run, and so resets early. That is the safe
side, because it shortens one wait.

The reboot invariant needs evidence of what happened, not a guess. "Unit
absent" alone is not a reboot, because a cleanly stopped transient unit is
collected too. "Host booted after admission" alone is not a loss, because
the unit might have been started this boot. The systemd driver owns both
facts (a `systemctl show` that returns nothing, and the `/proc/stat` btime).
So it answers one question, `lost_to_host_reboot`, and supervision keeps the
budget. The once-per-admission bound is still needed under never-stop. A
failing re-materialization leaves the same admitted generation, unit absent,
booted after admission. Without the bound it would repeat with no step, no
wait and no page, every tick. The clock comparison is safe: admission
happens at least one full transaction, tens of seconds, after boot.

What cut 1 deliberately does not fix. A dependency failure still grows the
streak (cut 3). A dead incumbent stays down while a pre-fencing deployment
builds over it (cut 2). A dead generation is still route-supervised and
topology-refreshed every tick (cut 4). Paging residual: the cap gives 3600 /
(300 + D) attempts an hour, where D is the attempt's duration, so a crash
loop whose attempts last longer than 300 s settles under 6 an hour. Its
incident opens in the first hour's burst and stays open while the release
never runs, but a release that runs for one pass after each restart can see
it close and not reopen. That is the operator's threshold, applied as ruled.
It is recorded here, not changed.

Replayed against 2026-10-18 with only cut 1. The reboot loss at 11:46 is
re-materialized at once, with no step. It fails on the nginx outage. The
next attempt resets the streak, because the generation had run for hours,
and then waits 5, 10, 20, 40, 80, 160 s, and 300 s after that. The incident
opens within the first hour and stays open (the release never runs), so the
operator is paged during the outage, not at 20:24. The `idunn up` at 20:24
freezes in the next gap. Continuity defers to it, nothing aborts it, and it
fences the absent incumbent as nothing and admits the new release.

Why cut 2 is shaped this way. The handover is the phase, guarded by one CAS
over the target's records, rather than a new per-target "owner" record. A
record would be a second spelling of what the phase already says, and it
would need its own lifecycle, crash repair and migration. A CAS that reads
every other live transaction of the target is enough because a newly minted
or frozen transaction starts at Sealing and cannot own anything (B16).
The rebase clears exactly the evidence that names an incumbent (B15): warming
names its lease, preflight its route and isolation its workload. Keeping any
of them would let the deployment fence one generation with proofs about
another. A deployment waits in Warming while another transaction owns the
target, because Warming's steps read the incumbent that owner is fencing, and
a failed pre-fencing step is an abort (B18). Sealing and Starting keep
running, so the long part, the build, still overlaps. A continuity that loses
the target is aborted, not parked: it restarts a generation that is about to
be replaced, and while it lives it blocks continuity of the generation that
replaces it.

Prior art on question `fencing-over-owning-continuity` (wait for a
continuity that already owns the target, or preempt it), from
`docs/research/restart-budget-prior-art.md` at `74a7008`. It leans the other
way from the recommendation, and the recommendation says why. Kubernetes'
rolling update scales old ReplicaSets down unhealthy pods first, so that
"unhealthy replicas will block deployment" does not happen (§1.4,
`rolling.go`). That is preemption of a failing old replica. Nomad separates
the two by layer: the client restarts old allocations under their own policy,
and the server reconciler places the new ones (§2.3). Neither waits on an
old replica's restart. But deleting a pod is the kubelet's ordinary teardown.
In Idunn, preempting a continuity past its own Fencing means a deployment
driving another transaction's post-fencing abort. That abort revokes a lease
and restores a projection, and it is the path that wedged Ghostlight (B7).
Waiting is bounded by the owner's own phase deadlines (`PhaseDeadline`,
`:938-960`) and adds no new abort path. Recommendation: wait, at medium
confidence. Preempt is the better answer once post-fencing aborts are known
not to wedge, which is follow-up `ghostlight-abort-lease-before-stop`.

Residuals, recorded and not fixed. A wedged post-fencing abort of a
continuity (B7, follow-up `ghostlight-abort-lease-before-stop`) holds a
waiting deployment as long as it holds the target. A continuity superseded
after its mint stays charged; that lengthens one wait for a generation that
is being replaced. Stop-then-start has no control-plane fixture (B17), so in
cut 2 its edge is pinned structurally (one seam, one call) and by a unit test
of the phase mapping, not by an engine run. raven-muninn uses it, so the
engine test is owed before cut 5.

## 5. Cut sequence

1. **c1-never-stop**: spec in the mind (specs-with-no-report recipe). Depends
   on nothing.
2. **c2-phase-split** (`handover-phase-split`): spec
   `cut-c2-phase-split.r1` in the mind, branch `idunn/reincarnation-c2` from
   `9a0a6a2`. Continuity restarts the incumbent while a pre-fencing
   deployment builds. The ownership predicates split (B13, B14). The handover
   is one CAS at `takes_target_at()`. A pre-handover deployment rebases onto
   whatever is admitted, and a superseded continuity aborts itself. Freeze
   refuses only a second live deployment for a target, which dissolves
   follow-up `profile-deploy-joint-gap`. It folds in the test-only follow-up
   `c1-sustained-reset-test-reach`. Open question:
   `fencing-over-owning-continuity`. Depends on cut 1 (merged at `1da150a`).
3. **Dependency failures do not grow the backoff**
   (`budget-never-stop`): an attempt that failed because of a declared
   dependency (Odin's stable route, the target's own stable route
   listener), as Idunn itself observed it, leaves the streak unchanged.
   Waits on follow-up `odin-route-record-stale`, because the record it
   would read is not trustworthy, and on cut 1.
4. **A dead generation is not supervised as alive** (target invariant 5):
   observe the workload before route supervision and topology refresh, so
   that a dead admitted generation gets no topology authentication and no
   lease-exactness challenge. Its non-binding correlation is reported once,
   not every tick. Waits on cut 1, because it rewrites the same function.
5. **Closing proof on Yggdrasil**: an operator-approved install of the merged
   cuts. streampixels-service survives the install, and `idunn status` is
   checked before and after. Waits on 1-4 and on follow-up
   `stop-first-handover-fixture` (raven-muninn is stop-then-start, B17).

Ops, out of scope: nginx boot order (`gamecult-ops`), and retention of the
roughly 130 dead transient units.

## Installed

Cut 1 (`1da150a`, merge of `idunn/reincarnation` at `ffb4293`) was installed on
Yggdrasil on 2026-10-19 at 08:34:45 UTC with the operator's OK. The binary is
`/usr/local/bin/idunn` sha256 `f0e58308…`, built in `/srv/build/idunn-1da150a`
(`cargo test --lib`: 414 passed). The pre-install store backup is
`/root/idunn-store-backup-*-pre-reincarnation-c1`. Restoring it means restoring
its stores and `idunn.running` together, because the cb60965 binary cannot read
`idunn.target_supervision.v2`. At first open Idunn logged
"migrated 5 control record(s) … idunn.target_supervision.v2". streampixels-service
kept running through the restart (unit 73ca40ba, PID 968752, NRestarts 0). The
catalog returned 200, and 4443/udp was listening.
