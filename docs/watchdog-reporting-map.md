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

### Cut-mapping probes (2026-09-30, 14:00-14:10 UTC)

Pinned heads: Idunn `487f4f14` (origin/main), Bifrost `42728a67`, gamecult-ops
`14df7ce4`, all read with `git ls-remote origin refs/heads/main`. Line numbers
in Idunn are unchanged from `4951125`, because `153af6a` and `487f4f1` touched
docs only.

| # | What | Where / command | Result |
|---|---|---|---|
| C1 | `control.cc` refuses unknown types | Idunn `src/control_plane.rs:2845` | `_ => bail!("Idunn control store contains a foreign document")`. A new type in `control.cc` makes every older binary unable to read the store at all, so a rollback of the incident cut would take continuity down for every target. |
| C2 | Idunn's store writes take a blocking exclusive lock; reads can skip it | CultLib `packages/cultcache-rs/src/lib.rs:737-785` | `pull_all_read_only_snapshot` reads unlocked. Writes are `fs2::lock_exclusive`, which blocks. A reader that held the lock could stall an Idunn write. |
| C3 | The TS store takes no lock | CultLib `packages/cultcache-ts/src/single-file-messagepack-backing-store.ts` (no `lock` anywhere); present at Bifrost's pinned CultLib `f67f5122` (`pullAll` at `:44`) | A Bifrost reader using `pullAll` cannot block Idunn. |
| C4 | A JS reader decodes an Idunn-written store | `scp ygg:/var/lib/gamecult/idunn-projection/topology.cc`; scratch `probe-read.mjs` with cultcache-ts `pullAll` | 18 envelopes decoded (`gamecult.service_trust_anchor.v1`, ...). No `.lock` file was created beside the copy. |
| C5 | Rust `DatabaseEntry` payload shape in JS | scratch `probe-decode.mjs`, `@msgpack/msgpack` `decode(envelope.payload)` | A **positional array**: index `n` is `#[cultcache(key = n)]`. The Bifrost reader decodes a tuple, so the seam must be pinned by bytes Idunn wrote, not by a hand-built object. |
| C6 | Projection publish mode | Idunn `src/drivers.rs:4385-4404`, `:7670-7685` | Publication is compare-exchange, then `chmod 0644` on the store and its `.lock` sibling. |
| C7 | Continuity's healthy site | Idunn `src/control_plane.rs:5631-5633` | `if observation.is_some() { continue; }`: the admitted workload was observed running this pass. The exhaustion decision is `:5709`, and the fault clears at `:5723`. |
| C8 | Test harness for exhaustion | Idunn `src/control_plane.rs:17659-17689` | Existing tests use `routed_world(Odin::Unreachable, 1, ...)`, `set_meters`, `routed.workload.kill()` and `routed.tick()`. |
| C9 | Idunn has no one-shot or timer workload | `grep -i "oneshot\|\.timer\|OnCalendar" src/` returns nothing; recipe schema `src/deployment.rs:1935-2000` | A target is a resident service with `[service]`, health contract and state slots. Idunn cannot deploy a systemd timer. |
| C10 | No Bifrost binding in current Idunn | `sudo ls /etc/gamecult/idunn/bindings/` on ygg | ghostlight, heimdall, odin, raven-muninn, streampixels-service, streampixels-web. No Bifrost. |
| C11 | Bifrost's old Idunn manifest is residue | `/srv/odin/deploy-manifests/bifrost-persona-feedback` on ygg (2026-09-04); `grep deploy-manifests\|IDUNN_ACTUATOR src/` in Idunn returns nothing | It requires `IDUNN_ACTUATOR=1` / `IDUNN_COMMAND_AUTHORITY=idunn-daemon` env from the pre-rebuild Idunn, which no longer exists. gamecult-ops `runbooks/bifrost-persona-feedback-yggdrasil.md` still describes that path. The persona-feedback `current` has pointed at `cb3239a` since 2026-08-08. |
| C12 | How Idunn itself is installed | ygg `/usr/local/sbin/idunn-a96ad9d-build.sh`; `ls -la /usr/local/bin/idunn` (2026-09-30 13:22) | One hand-written script per commit. It runs `cargo test --lib` and `cargo build --release` in a pinned rust image under `/srv/build/idunn-<sha>` and copies the unit. The install is manual. No such script is in any repo. |
| C13 | Legacy alarm consumers in gamecult-ops | `scripts/idunn/notify-idunn-operator-alarm.{ps1,cmd}` (25 + 2 lines); `scripts/idunn/start-idunn-local.ps1:10,134-136` | The only callers of `publish-idunn-alarm`. They pass `--operator-alarm-command` to the deleted pre-rebuild Idunn (I14). No Idunn scheduled task or process runs on Starfire (`Get-ScheduledTask`, `Get-Process`). |
| C14 | `discord-dm` consumers in Bifrost | `grep discord-dm` | `cultmesh-bridge-commands.mjs:152,227` exist only for `operator-notification.mjs` (BF6). `bifrost-bridge.mjs` `discord-dm` (`:48`, `:473-535`) is also used by `docs/bridge.md:156` and `tests/Bifrost.Web.Tests/BridgeCliTests.cs:481`, so it stays. |
| C15 | Bridge CLI contract | Bifrost `tools/bifrost-bridge.mjs:1266-1289`; `tools/persona-discord-delivery.mjs:45` | The bridge refuses to act without `--cultmesh-command-id`. The Persona precedent spawns it with `--receipt-store` under its private state directory and parses stdout JSON. |
| C16 | Crash-recovery precedent | Bifrost `tests/persona-discord-delivery.test.mjs`, "running journal recovers as terminal unknown without a second Discord post" | A `running` execution found at start becomes terminal `unknown` and is never re-posted. |
| C17 | Discord nonce | docs.discord.com/developers/resources/message (fetched 2026-09-30) | `nonce`: integer or string, "up to 25 characters". `enforce_nonce`: "checked for uniqueness in the past few minutes"; on a duplicate from the same author, "that message will be returned and no new message will be created". |
| C18 | Bifrost tests and mutation tooling | Bifrost: no root `package.json`, `node:test` files under `tests/`, `grep stryker` returns nothing | Tests run with `node --test`. StrykerJS is not installed. How the tests find CultLib: C23 (the tests ignore `VOIDBOT_CULTLIB_ROOT`). |
| C19 | Idunn verification | `scripts/verify.sh`; eureka `tools/stopgap/ygg-verify.sh` (rust image carries cargo-mutants) | `cargo check` for the Windows GNU target twice, then `cargo test --locked --lib`. |
| C20 | Idunn source mentions no transport | `grep -rin "discord\|bifrost" src/` at `487f4f1` | No hits. This is the negative-grep baseline. |

### Bifrost suite probes (2026-09-30, 15:00-15:30 UTC)

Pinned heads: Bifrost `42728a67` (origin/main) and `hands/watchdog-bifrost-retire`
`4619939e`; CultLib pin `36ea08d3` (origin/main `016df462`); Epiphany origin/main
`4d1113ef`. Probes ran in a scratch clone, never in `F:\Projects\Bifrost`. Suite
runs went through `ygg-verify.sh` in `node:24.14.1-bookworm`, with CultLib cloned
to `/CultLib`, the sibling of `/src`, then `npm ci` and `npm run build:ts`. Each
file ran alone with the TAP reporter, then the whole glob ran once.

| # | What | Where / command | Result |
|---|---|---|---|
| C21 | Bare `cultcache-ts` require sites | Bifrost `git grep -nF '("cultcache-ts")'` at `42728a67` | `tests/persona-feedback-cli.test.mjs:15,55,62`, `tests/persona-discord-delivery.test.mjs:33`, `tests/persona-discord-crossing-rust-smoke.test.mjs:13`, `tools/persona-feedback.mjs:114`, `tools/agent-transport.mjs:40`, `tools/governance-threads.mjs:36`, and `tools/operator-notification.mjs:186`, which cut `bifrost-retire-alarm` deletes. `persona-feedback.mjs` calls `loadRuntime()` at module top (`:23`), so every verb fails at import. The bare `cultnet-ts` and `cultmesh-ts` sites resolve. Tools that load `dist/index.js` by path do not depend on the name: `bifrost-crossing-documents.mjs:271`, `bifrost-repository-release-authority.mjs:161`, `cultmesh-bridge-commands.mjs:313-314` and `provider-advertisement.mjs:47-119`. |
| C22 | What CultLib publishes | `git show <rev>:packages/{cultcache,cultnet,cultmesh}-ts/package.json` at `36ea08d3` and `016df462`; `git log -S'"@gamecult/cultcache-ts"'` | Only `cultcache-ts` is scoped: `@gamecult/cultcache-ts`, renamed in `8cb3b728` (2026-09-04). `cultnet-ts` and `cultmesh-ts` are still bare and depend on `@gamecult/cultcache-ts ^0.14.0`. The commit message says they "move with it", but only their dependency moved. The directory `packages/cultcache-ts` keeps its name, and every package declares `exports`. A local resolve at `016df462`, anchored at `packages/cultcache-ts/package.json`, gives `@gamecult/cultcache-ts` -> `dist/index.js` and `cultcache-ts` -> `MODULE_NOT_FOUND`. |
| C23 | How Bifrost finds CultLib | C21 sites; `ls .github/workflows` | Tools use `VOIDBOT_CULTLIB_ROOT`, falling back to the sibling `../CultLib`. **Tests hard-code the sibling `../CultLib` and ignore the variable.** Bifrost has no `package.json` and no CultLib dependency. No CI runs the node tests: the only workflow is `publish-container.yml`. |
| C24 | **Production Persona crossing is not broken by the rename** | `ssh ygg`, read-only: `systemctl cat bifrost-persona-mouth bifrost-persona-feedback`; `cat .../runtime/current/release-manifest.txt`; `createRequire(<release>/CultLib/packages/cultcache-ts/package.json).resolve("cultcache-ts")` | Both units run release `cb3239a…-f67f5122…`. The manifest says `cultlib_commit=f67f5122`, node `v24.14.1`. `VOIDBOT_CULTLIB_ROOT` points at the release's own CultLib. There the package is named `cultcache-ts`, `node_modules` holds real `cultcache-ts`, `cultmesh-ts` and `cultnet-ts` directories, and the bare name resolves to the release's `dist/index.js`. `f67f5122` is an ancestor of `8cb3b728`, so it predates the rename. A restart reloads the same pinned bundle. Since the C21 fix, Bifrost needs CultLib `8cb3b728` or later. A release that pairs it with an older CultLib fails at import (follow_up `stale-bifrost-persona-manifest` owns that path). |
| C25 | The Rust fixture tests have no producer | `tests/persona-discord-crossing-rust-smoke.test.mjs:17`, `tests/persona-discord-rudp-cross-language.test.mjs:21`; Epiphany `git log -S persona-discord-crossing-fixture`, `git grep` at `4d1113ef` | Both tests `cargo run --bin epiphany-persona-discord-{crossing,rudp-client}-fixture` in the sibling `../Epiphany`, with `CARGO_TARGET_DIR` hard-coded to `C:\\Users\\Meta\\.cargo-target-codex`. Epiphany deleted both bins in `387afe49` (2026-08-23), and neither exists at `4d1113ef`. So since 2026-08-23 these tests fail on every host, with cargo or without. `persona_discord_crossing.rs` survives as a library module. |
| C26 | Suite at base | ygg-verify Bifrost `42728a67`, CultLib `36ea08d3` at `/CultLib` | Per file (tests/pass/fail/skip): rust-smoke 1/0/1/0 (`spawn cargo ENOENT`); delivery 9/3/6/0 (`MODULE_NOT_FOUND 'cultcache-ts'`); permit 2/2/0/0; rudp 1/0/1/0 (`spawn cargo ENOENT`); feedback-cli 1/0/1/0 (the file fails to load, so its 8 tests are counted as 1); idunn-health 3/3/0/0. Glob total 17/8/9, exit 1, the same as Hands h1. |
| C27 | Suite with the fix | scratch commit `a6770c5` on `42728a67`: the scoped name at the seven live C21 sites, a skip guard on both Rust tests, and the `CARGO_TARGET_DIR` override removed. Same command. | rust-smoke 1/0/0/1 (`# SKIP`); delivery 9/9/0/0; permit 2/2/0/0; rudp 1/0/0/1 (`# SKIP`); feedback-cli 8/8/0/0; idunn-health 3/3/0/0. Glob total 24 tests, 22 pass, 0 fail, 2 skipped, exit 0. `agent-transport.mjs` and `governance-threads.mjs` were checked only by name resolution (C22), because no test loads them. |
| C28 | C3 at the reader's install pin | `git show 36ea08d3:packages/cultcache-ts/src/single-file-messagepack-backing-store.ts` | `pullAll` is at `:43`, and the file has no lock. C3 holds at `36ea08d3`, which is the CultLib that cut `ops-notice-deploy` installs. |

## Model page

A row per persistent kind this campaign creates or depends on. "New" means the
kind does not exist yet. Names in `code` for new kinds are working names; the
cut spec fixes them.

| Kind | Identity | Lifecycle | Authority |
|---|---|---|---|
| **Incident record** (new, `idunn.operator_incident.v1`, its own file `/var/lib/gamecult/idunn-projection/incidents.cc`, published 0644 beside `topology.cc`) | Key `<condition>:<subject>:<opened_at_unix_ms>`. Namespace: `incidents.cc`, one type; the file refuses any other. It is never written to `control.cc`, which bails on unknown types, so a rollback would stop continuity (C1). `condition` is a closed enum (`continuity-exhausted` first; `odin-store-contested`, `odin-store-unpersisted` and `operator-required` join when their sources exist). `subject` is the target for target conditions and the `transaction_id` for `operator-required`. `opened_at` is written once by Idunn's clock. Injective because of one rule: **at most one open incident per `(condition, subject)`**, checked by a compare-exchange create. | **Created** at the site that decides the condition, after that decision and before its trace line. **Closed** by writing `closed_at` and `close_reason` on the same record, never by deleting it. Continuity-exhausted closes when the pass sees the workload running (`recovered`) or the target is no longer admitted, **not** when the window slides (I2). Odin conditions close when the catalog stops reporting them. `operator-required` has **no closing path yet** (I6; follow_up `b5-operator-required-no-exit`). **Reopened**: never; a recurrence is a new record with a new `opened_at`. **Replayed after an Idunn restart**: the durable open record makes the site write nothing. **Pruned**: closed more than 7 days ago, it retires to `history.cc`. **Store failure**: reported once as a fault and never returned into the tick (`survival-independent`). | Idunn's daemon, at the decision site, is the only writer. The same file is the record and what Bifrost reads, so no separate projection exists to drift. Forbidden: the CLI (`status` reads only), Bifrost and every other reader, Odin, `ReportOnce`, `control.cc`, a repair loop that opens incidents from log text. |
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

**The incident store is its own file, and that file is the projection.** The
model page placed the record in `control.cc` with a separate projection
derived from it. C1 overturns that: an unknown type in `control.cc` makes every
older Idunn binary refuse the whole store, so rolling back the incident cut
would stop continuity for every target. The record therefore lives in its own
CultCache file, `/var/lib/gamecult/idunn-projection/incidents.cc`, written by
Idunn with compare-exchange and published `0644` like `topology.cc` (C6). An
incident carries nothing sensitive (`no-sensitive-egress`), so there is no
private half to keep apart, and the derived copy disappears. What is lost: the
open write is not atomic with any `control.cc` write. That costs nothing
today, because the exhaustion decision writes nothing to `control.cc` (I2),
and later sources are read-only observations. The model page's
"Incident record" and "Incident projection" rows collapse into this one file.
Self owns the edit to the model page.

**A broken incident store never reaches the tick.** Every incident operation
returns its error to one call site, which traces it once with `report_once`
and moves on. An unreadable store means no incident opens (the stderr trace
still prints); it never changes what continuity decides. The Idunn cut's
survival test pins this with an unwritable store path.

**Bifrost reads without the lock.** Idunn's writes block on an exclusive lock
(C2). A reader that took the lock could stall a tick. cultcache-ts `pullAll`
takes none (C3, C4), and Idunn's rename is atomic, so the reader only ever sees
a whole snapshot. The reader never opens Idunn's file through a CultMesh node,
which could flush.

**Delivery at most once, with the nonce as the second guard.** This follows
the Persona precedent (C16): a journal entry found `running` at start becomes
`unknown` and is never re-posted. A bridge exit that failed is retried on a
later run, with the same nonce and `enforce_nonce` (C17). A retry inside
Discord's "past few minutes" returns the original message instead of creating
another. The remaining duplicate needs the bridge to have posted, then exited
non-zero, and the retry to fall outside that window. The attempt cap and
timer period bound it, and it is recorded as a residual risk rather than
engineered away.

**Who sees a delivery failure: Bifrost, through systemd.** There is one
channel to the operator, and it is the one that failed, so a failure cannot
page over it. The reader exits non-zero while any journal entry is `unknown`
or has exhausted its attempts. The unit then shows in `systemctl --failed` and
in its journal, and the reader's `status` verb lists the entries. Idunn reads
none of this (`survival-independent`). This is a default: the alternatives
either couple Idunn to delivery state or need a second channel that does not
exist.

**Deployment of the reader has no Idunn path.** Idunn deploys resident
services, and a timer does not fit its recipe model (C9). No Bifrost binding
exists in current Idunn (C10), and the old Bifrost manifest belongs to the
Idunn that was rebuilt away (C11). The ruled one-shot reader can be
hand-installed by a gamecult-ops script and runbook, the same interim footing
as `huginn.service` and `bifrost-persona-mouth`. Otherwise Idunn must first
learn a new workload kind. That choice is an operator question raised in the
ops cut.

**The reader gets its own release root.** It installs under
`/srv/bifrost/watchdog-notice/releases/<bifrost>-<cultlib>` and does not
repoint `/srv/bifrost/persona-feedback/runtime/current`. Repointing that link
would stage new code under `bifrost-persona-mouth`. With the token file now
present (Y3, Y6), the mouth's next restart would also make the Persona crossing
able to post. That crossing is out of scope and must not change as a side
effect.

**Cut order.** The Idunn incident cut comes first. It writes the golden
fixture that Bifrost's reader is tested against (C5). The Bifrost subtraction
cut is independent and lands before the reader, so the reader is written
against a tree with one Discord command vocabulary. The gamecult-ops
subtraction is independent too. The ops deploy cut comes last and needs both
behaviour cuts merged.

**The Bifrost suite follows the owner's name, with no fallback.** CultLib
renamed `cultcache-ts` to `@gamecult/cultcache-ts` so that the org owns the
name on a public registry (C22). The consumer changes the string at each live
require site (C21) and keeps the directory anchor, which did not move. A
try-both-names loader was rejected. It would be a local shim that keeps the
old name alive in Bifrost after its owner retired it. It would also let a
release silently pair new Bifrost with a pre-rename CultLib. After the change
that pairing fails loudly at import (C24). Production does not move: its
bundle pins CultLib `f67f5122` (C24). `cultnet-ts` and `cultmesh-ts` stay bare
because that is what CultLib publishes. Whether they should follow the scope
is CultLib's decision, not Bifrost's.

**The Rust fixture tests skip with a reason; they are not deleted.** Their
producers were deleted from Epiphany on 2026-08-23 (C25), so installing cargo
would not make them pass. Each one skips unless cargo and its named fixture
source are present, and the skip reason names the Epiphany commit that removed
the fixture. Deleting them would throw away the only Rust-authored byte seam
test the Persona crossing has. Restoring a producer belongs to Epiphany and the
Persona crossing, both outside this campaign (follow_up
`bifrost-rust-fixture-producer`). The hard-coded Windows `CARGO_TARGET_DIR`
goes, because it is wrong on every host except one workstation.

**`bifrost-suite` does not depend on `bifrost-retire-alarm`.** It leaves
`operator-notification.mjs` alone, because that cut deletes the file, so the
two branches merge in either order without conflict. The reader depends on
both. It is written against a tree with one Discord command vocabulary, and it
is verified by a suite that loads.
