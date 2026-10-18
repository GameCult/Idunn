# Reincarnation after reboot: map

Status: map, Imagination, 2026-10-18 (campaign `idunn-reincarnation`, session
`idunn-reincarnation-2026-10-18`). The ends are in
[`reincarnation-target.md`](reincarnation-target.md). Questions, rulings, the
first cut spec and follow-ups live in Eureka's mind; this page holds what has
no kind: body facts, the model page, the authority map's rationale and the
cut sequence. Line numbers are at `c2477d4`.

In force from the mind: `idunn-reincarnation:cut_spec:cut-c1-durable-stop.r1`;
questions `idunn-reincarnation:question:handover-pre-fencing` and
`idunn-reincarnation:question:budget-charge`; defaulted rulings
`stop-is-control-state`, `stop-rearms-on-window`,
`rematerialize-once-per-admission`; follow-ups
`ghostlight-abort-lease-before-stop`, `host-actuator-reboot`,
`odin-route-record-stale`. Cited from `idunn-watchdog`:
`exhaustion-close-rule-op` (an exhaustion incident closes when the target runs
and the window no longer holds it) and `paging-conditions` (page on
exhaustion).

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
`odin-route-record-stale`, and it blocks the recommended answer to
`budget-charge`, which reads dependency health from this record.

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

## 2. Model page

One row per persistent kind the campaign touches.

| Kind | What names it | Lifecycle | Who decides |
|---|---|---|---|
| Continuity budget: `TargetSupervision.continuity_restarts` (control.cc, `idunn.target_supervision.v1`) | key = target | Ascending times, at most 6, sliding 1 h window. It outlives every generation. Charged at mint today, and after `budget-charge` at outcome | Continuity supervision, in the mint CAS |
| Continuity stop (new, cut 1: keys 7-8) | target, plus the admitted `generation_id` it names | Written when the window is spent. Cleared by the next continuity mint (an explicit resume, `stop-rearms-on-window`). Inert once another generation is admitted | Continuity supervision only (`stop-is-control-state`) |
| Re-materialization bound (new, cut 1: key 9) | target | Set by the one uncharged re-materialization. Spent while it is not older than the admitted generation's `admitted_at` | Continuity supervision, from the workload driver's evidence |
| Admitted generation (`idunn.admitted_generation`) | key = target, `generation_id` = `generation-<tx>` | Replaced atomically by every committed deployment or continuity. A dead incarnation keeps its record until replaced | The committing transaction |
| Incident `ContinuityExhausted` (incidents.cc) | condition + subject (target) | Opened on exhaustion. Closed when the target runs and the window no longer holds it (`exhaustion-close-rule-op`). Retired after retention | Idunn's incident writer, a notification projection: never read by ownership |
| Transaction and pre-fencing abort (`idunn.deployment_transaction.v4`, history.cc once terminal) | `tx-<uuid>` and its `command_id` (`up-…` or `continuity-…`) | Sealing … Complete. A pre-fencing abort completes `FailedBeforeFencing`, a post-fencing abort `FailedAfterFencing` with a named recovery. Retired to history | The scheduler advances it. Continuity supervision may begin a pre-fencing abort of a Deploy only while continuity is live |
| Unit materialization (systemd transient unit `idunn-<target>-<hash>`) | the activation's hash | Created by Starting. Lost on a host reboot (`/run/systemd/transient`). A failed unit stays loaded; a cleanly stopped one is collected | The workload driver actuates. Whether a loss was a reboot is the driver's evidence (`WorkloadPort::lost_to_host_reboot`, cut 1) |

No cell is open. The two forks are questions with recommendations, and cut 1
does not depend on either of them.

## 3. Authority map: continuity and deployment

The authority map for cut 1 is in its spec. This section is the campaign-level
map that the spec is one step of.

- **Owner.** Exactly one authority actuates a target at each instant. A live
  `DeploymentTransaction` owns the target from Fencing on. Before Fencing,
  ownership is the open fork `handover-pre-fencing`; the recommendation is that
  the deployment owns only its candidate and continuity owns the incumbent
  slot. Continuity owns the admitted generation's survival, and whether it is
  live or stopped is its own durable record.
- **Inputs.** For continuity: the admitted generation, the workload driver's
  observation and reboot evidence, its restart log, and the live
  transaction's kind and phase. For a deployment: its own plan, its brake, and
  the admitted generation at Fencing.
- **Outputs.** Continuity: mints, charged or uncharged, a stop record, and an
  abort of a pre-fencing deployment only while it is live (until
  `handover-pre-fencing` removes even that). Deployment: a new admitted
  generation, or an abort that names its recovery.
- **Derived state.** `restarts_exhausted(now)` is derived and is no longer an
  ownership input. The ContinuityExhausted incident and the "free for a
  deployment" report are notifications. `idunn status` is display.
- **Forbidden writers.** The sliding window may not decide a yield. The
  incident store may not decide ownership. No deployment path writes
  continuity's stop. Continuity never aborts a deployment that it has stopped
  in front of. A window slide never resumes continuity while any transaction
  owns the target.
- **Shared paths.** Crash restart, reboot re-materialization and resume all
  mint through the one mint CAS (`:6158-6183`). Deployments, whether operator
  or scheduled, go through `advance_transaction`.
- **Deletion line.** The window term in the yield (`:6046`) dies first.
  Under `handover-pre-fencing` = phase-split, the whole yield
  (`:6033-6061`) and the blocker gate on continuity die in cut 2.

Demotions, named: `restarts_exhausted` is no longer an owner of the handover.
It is derived from the log and feeds the stop write, the incident and status.
The ContinuityExhausted incident is no longer read as "stopped". It is derived
from exhaustion.

## 4. Rationale

The fault is a split brain, not a missing rule. Continuity's "I have stopped"
was written in two places, a durable incident and a log message. Neither was
the place the ownership decision read; the yield re-derived "stopped" from a
clock. Each answer was right at the moment it was computed, and they disagreed
as soon as the clock moved. The tempting patch, not yielding if an exhaustion
incident is open, would make a projection store decide ownership and add a
second reader of a record whose writes "never wait" and may fail. The coherent
cut moves "stopped" into the record the decision reads (control.cc, beside the
log it summarises), deletes the clock from the decision, and binds the stop to
the generation, so a new admission ends it without another writer.

The reboot invariant needs the evidence of what happened, not a guess.
"Unit absent" alone is not a reboot: a cleanly stopped transient unit is
collected too. "Host booted after admission" alone is not a loss: the unit
might have been started this boot. The systemd driver owns both facts (a
`systemctl show` of nothing, and `/proc/stat` btime), so it answers one
question, `lost_to_host_reboot`, and supervision keeps the budget. The bound,
once per admission, keeps a reboot loop finite without inventing a boot
ledger. It compares two wall clocks (Idunn's admission time and the kernel's
boot time). Admission happens at least one full transaction, tens of seconds,
after boot, so skew of a few seconds cannot misclassify it.

What cut 1 deliberately does not fix: dependency failures still charge
(`budget-charge`), a live continuity still kills a pre-fencing deployment
(`handover-pre-fencing`), and a dead generation is still supervised every tick
(cut 4). Replayed against 2026-10-18 with only cut 1: the reboot
re-materialization at 11:46 is free, the next five attempts are charged and
fail on the nginx outage, and the stop is recorded at about 11:53. The
operator's `idunn up` at 20:24 is then not aborted at 20:46. It proceeds over
the absent incumbent and admits the new release. Without that deployment,
continuity resumes explicitly each hour, as it did, under
`stop-rearms-on-window`.

## 5. Cut sequence

1. **c1-durable-stop**: spec
   `idunn-reincarnation:cut_spec:cut-c1-durable-stop.r1`. Depends on
   nothing.
2. **Handover before Fencing**: waits on `handover-pre-fencing`. With
   phase-split it deletes the yield and the blocker gate on continuity, admits
   one continuity and one pre-fencing deployment per target, and makes the
   deployment rebase its incumbent at Fencing (`:7556`).
3. **Budget measures the body**: waits on `budget-charge` and on follow-up
   `odin-route-record-stale`. It moves the charge from the mint to the
   terminal outcome, classified against Idunn's own dependency observation.
4. **A dead generation is not supervised as alive** (target invariant 5):
   observe the workload before route supervision and topology refresh, so that
   a dead admitted generation gets no topology authentication and no
   lease-exactness challenge. Its non-binding correlation is reported once,
   not every tick. It waits on cut 1, because it rewrites the same function.
5. **Closing proof on Yggdrasil**: an operator-approved install of the merged
   cuts. streampixels-service survives the install, and an `idunn status`
   check is made before and after. It waits on 1-4.

Ops, out of scope: nginx boot order (`gamecult-ops`), and retention of the
roughly 130 dead transient units.
