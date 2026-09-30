# Watchdog reporting: map

Campaign `idunn-watchdog`. Questions, rulings, cut specs, reports and verdicts
are typed documents in Eureka's mind; this file keeps only body facts, the
model page and rationale. Target rationale: `watchdog-reporting-target.md`
(`4951125`).

## Body facts

Source reads are at Idunn `4951125` (main), Idunn `hands/b5` (`0c80a44`,
unmerged, read with `git show`/`git diff main...hands/b5` only), Bifrost
`42728a6`, Odin `cdc57bd`, VoidBot `46d891b`, gamecult-ops `14df7ce`. Host
probes ran on Yggdrasil over `ssh ygg`, read-only, 2026-09-30 13:51-13:55 UTC
(15:51-15:55 CEST).

### Idunn

| # | What | Where | Result |
|---|---|---|---|
| I1 | Alert path | `src/control_plane.rs:3798-3812` (`ReportOnce`), `:4599-4610` (`report_once`), `:5114-5138` (`note_fault`, `clear_fault`) | The only operator signal is `eprintln!`. De-dup is `fault_reports: Mutex<BTreeMap<String, ReportOnce>>` (`:4392`), in memory: an Idunn restart forgets it and reprints. `note_fault` also writes the transaction's `last_error`. |
| I2 | Continuity exhaustion | `:68-71` (6 restarts / 3,600,000 ms, 5 s doubling), `:1066-1072` (`restarts_used`, `restarts_exhausted`), decided `:5709-5722` under key `continuity:<target>` (`:5678`) | Exhaustion is **derived** from `TargetSupervision.continuity_restarts` (newest 6 times, sliding window). No write marks it. The fault clears (`:5723`) as soon as one entry leaves the window, so a permanently dead target leaves and re-enters exhaustion several times each hour. |
| I3 | Route degradation | `RouteSupervisionState` `:890-942`; `record_failed_challenge` sets `degraded_since_unix_millis` on the **first** failed challenge, `record_proved_challenge` clears it; reported at `:5975` under `route:<target>` | Durable per incarnation (inside `admitted_generation.v4`); a new generation starts from `default()`. Backoff doubles from the topology max age to a cap. |
| I4 | Other operator-facing faults | `:5645-5651` (`generation-down:<target>`, "holds a dead admitted generation down"), `:6016` (`route-record:<target>`), `note_fault` per transaction | Also stderr plus `last_error`. Not named by the target; listed so the paging-set question sees them. |
| I5 | `OperatorRequired` on main | `TerminalRecovery::OperatorRequired` `:878-885`, validated `:1954`; constructed only in tests (`:12525`, `:12535`) | No production path produces it on main. |
| I6 | `OperatorRequired` on B5 | `hands/b5` `resolve_phase_deadline` (diff hunk at `control_plane.rs` ~8560-8625 on the branch), `PhaseEnd::OperatorRequired` | Deadline passed on a stateful deploy with an issued lease: `report_once("deadline:<transaction_id>")`, returns `Ok(None)`, the transaction stays live. No terminal transition and no operator action that ends it were found on the branch (`idunn expire` writes `idunn.expiry_request.v1`, which re-enters the same arm). |
| I7 | Store kinds | `:55-61`, `:936-966`; B5 adds `idunn.expiry_request.v1` | `control.cc`: `idunn.deployment_command`, `idunn.deployment_transaction`, `idunn.admitted_generation.v4`, `idunn.target_supervision.v1`. `history.cc` beside it holds terminal transactions (`:3653`), retired one per tick (`:5068-5091`). |
| I8 | Single-writer precedent inside `control.cc` | B5 `ExpiryRequest` doc comment | "The CLI only writes this record. The daemon's deadline resolver reads it ... No CLI writes a transaction field." One writer per kind. |
| I9 | Serve loop | `:5041-5058`, `run_scheduler_tick` `:5098-5110` | 500 ms poll. A tick error is logged, never propagated. |
| I10 | `idunn status` | `render_supervision` `:4215` | Renders restarts used/limit and `degraded-since`; no incident notion. |
| I11 | Idunn already talks CultNet to targets | `src/drivers.rs:5567-5600` `request_runtime_presence_at` | `CultNetMessage::SnapshotRequest { schema_ids, record_keys }` over the route transport (http/tcp/rudp) to a target endpoint. Precedent for reading a typed record out of Odin's catalog. |
| I12 | Idunn reads Odin's file read-only | `src/drivers.rs:5005-5020` (`receive`), unit `--odin-correlation-store /var/lib/gamecult/odin/topology.cc` | Absent file yields `None`; continuity never consults it. |
| I13 | Idunn's published surface | unit `deploy/idunn-yggdrasil.service` comment on `ReadWritePaths` | "the topology store is Idunn's *published* surface, and every managed target must read it". Lives in `/var/lib/gamecult/idunn-projection`, outside Idunn's 0750 root. |
| I14 | Rejected predecessor | `git show 821a8d0^:src/main.rs:2020-2060, 8206-8250` | Before the 2026-09-02 rebuild, Idunn wrote an `IdunnOperatorAlarmRecord` (`alarm_id = alarm:<daemon>:<now>`, one per decision) and ran an operator-supplied shell `--operator-alarm-command` synchronously with a timeout, passing `IDUNN_ALARM_*` env. The rebuild deleted it. |
| I15 | Stale guide lines | `docs/guide.md:47` "escalate to an operator through Bifrost"; `:328` "Idunn is not Bifrost ... Discord delivery, owner DMs" | `:47` describes a capability that does not exist; `:328` stays true. |

### Yggdrasil

| # | Command | Result |
|---|---|---|
| Y1 | `systemctl is-active idunn-yggdrasil bifrost bifrost-persona-mouth bifrost-persona-feedback` | all `active` |
| Y2 | `systemctl show idunn-yggdrasil -p User -p ProtectSystem -p ReadWritePaths` | `User=root`, `ProtectSystem=full`. `ReadWritePaths` names no Bifrost path, but `ProtectSystem=full` only makes `/usr`, `/boot`, `/etc` read-only, so root Idunn can physically write `/srv/bifrost` and `/var/lib/gamecult/bifrost`. The `/srv` and `/var/lib` entries are documentation, not a boundary. |
| Y3 | `ls -la /srv/bifrost/env` | `persona-delivery.env` **exists**: `root:root 0600`, 135 bytes, 2026-09-04 02:20. gamecult-ops `inventory.md:741` ("the file is absent") is stale. |
| Y4 | `sudo awk -F= '{print $1, length($2)}' persona-delivery.env` (values not printed) | `BIFROST_DISCORD_BOT_TOKEN` 72 chars, `DISCORD_OWNER_ID` 18 chars. The token and the recipient binding are both already in Bifrost's env. |
| Y5 | `systemctl cat bifrost-persona-mouth` | `User=bifrost-feedback`, `ProtectSystem=strict`, `EnvironmentFile=-/srv/bifrost/env/persona-delivery.env`, `ExecStart=node .../tools/persona-feedback.mjs serve-persona-delivery ...` with signed-request RUDP listener `10.77.0.1:17876` and Starfire permit requester `rudp://10.77.0.2:17877`. |
| Y6 | `systemctl show bifrost-persona-mouth -p ActiveEnterTimestamp -p NRestarts` | Started 2026-08-21 10:51:22 UTC, 0 restarts: **before** the env file was written, so the running process has no token loaded. |
| Y7 | `readlink -f .../persona-feedback/runtime/current`; `grep -c discord-dm .../cultmesh-bridge-commands.mjs` | Current release `cb3239a` (2026-08-08). It ships `cultmesh-bridge-commands.mjs`, `operator-notification.mjs`, `bifrost-bridge.mjs`, but `c063f27` (discord-dm verb, 2026-09-04) is **not** an ancestor: the deployed pump cannot send a DM. `discord-dm` count 0. |
| Y8 | `find /srv/bifrost /var/lib/gamecult/bifrost -name provider-store.cc` | None. The pump's store has never existed on the host. |
| Y9 | `getfacl /var/lib/gamecult/idunn-projection`; `ls -ld /var/lib/gamecult/idunn` | Projection dir `root`, `other::r-x`, default `other::r--`: world-readable. `/var/lib/gamecult/idunn` is `idunn:idunn 0750`; `control.cc` is unreadable to other users. |
| Y10 | `id idunn`; `getent passwd bifrost bifrost-feedback` | `idunn` 986/978; `bifrost` 111/113; `bifrost-feedback` 994/983. No shared group. |

### Bifrost

| # | What | Where | Result |
|---|---|---|---|
| BF1 | Alarm publisher | `tools/operator-notification.mjs:41-148` | Opens Bifrost's `.bifrost/provider-store.cc` (repo-relative, `:20`) and `put`s a `bifrost.bridge.discord_post_command.v1` `discord-dm`. The **caller** supplies `recipientId` (`:51-54`). `commandId = idunn-alarm-sha1(alarm JSON)[0..16]`, and the JSON includes `raisedAt`, which defaults to now (`:48`): a re-publish after a restart mints a new id. |
| BF2 | Pump | `tools/cultmesh-bridge-commands.mjs:50-71, 133-221` | One-shot `process`: selects `pending` and `running`, marks `running`, spawns `bifrost-bridge.mjs`, writes `discord_post_receipt.v1` and `completed`/`failed`. `failed` is terminal (no retry). A crash after the Discord POST and before the receipt leaves `running`, which the next run resends. |
| BF3 | Discord send | `tools/bifrost-bridge.mjs:735-770` | Opens the DM channel, posts `content`. No `nonce` is sent (`grep nonce` hits only the Persona permit tools). |
| BF4 | No caller authentication on the command path | BF1, BF2 | Whoever can write the store file can make Bifrost post anything to anyone. |
| BF5 | Authenticated precedent in Bifrost | `tools/persona-discord-delivery.mjs:27-30`, `tools/persona-discord-permit.mjs` | Persona crossing: signed requests, Epiphany-signed permits, "permit request intent persisted before network" in an execution journal. |
| BF6 | History | `git log -S publish-idunn-alarm`: `755649f` (2026-06-07), `c063f27` (2026-09-04) | `c063f27`: "an alarm is a command addressed to the gate, not a document dropped into somebody else's state." |

### Odin and VoidBot

| # | What | Where | Result |
|---|---|---|---|
| O1 | `odin.store_condition.v1` | Odin `docs/store-recovery-cut.md:158, 199, 318-325` | Not built. One record, key `topology`, held in Odin's in-memory W, rewritten on change, **flushed with the store** and served by Odin's catalog. While `contested` Odin writes no file, and while `unpersisted` it cannot: in exactly the conditions that matter, `topology.cc` on disk does not carry them. Only the catalog does. |
| O2 | I-F1, I-F2 | Odin `docs/store-recovery-cut.md:496-500` | I-F2 is this campaign's continuity-exhaustion row. I-F1 (hold deploys on a bad condition) is a deployment-brake decision, not reporting. |
| V1 | Peer precedent | VoidBot `packages/core/src/bifrost-discord-command.ts:39-68` | The caller opens Bifrost's store, derives `commandId` from its `idempotencyKey`, optionally spawns the pump itself, and **waits** for the receipt. An unconditional `put` of an existing id overwrites a `completed` command back to `pending`. |
| V2 | VoidBot `notify_owner` | Self's mapper, not re-probed | Not live on Yggdrasil; no Discord-capable VoidBot process runs there. |

## Model page

A row per persistent kind this campaign creates or depends on. "New" means the
kind does not exist yet. Names in `code` for new kinds are working names; the
cut spec fixes them.

| Kind | Identity | Lifecycle | Authority |
|---|---|---|---|
| **Incident record** (new, Idunn `control.cc`, e.g. `idunn.operator_incident.v1`) | Key `<condition>:<subject>:<opened_at_unix_ms>`. Namespace: Idunn's control store, one type. `condition` is a closed enum (`continuity-exhausted`, `route-degraded`, `operator-required`, `odin-store-contested`, ...). `subject` is the target for target conditions and the `transaction_id` for `operator-required`. `opened_at` is written once at opening by Idunn's clock. Injective because of one rule: **at most one open incident per `(condition, subject)`**; the opener checks it before writing. Two different faults on one target are different conditions, so they never collide. Nothing in the key comes from Bifrost or Odin. | **Created** at the site that decides the condition (I2, I3, I6, Odin read) in the same store transaction family as that condition's own write, before any trace line. **Closed** by a write of `closed_at` and `close_reason` on the same record, never a delete: continuity-exhausted closes when the supervision pass sees the admitted workload running or a new generation is admitted, **not** when the window slides (I2 would flap otherwise); route-degraded closes on `record_proved_challenge` or when its generation is replaced; odin-store-contested closes when Odin's catalog stops reporting it; operator-required closes when its transaction leaves the phase, for which **no path exists yet** (I6). **Reopened**: never revived; a recurrence is a new record with a new `opened_at`. **Replayed after an Idunn restart**: the record is durable, so the decision site finds the open incident and writes nothing; `ReportOnce` is not consulted. **Pruned**: a closed record retires to `history.cc` one per tick, like a terminal transaction, after the projection has carried its closure for a retention period (length is a cut detail). After pruning, `idunn status` shows only open incidents. | Idunn's daemon, at the condition's decision site, is the only writer. Forbidden: the CLI (it may request, as `ExpiryRequest` does, never write the record), Bifrost, Odin, any reader of the projection, `ReportOnce`, a repair loop that opens incidents from log text. |
| **Incident projection** (new, derived) | One file in `/var/lib/gamecult/idunn-projection/` beside `topology.cc`, holding the open incidents plus closed ones inside retention, keyed exactly as the record. World-readable (Y9). | Rewritten whenever an incident opens or closes. Idunn restart: regenerated from `control.cc`. Never an input to Idunn. | Derived, notification-only. Idunn writes it; nothing else does. Whether it is signed with Idunn's service identity, as the topology projection is, follows that precedent (cut detail). Forbidden: Idunn reading it back, any Bifrost write. |
| **Stderr trace** (exists) | `report_once` key, in memory | Printed once per process lifetime per fault; lost on restart (I1). | **Demoted**: `ReportOnce` is no longer an owner of "has the operator been told". It de-dups stderr only. |
| **Delivery request / journal entry** (Bifrost store) | Keyed by the Idunn incident key plus the notice kind (`opened`, and `closed` if Q `closure-notice` says so). Injective because the incident key is. Not derived from payload text or time (BF1's `raisedAt` hash is the counter-example). | **Created** by Bifrost when its reader first sees a notice-worthy incident state in the projection. **Revised**: `pending` -> `sent` or `failed`, with an attempt count and last error. **Retried** until sent, with backoff; a create for an existing key writes nothing (V1's unconditional `put` is the counter-example). **Replayed after a Bifrost restart**: journal is durable; a `pending` entry is retried. **Pruned** only after the incident key has left Idunn's projection. | Bifrost's reader only. Forbidden: Idunn (it never writes Bifrost's store, even though Y2 shows it physically could), VoidBot, the operator by hand. Which Bifrost process: see Rationale. |
| **Delivery receipt** (Bifrost, `discord_post_receipt.v1` or the journal's `sent` state) | Same key as the request. | Written once on success with the Discord message id. | Bifrost. **Idunn does nothing with it**: it neither reads it in the daemon nor waits on it (`survival-independent`). `idunn status` does not show delivery state; Bifrost's journal is where a failed delivery is visible. |
| **Recipient binding** | `DISCORD_OWNER_ID` in `/srv/bifrost/env/persona-delivery.env` (Y4). One recipient, the operator. | Operator-supplied; changes by editing that file. | Bifrost (with the operator as supplier). Forbidden: Idunn and every request payload. BF1's caller-supplied `recipientId` is the counter-example and does not survive. |
| **Bot token** | `BIFROST_DISCORD_BOT_TOKEN` in the same file (Y4), the one GameCult bot. | Operator-supplied, root `0600`, loaded by systemd `EnvironmentFile`. A process started before the file changed has the old value (Y6). | Bifrost holds it; the operator supplies it. Forbidden: Idunn, any notice content (`no-sensitive-egress`). |
| **Odin store condition** (input, not built) | `odin.store_condition.v1`, key `topology` (O1). | Owned by Odin's campaign. Rewritten on change; `contested` and `unpersisted` exist only in Odin's memory and catalog, not on disk. | Odin. Idunn reads it through Odin's catalog with a `SnapshotRequest` (I11 precedent), never from `topology.cc`, and never writes it. An unreachable Odin yields no store incident; continuity already covers a dead Odin. |
| **De-dup key** | Not a separate kind: it **is** the incident key. At Idunn: one open incident per `(condition, subject)`. At Bifrost: one journal entry per `(incident key, notice kind)`. At Discord: the journal key hashed into the message `nonce` with `enforce_nonce`, which covers a crash between the POST and the journal write. The Discord de-dup window is short and unverified here; outside it, a crash in that gap can still send twice. | Lives as long as the incident and its journal entry. | Idunn owns the incident half, Bifrost the delivery half. |
| **Delivery-failure signal** | **Empty.** Nothing tells the operator that the watchdog's own delivery is failing. | | Bifrost's journal holds the state; nobody surfaces it. Out of this table's reach; left for the delivery-mechanism cut (see Rationale). |
| **Operator acknowledgement** | **Not created.** No kind lets the operator mark an incident as seen. Nothing in the target asks for one. | | |

Empty cells, stated plainly:

- `operator-required` closure: no path on `hands/b5` ends a transaction left to
  the operator (I6). The incident can open and never close until B5 names that
  action.
- Delivery-failure visibility: no owner.
- Discord `enforce_nonce` window: not verified against Discord's current docs
  in this pass.

## Rationale

**Idunn publishes and Bifrost reads, not the reverse.** Three crossings were
weighed against the target:

1. *Idunn writes a command into Bifrost's store* (the `c063f27` shape). Idunn
   would write another organ's state; it would carry the recipient (BF1); an
   unconditional `put` re-arms a completed command (V1); and the deployed pump
   cannot send a DM anyway (Y7). Nothing stops root Idunn from doing it (Y2),
   so the boundary would be doctrine alone.
2. *Idunn sends a CultNet command to a Bifrost listener.* Idunn becomes a
   delivery client with timeouts and retry state in its tick, which is where
   `survival-independent` breaks. The rebuild already deleted the synchronous
   version of this (I14).
3. *Idunn writes its incident into its own store and its world-readable
   projection; a Bifrost reader derives its own delivery journal.* No write
   crosses an owner boundary. Idunn holds no recipient and no token. Bifrost
   down means a late message, not a slow tick. This is the shape of the
   existing topology projection (I13), which every managed target already
   reads. **Recommended.** It reverses `c063f27`'s direction, so it is a
   question for the operator, not a default.

**Which Bifrost process delivers is doctrine, not a fork.** A service earns a
daemon only when isolation protects a named invariant. A notice can wait a
minute, so a systemd timer running a one-shot Bifrost reader under a Bifrost
user, with `EnvironmentFile=/srv/bifrost/env/persona-delivery.env`, is the
smallest body. Folding it into `bifrost-persona-mouth` was rejected: that
process runs the Epiphany-permit-gated Persona crossing, and a watchdog page
that needs Starfire's permit would stop whenever the workstation is off.
Idunn-invoked delivery was deleted once already (I14).

**DM is the default, not a question.** The operator's words are "I ought to
get a message", and the recipient binding that exists is `DISCORD_OWNER_ID`.

**Retiring `publish-idunn-alarm` follows from the crossing.** If the crossing
is (3), nothing calls the publisher. The unauthenticated command path (BF4)
then carries no watchdog traffic. Closing it is Bifrost's own business, and
VoidBot's caller is not live on Yggdrasil (V2). If the operator picks (1), that
path must be authenticated before Idunn uses it.

**Exhaustion closes on health, not on the window.** `restarts_exhausted` turns
false the moment one timestamp leaves the hour (I2). With a still-dead target
that happens several times an hour. Closing on it would mean one incident, and
one notice, per slide.

**Why `ReportOnce` is demoted rather than extended.** Persisting its map would
make an in-memory de-dup cache the owner of "was the operator told", keyed by
free-form strings, with no closure and no subject. The incident record puts
that decision at the condition's own site, as `record-before-delivery` asks.
