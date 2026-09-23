# Idunn verify transaction: cut map

Status: cut map, Imagination pass 1 (Opus), 2026-09-22. Nothing has landed.
There is no separate target document yet. Until Self writes one, the ends are
the operator ruling below plus section 2 of this map. This document owns the
means.

Ruling (operator, 2026-09-22, "go for it"): **Idunn owns verification.** A verify
transaction takes a repository, an exact revision and a declared verify recipe.
It materializes the revision in a capped Docker runner on the declared host,
runs test and mutation steps, and returns a typed verdict. It seals no artifact
and installs nothing, so it sits outside the deployment brake.

Open: operator questions Q-V1 to Q-V10 (section 4). No cut from Cut 2 onward may
be briefed to Hands until Q-V1, Q-V2, Q-V4, Q-V7 and Q-V8 are ruled. Cuts 0 and 1
depend on no ruling.

Heads read for this map:

| Repo | HEAD | Notes |
|---|---|---|
| `F:\Projects\Idunn` | `5b3f646` (= origin/main) | clean |
| `F:\Projects\gamecult-ops` | `6a5c16c` | |
| `F:\Projects\CultLib` | `d0480a7` (main) | `scripts/mutate-dotnet.ps1` exists only on `cultnet/selection-cut1` (pushed, `0714492`) |
| `GameCult/Eureka` (`~/.claude/skills/eureka`) | `266faf5` | `tools/eureka-mutations.ps1` has an uncommitted 8+/2- edit in the working tree |
| Yggdrasil `/usr/local/bin/idunn` | unknown revision | installed 2026-09-11 12:44; its usage text shows the five-command CLI. It predates `5b3f646` (2026-09-13) |

---

## 1. Body facts

Each fact below was read at the heads above. "Probe" means it was run on
Yggdrasil in a container capped at `--cpus=4 --memory=8g` and niced, and then
cleaned up.

### 1.1 Recipes, bindings, steps

- F1. `RecipeStep` is at `src/deployment.rs:53-64`: `id`, `phase`, `runner`,
  `argv`, `working_directory` (default `.`), and `required_environment`.
  `RecipePhase` is `Prepare|Test|Build|Acceptance` at `:70-77`. The Eyes pass
  gave both ranges one line early.
- F2. **Nothing reads `RecipePhase`.** It is parsed and never consumed. A grep
  of `src/` finds no use outside the enum. `DockerRunnerDriver::materialize`
  runs every step in file order (`src/drivers.rs:1662-1684`), and Ghostlight's
  recipe interleaves `test`, `build`, `prepare` and `acceptance` steps in file
  order. No existing mechanism selects steps by phase, so "run the
  Test/Acceptance-phase steps" has nothing to build on.
- F3. `TargetDeclaration` requires `artifacts` and `service`
  (`deployment.rs:32-33`, and `:762` rejects a declaration with no artifacts).
  `service` must require `GAMECULT_IDUNN_RUNTIME_BUNDLE` (`:864-869`). A repo
  with no daemon, such as CultLib, Huginn or Eureka, cannot state a deploy
  recipe. `OperatorBinding` also requires `workload`, `runtime_identity`,
  `brakes`, `rollout` and `placement` (`:266-283`). **Verify cannot reuse
  either document.**
- F4. The program allowlist is **per runner and on the binding side**:
  `DockerRunnerBinding.allowed_programs` (`deployment.rs:429`), validated as
  bare executable names (`:1126-1132`, `require_program` `:1728-1735`). It is
  checked at admission (`admit`, `:1447-1453`) and again at run time
  (`drivers.rs:1416-1420`). It is not a global list. The recipe writes the argv
  and the binding admits `argv[0]`.
- F5. Docker caps come from the binding: `memory_mebibytes` and
  `cpu_quota_percent` (`deployment.rs:436-437`), lowered to `--memory` and
  `--cpus` (`drivers.rs:1468-1476`). The same call also sets `--network`
  (binding `network_profile`, default `none`), `--cap-drop ALL`,
  `no-new-privileges`, `--read-only`, `--pids-limit`, a `noexec` `/tmp` tmpfs,
  and a per-workspace `/etc/machine-id` (`drivers.rs:1456-1499`). **There is no
  seccomp setting and no timeout.**
- F6. The only environment a step receives is the source stamp plus the names
  it lists in `required_environment`. Binding environment is never ambient
  (`drivers.rs:1429-1450`).
- F7. `docker()` calls `Command::output()` (`drivers.rs:1392-1396`). It has
  **no deadline**. Output is buffered in memory, thrown away on success, and on
  failure appears only inside the `bail!` message.
- F8. Each runner gets a scratch **copy** of the frozen tree
  (`drivers.rs:1631-1636`, `copy_tree`), owned by the container user
  (`assign_runner_tree`, `:1649-1654`). Every step of that runner runs in that
  workspace. Mutating the source in place therefore touches only the scratch
  copy.
- F9. External inputs (`https` URL plus `sha256`) are fetched by `curl` inside
  the runner image and checked against their digest (`drivers.rs:1501-1559`).
  This is the existing way to bring a pinned harness from outside the tree.
- F10. Existing binding practice (`gamecult-ops/idunn/yggdrasil/bindings/ghostlight.toml.in`):
  runners use `network_profile = "bridge"` for registry access plus a
  `cache_root`. Ghostlight's `rust-acceptance` runner uses
  `network_profile = "host"` and `secret_files`, so that it can reach the live
  connector at `127.0.0.1:4103`.

### 1.2 Source

- F11. The deploy source path fetches the binding's `admitted_ref` and requires
  the selected revision to be an ancestor of it and a descendant of
  `minimum_revision` (`drivers.rs:1157-1219`, `:810-852`). `freeze` fetches the
  exact revision by SHA, runs `git archive | tar` into a root-owned tree, and
  hashes it (`:1221-1304`). Git runs with `env_clear`, `GIT_CONFIG_NOSYSTEM=1`,
  and as the unprivileged `idunn` uid (`:705-726`). These primitives are typed
  against `OperatorBinding` and `CompiledDeploymentPlan`.
- F12. CultLib `.gitattributes` has `* text=auto` and no `export-ignore`,
  `export-subst`, `eol=crlf` or `filter=`, so an archive on Linux is the stored
  blob. The Eureka scar about `git archive` concerns byte comparison against a
  Windows worktree, and it does not bite here. The frozen tree is compared only
  with itself.

### 1.3 Control plane

- F13. The CLI has **five** commands: `serve`, `up`, `status`, `cancel`,
  `validate` (`control_plane.rs:1479-1500`, `parse` `:1541-1553`, `usage`
  `:6489-6499`). The README (`README.md:33-40`) says three, which is stale.
  `cli_exposes_only_declarative_commands` is at `:7577-7586`.
- F14. `idunn up` writes a `DeploymentCommand` straight into the local
  `control.cc` by compare-exchange (`submit`, `:2434-2502`). **There is no
  remote request surface.** An agent on Starfire reaches it only through
  `ssh ygg sudo …`. `control.cc` on Yggdrasil is `root:root 0640`.
- F15. **The scheduler is one thread.** `serve` loops over `run_scheduler_tick`
  (`:3004-3019`, `:3048-3055`). Sealing calls
  `self.runner_for(plan)?.materialize(...)` synchronously (`:3847-3852`), so
  every Docker build blocks continuity supervision for its whole duration.
  `gamecult-ops/runbooks/idunn-host-raven.md` says so: "the Idunn tick blocks
  on it the way it blocks on a docker build." A 30-to-60-minute mutation
  harness run through this path would suspend daemon survival for every target.
- F16. **The deployment brake is consulted after the build, not before it.** In
  `advance_sealing`, resolve, freeze, `materialize` (which runs every recipe
  step) and `install` all happen at `:3786-3866`. The brake is checked only when
  moving to `Starting` (`:3868-3897`, "The candidate's Expected is published in
  Starting, after the brake"). So the Eyes claim that every build or test sits
  inside a transaction that needs the brake is true only in the sense that
  builds happen inside Deploy transactions. **Builds and tests already run
  without brake authorization.** The brake gates the first Verse-visible change.
- F17. The host-actuator hub (`--host-actuator-bind 10.77.0.1:17890`) is a
  signed CultNet RUDP channel. Managed hosts dial in, and Idunn sends them
  requests such as `Materialize` (`host_actuator.rs:91`,
  `HostActuatorRunnerDriver` `:697-736`). It carries no client requests.
- F18. Terminal deploy transactions move to `history.cc`
  (`archive_terminal_transaction`, `control_plane.rs:2193`). On Yggdrasil,
  `history.cc` is 10 MB. `/var/lib/gamecult/idunn/staging` holds 382 entries
  and 4.1 GiB, and nothing prunes it. Verify must own its own cleanup and must
  not add to this.

### 1.4 Doctrine and docs

- F19. `README.md:3-10`: "Idunn decides two things and nothing else." Verify is
  a third charter item, and the README must say so.
- F20. **`docs/deployment-authority.md:61-78` "Current implementation boundary"
  is stale.** It says `deployment.rs` and `deployment_plan.rs` "do not yet drive
  Git, runners, systemd, nginx, brakes, CultCache, CultMesh". `drivers.rs` has
  done all of that for weeks. Its second paragraph, that private plan state is
  never published and only `ExpectedIncarnation` is Verse-shaped, is still true
  and belongs under "Typed deployment state".
- F21. **`docs/authority-map.md:203-213` "Current implementation boundary" is
  also stale.** It names the Odin-era path `crates/idunn-daemon/src/` and says
  "The transaction engine still must persist and sequence …", which it now does.
- F22. `README.md:204` says `docs/guide.md` describes the generation "still what
  runs on yggdrasil". That is false: probe of `systemctl show idunn-yggdrasil`
  shows the new generation's `idunn serve` running.
- F23. `gamecult-ops/scripts/request-idunn-bounded-redeploy-yggdrasil.sh` calls
  `idunn redeploy --daemon …`. The installed binary has no `redeploy` command
  (probe of its usage text), so the script is dead and is **not** the current
  pattern. The current pattern is `idunn up` followed by
  `idunn-provision deployment-brake-release`
  (`runbooks/idunn-host-raven.md`, "Deploy Muninn to Raven").
- F24. Build placement (`gamecult-ops/inventory.md:559-566`): build on or for the
  target's host. Yggdrasil has 16 vCPU and 62 GiB and carries live services, so
  state the core count and duration before a large build. Probe: load average
  0.38, 52 GiB available, `/` at 15% of 2 TB, `/srv/build` 53 GiB.

### 1.5 Harnesses and existing verification

- F25. The Eureka stopgap is already live. It is
  `~/.claude/skills/eureka/tools/stopgap/ygg-verify.sh` (91 lines) plus
  `rust.Dockerfile` (9 lines). It pushes to a bare mirror
  `~/eureka-verify/repos/<name>.git`, runs **one free `bash -c` string** as root
  in a container capped at `--cpus 6 --memory 16g`, niced, with 2 flock slots,
  and uses the shared volumes `eureka-cargo-registry` and `eureka-nuget`. On
  Yggdrasil it has left `~/eureka-verify/{harness,repos/Huginn.git,work,slot-1.lock}`
  and the image `eureka-verify-rust`, whose `FROM` is a tag and not a digest. It
  is named in `SKILL.md:417-424` and `references/changelog.md:5-35`. Note that
  its caps are 6 CPU and 16 GiB, not the brief's 4 CPU and 8 GiB.
- F26. There is a second, older verification authority:
  `gamecult-ops/scripts/deploy-epiphany-yggdrasil.sh:171-208`. It writes text
  "test receipts" for the legacy generation and greps them. CultLib's GitHub
  Actions are a third: `.github/workflows/cultnet-interop.yml` runs on
  `windows-latest`, and `cultmesh-portability.yml` runs an OS matrix.
- F27. The three harnesses have three exit conventions and **none writes a
  typed summary**:
  - `eureka-mutations.ps1` throws when a mutant is not killed (`:542-546`).
  - `mutate-dotnet.ps1` (on the branch only) exits non-zero.
  - `mutate-cultmesh.mjs` exits 2 when entries were skipped.
- F28. The linux-x64 native QUIC mutation needs
  `--security-opt seccomp=unconfined`, because TSan re-executes under
  `setarch -R`. It also needs the image built from the digest-pinned
  `scripts/quic-native-linux-dev.Dockerfile`, and it runs as a two-command
  `bash -lc` string (`scripts/quic-native-linux-dev.Dockerfile:1-25`,
  `scripts/mutate-cultmesh.mjs:40-60`).
- F29. The Eureka harness is published at `github.com/GameCult/Eureka`
  (`tools/eureka-mutations.ps1`). It is not in any target repo.

### 1.6 Probes (Yggdrasil, capped, cleaned up)

- P1. The Idunn baseline at `5b3f646` in `rust:1.95-bookworm`, using the
  README's method (`/etc/machine-id` mounted), capped at 4 CPU and 8 GiB:
  `cargo test --lib` gives **126 passed, 0 failed, 2 ignored**. The ignored
  tests are `live_committing_record_is_unstuck_by_the_replacement_gate` and
  `live_store_inventory`. The test profile compiled in 31.9 s with a warm
  registry.
- P2. Idunn-shaped hardening (all of F5, user `65532:65532`, network `none`):
  - pwsh 7.5.3 **crashes** at start with no writable `HOME` (a stack in
    `GetPolicySettingFromConfigFile`).
  - With `HOME=/workspace/.home` it works, and an in-place file write under
    `/workspace` succeeds.
  - `dotnet` 10.0.400 runs, and `dotnet new console` restores.
  - node 24.14.1 runs.
- P3. A **locally built** image can be pinned. `docker image inspect` reports
  `RepoDigests` equal to the image id under this Docker's image store, and
  `docker run eureka-verify-rust@sha256:<id>` runs. So `require_pinned_image`
  (`deployment.rs:1841-1847`) admits images built on Yggdrasil without a
  registry.

---

## 2. Doctrine placement and the campaign authority map

**What verify is.** A verify transaction changes no artifact, revision,
configuration, schema, unit or authority binding of any managed target. It
installs nothing, publishes nothing to the Verse, and restarts nothing. It is
therefore **neither deployment nor continuity**, and neither brake governs it.
The doctrine says a target brake may gate only Idunn's mutation of that target,
and verify mutates no target, so **no brake may gate verify**. F16 shows that
the Body already treats build and test as unbraked: the deployment brake gates
the Verse-visible change, not the build.

**What governs it instead.** Verify spends shared host resources beside live
services, so its governor is a **resource authority**: the typed host verify
policy (Cut 2). That policy holds concurrency, per-runner ceilings, aggregate
budget, and a suspend switch (`max_concurrent = 0`). This is not a brake. It
names no target and cannot touch deployment or continuity.

**The survival invariant extends.** Idunn must be able to start, recover,
observe and report while verify is absent, broken, overloaded or wedged. F15
makes that a structural requirement: verify must never execute on the
scheduler thread. The recommended form (Q-V8) is a separate process with its
own stores, which the survival process never opens.

**Verdicts are evidence, not admission.** No Idunn decision reads a verdict.
Deployment does not consult verify; that coupling is out of scope.

### Authority map (campaign level)

- **Owner:** the Idunn verify worker (`idunn verify-serve`; see Q-V8). It alone
  decides what ran, in what runner, under what limits, and what the verdict is.
- **Inputs:**
  - verify request and cancel records from the request store;
  - verify bindings (operator);
  - the host verify policy (operator);
  - exact Git objects fetched by SHA from the binding's origin;
  - the verify recipe blob at that revision;
  - step exit status, deadline expiry, and mutation report files.
- **Outputs:** verify transactions and verdicts in the verdict store, step logs
  under the log root, and the CLI's status, log and wait output.
- **Derived state:**
  - mutation counts are derived by Idunn from report entries, never supplied by
    the harness;
  - the `--wait` exit code is derived from the verdict;
  - CLI status text is display only;
  - logs are evidence, never read back to decide anything.
- **Forbidden writers:**
  - Eureka agents running builds, tests or harnesses on Starfire (apart from
    the Windows exception, Q-V5);
  - ad hoc containers on Yggdrasil: the stopgap's `docker run`, and any
    hand-launched test container;
  - request-supplied argv, images, caps, network, environment or secrets;
  - recipe-supplied images, network, caps, secrets or seccomp;
  - the deploy scheduler, which must never read or write verify stores;
  - the verify worker writing `control.cc`, the topology, brake stores,
    bindings, release roots or deploy source checkouts.
- **Shared paths:**
  - one container-spec lowering and one step-execution primitive, used by both
    deploy materialize and verify (Cut 1);
  - one exact-revision freeze primitive (Cut 1);
  - CLI submission, cancel and restart recovery all go through the same
    request, transaction and cancel record types.
- **Deletion line:** Cut 7 deletes the stopgap (`tools/stopgap/`, the SKILL.md
  text, and `~/eureka-verify` plus its volumes and image on Yggdrasil).

**Rejected path: verify as a Deploy transaction that stops before Starting.**
It looks smaller, but it fails three ways:
1. It needs a `TargetDeclaration` with a service (F3).
2. It runs on the blocking scheduler (F15).
3. It would put verify records into `control.cc`, which every survival decision
   reads.

---

## 3. Step 0b: identity, lifecycle and authority per persistent kind

| Kind | What names it | What happens to it over time | Who decides |
|---|---|---|---|
| **Verify recipe** `gamecult.idunn.verify_recipe.v1` | `(repo id, revision, recipe_path)`, content-addressed by `recipe_blob_sha256`. Step ids are unique within the recipe. | Immutable per revision. Read once at resolve. Its bytes are retained in the transaction, so the verdict names exactly what ran. Changing it means committing a new revision. | The repository authors capability: steps, argv, working dir, timeouts, required env names, report declaration, external inputs. It cannot name an image, network, caps, seccomp or secrets. |
| **Verify binding** `gamecult.idunn.verify_binding.v1` | `repo` field, which must equal the file stem of `/etc/gamecult/idunn/verify/bindings/<repo>.toml`. This namespace is **separate from deploy targets**: a different directory, schema and record types, so `odin` the verify repo and `odin` the deploy target never collide. | Edited by the operator from the gamecult-ops template. Read at each admission. Bytes and sha256 are snapshotted into the transaction, so a later edit never changes a running or finished transaction. | Operator (installed as root, from gamecult-ops). |
| **Host verify policy** `gamecult.idunn.verify_host_policy.v1` | Singleton per host at `/etc/gamecult/idunn/verify/host.toml`. Its `host` field must equal the worker's configured host name. | Re-read on each admission pass. Changes apply to new admissions only; running jobs keep the limits they were admitted under. `max_concurrent = 0` suspends admission. | Operator. |
| **Verify request** `idunn.verify_request` (v1), in the **request store** | `verify-<uuid v4>`, minted by the CLI. The same id becomes the transaction key. | Write-once. Consumed when the worker creates a transaction with the same id. Retired with that transaction to history. A request whose binding is missing is sealed `Rejected`, not left resident. | The requester (agent or operator) chooses the repo, a 40-hex revision, an optional step subset, and `requested_by`. Nothing privileged. |
| **Verify cancel** `idunn.verify_cancel` (v1), in the request store | Keyed by `verify_id`. | Write-once. The worker observes it: a queued transaction is sealed `Cancelled`, and a running one has its step container killed and is sealed `Cancelled`. The CLI refuses to cancel an unknown or terminal id. | Requester. |
| **Verify transaction / verdict** `idunn.verify_transaction` (v1), in the **verdict store** | `verify_id`. | Phases: `Admitted → Frozen → Running{step} → Terminal`. Terminal is one of `Passed`, `Failed`, `Error{kind}`, `TimedOut{step}`, `Cancelled`, `Interrupted`, `Rejected`. **Terminal is immutable.** It moves to the verify history store after `resident_terminal` newer terminals, and history is pruned to `history_limit`. **No resume:** a transaction that is not terminal when the worker starts is sealed `Interrupted`. | The verify worker is the only writer. |
| **Step outcome** (embedded in the transaction) | `(verify_id, step id)`. | Set once when the step ends: exit status, duration, deadline, log reference, optional decoded mutation report. Steps after a failed step are `NotRun`. | Worker. |
| **Step log** | `<log_root>/<verify_id>/<step>.log`. | Combined stdout and stderr, written while the step runs and capped at `log_byte_cap`. At step end its `sha256`, `bytes_kept`, `bytes_total` and `truncated` are recorded. Deleted when its transaction leaves history. | Worker. |
| **Mutation report** `gamecult.eureka.mutation_report.v1` (JSON at the xenos boundary; see Q-V4) | `(verify_id, step id)`, read from the step-declared path. | Read once after the step exits, decoded strictly, and embedded as a typed record. The source file dies with the workspace. | The harness produces entries. Idunn decodes them, derives the counts, and cross-checks them against the exit status. |
| **Frozen tree and runner workspaces** | `<workspace_root>/<verify_id>/{frozen,runner-<id>}`. | Created at `Frozen` and `Running`, deleted when the transaction goes terminal. At worker start, any directory whose id is terminal or unknown is deleted. | Worker. |
| **Step container** | name `idunn-verify-<verify_id>-<step>`, label `gamecult.idunn.verify=<verify_id>`. | Runs with `--rm`, and is killed on deadline, cancel or worker restart. Any container carrying the label with no live transaction is an orphan, and the worker kills it. | Worker. |
| **Verify source checkout** | `<verify_source_root>/<repo>`, a partial clone of the binding origin. **Separate from deploy checkouts** (F11), so fetching arbitrary branch SHAs never touches deploy refs. | A persistent cache that fetches by SHA. Recreated if its origin differs from the binding. | Worker, as the `idunn` uid. |
| **Runner images** | `name@sha256:<digest>` pinned in the verify binding (P3). | Built from a committed Dockerfile whose `FROM` is digest-pinned. Changed only by editing the binding. | Operator (Q-V10). |
| **Verify stores** | `/var/lib/gamecult/idunn-verify/requests.cc` (requests and cancels) and `/var/lib/gamecult/idunn-verify/verify.cc` plus `verify-history.cc` (transactions). | The request store is written by the CLI and read by the worker. The verdict store is written only by the worker and readable by the requester group. The deploy daemon never opens either. | Worker owns the verdict store; requesters own only request and cancel records (Q-V7). |

No cell is empty. The identity rule that the rest of the map depends on: **the
request id, transaction id, workspace directory, log directory and container
label are all one `verify_id`.** One id, so cleanup and orphan detection are
exact.

---

## 4. Operator questions

Each question gives its options, a recommendation, and what depends on it. The
answers gate Cut 2 onward.

**Q-V1: RULED A by the operator, 2026-09-22 ("A sounds good").** Recipes live in the repo at a binding-named path, read from the exact revision. Privilege stays in the operator verify binding.

*History, the question as asked:* **Q-V1. Where do verify recipes live?**
- A: in the repository, at a binding-named path (suggested
  `deployment/idunn/verify.toml`, beside the deploy recipe), read from the exact
  revision being verified. Privilege lives in a separate operator verify binding
  (gamecult-ops template, installed on the host). This is the same split as
  deploy.
- B: in gamecult-ops bindings only. The operator authors every step.
- C: in the request.

**Recommended: A.** A Hands pass can then add a mutation-suite step in the same
commit as the rule it pins, and the suite is committed, rerunnable and named in
the verdict. This is the skill's "a probe that is the only thing defending a
rule must be committed" rule, enforced by the substrate. B turns every new
suite into an ops commit and decouples steps from the code they test. C is the
escape hatch the README forbids. A branch can weaken its own recipe, but the
verdict records the recipe blob and each step's argv, so Soul sees exactly what
ran.

Depends: the Cut 2 schema, and the per-repo recipes in Cut 6.

**Q-V2: RULED A by the operator, 2026-09-22 ("agreed").** A request carries an exact 40-hex commit that can be fetched from the binding's origin, plus an optional subset of step ids, and nothing else.

*History, the question as asked:* **Q-V2. What does a request carry?**
- A: an exact 40-hex commit that is fetchable from the binding's origin (any
  branch, pushed), plus an optional subset of step ids. Nothing else.
- B: A plus unpushed commits, pushed by the agent into an Idunn-trusted mirror
  (the stopgap's model).
- C: a branch name resolved at request time.

**Recommended: A.** Eureka already requires every Hands commit to be pushed, and
`cultnet/selection-cut1` is on origin. B creates a second source authority, an
agent-writable repository that Idunn would trust. C makes a verdict unreplayable.

In-place mutation fits: each runner works on a scratch copy (F8), and Idunn
additionally re-hashes the frozen files after every report-bearing step (Cut 4,
R-V14).

Depends: Cut 3 request validation, and the Cut 4 fetch.

**Q-V3: RULED A by the operator, 2026-09-22 ("A is fine").** Each runner has an allowlist on the binding side, and a request names only step ids. The command line comes from the recipe at the verified revision and is admitted against the binding. The confinement is the container and the binding.

*History, the question as asked:* **Q-V3. How does the allowlist admit `pwsh`, `node` and `bash` without
becoming an escape hatch?**
- A: keep the allowlist per runner on the binding side (F4). A request names
  only step ids. Argv is authored in the recipe at the verified revision and
  admitted against the binding. The binding may list `pwsh`, `node`, `bash`,
  `cargo`, `dotnet`, `cmake` or `npm` as the operator sees fit.
- B: A, but forbid shell interpreters in verify allowlists.
- C: pin each step's argv in the binding.

**Recommended: A.** The escape hatch the README forbids is **command text
supplied by the requester**, and A makes that unrepresentable. A program name
confines nothing: `cargo test` already runs arbitrary `build.rs` code. The
confinement is the container and the binding, which for verify means no
secrets, no host network, capped resources, and read-only root (Cut 2, R-V1 and
R-V2).

B buys nothing, because `pwsh -Command` and `node -e` are shells. The QUIC
native job needs no shell string: it becomes two steps, `bash
scripts/build-quic-native.sh` and `node scripts/mutate-cultmesh.mjs native`. C
duplicates the recipe.

Depends: the Cut 2 admit rules.

**Q-V4: RULED B, via `Add-Type`, by the operator, 2026-09-22 ("yep").**

The harness writes its mutation report as CultCache itself. PowerShell 7 is .NET, so the harness loads `GameCult.Caching.dll`, CultLib's C# reference runtime at a pinned published version, with `Add-Type -Path`. It compiles the report document type inline with `Add-Type -TypeDefinition` and writes a `.cc` report to the step's declared path. Idunn decodes the report into its own Rust type, which is canonical, and cross-checks it against the exit status. A report that contradicts the exit status becomes `Error{ReportContradictsExit}`.

Why B over A, C or D:
- **A put load-bearing JSON between two GameCult components.** The operator's doctrine rules that out.
- **A and B's original premise was wrong.** It assumed PowerShell had no CultCache runtime, and Self's option D (an `idunn verify-report` subcommand) rested on the same assumption. The operator pointed out that PowerShell compiles and loads C#.
- **B gives one report path for every run on every host.** That includes the win32 QUIC runs that stay on Starfire outside Idunn (Q-V5). Under D those runs had no `idunn` binary and would have fallen back to text.

The cost is two declarations of the report type, one in C# and one in Rust. CultCache schema identity makes drift a loud decode refusal rather than a silent misread. Cut 5 changes accordingly: the harness side lands in Eureka and CultLib, and Idunn's side is the decoder plus the cross-check. Idunn parses no JSON and no stdout.

*History, the question as asked:* **Q-V4. What shape is the verdict, and how does mutation data reach it?**

The verdict records, for each step: exit status, duration, admitted deadline,
and a log reference (path, sha256, kept and total bytes, truncated flag), never
inline log text. For a mutation step it also records a typed report: control
`Green|Red`, and entries `{id, verdict: Killed|Survived|NoVerdict|Skipped}`,
with the counts derived by Idunn.

How the report gets in:
- A: the step declares `report = { schema = "gamecult.eureka.mutation_report.v1", path = "…" }`.
  The harness writes a strict JSON file there, the xenos boundary, because
  PowerShell has no CultCache runtime. Idunn decodes it with
  `deny_unknown_fields` into a typed record and persists it as CultCache.
- B: harnesses write CultCache `.cc` directly. This needs a CultCache runtime in
  PowerShell, which does not exist.
- C: Idunn parses harness stdout.

**Recommended: A.** C makes Idunn a text parser for three formats (F27). With A,
Idunn also cross-checks the report against the exit status. Exit 0 with a
survivor, or exit non-zero with a clean report, becomes `Error{ReportContradictsExit}`.
The harness's honesty is observed, not trusted.

Depends: Cut 5 in Idunn, Eureka and CultLib.

**Q-V5: RULED A by the operator, 2026-09-22 ("A indeed").** This campaign has no Windows verify host. Win32 jobs stay on Starfire, run by hand, one at a time, with no burners. They still write the typed CultCache report (Q-V4). Cut 9 stays a placeholder until a dedicated Windows host exists.

*History, the question as asked:* **Q-V5. Is there a Windows verify host?** The QUIC win32 harness has to run on
Windows.
- A: none in this campaign. Win32 jobs stay on Starfire, one at a time and never
  with burners (today's SKILL.md rule), outside Idunn.
- B: Raven, through the host actuator, with a new `VerifyStep` request. Raven is
  human-used and streams, and `idunn-host.exe` is itself built on Starfire
  (runbook).
- C: a dedicated Windows box or VM. Which one?
- D: GitHub Actions `windows-latest`, which CultLib already uses (F26). Its
  results are outside Idunn and untyped.

**Recommended: A for this campaign.** Ask again once a dedicated host exists.
The map carries Cut 9 as a placeholder that only B or C activates.

Depends: Cut 9, and whether Cut 7's SKILL.md text keeps the Starfire exception.

**Q-V6: RULED by the operator, 2026-09-22 ("That's fine"), on measured data.** `max_concurrent = 2`. Each runner is capped at 400% CPU and **12288 MiB**. The aggregate budget is 800% CPU and **24576 MiB**, so live services keep half of Yggdrasil's CPU. Measurement: Ghostlight `cargo test --workspace --no-run -j 4` peaks at 5.7 GiB in 78 s, with a 3.4 GB target. Epiphany is not yet measurable, because its pinned Ghostlight rev has a submodule pointing at the deleted `GameCult/cultcache-py`. **Epiphany was measured later the same day**, after its Ghostlight pin moved past the dead submodule: 4.36 GiB, 92 s, 3.3 GiB target. The 12 GiB ceiling holds with about 2x headroom on both workspaces.

*History, the question as asked:* **Q-V6. Concurrency and caps on Yggdrasil.** The host policy is a typed setting.
- A: `max_concurrent = 2`, per-runner ceiling 400% CPU and 8192 MiB, aggregate
  budget 800% and 16384 MiB. At most 8 of 16 vCPU go to verification.
- B: 2 jobs at 6 CPU and 16 GiB each, which is the stopgap's setting (F25): 12
  vCPU.
- C: 1 job at 8 CPU and 16 GiB.

**Recommended: A.** It leaves half the host to live services, it matches the
cap in the operator's brief, and P1 shows Idunn's own suite compiles in about
32 s at 4 CPU.

Depends: the Cut 6 host policy values only. The mechanism is in Cut 2 and Cut 4
whatever the answer.

**Q-V7: RULED B by the operator, 2026-09-22 ("B sounds good to me too").** A dedicated `eureka` account on Yggdrasil holds an SSH key and is a member of group `idunn-verify`. It has no sudo. It can submit and cancel requests and can read verdicts and logs, and nothing else. The ban on ad hoc containers becomes structural rather than prose.

*History, the question as asked:* **Q-V7. How does a pipeline agent on Starfire reach Idunn?**
- A: `ssh ygg sudo -n idunn verify …` as `gamecultadmin`, which has full
  passwordless sudo (probe).
- B: a dedicated `eureka` account on Yggdrasil with an SSH key, member of group
  `idunn-verify`, and **no sudo**. The request store is `root:idunn-verify 0660`,
  so the CLI can only submit or cancel. The verdict store is
  `root:idunn-verify 0640`, readable only.
- C: a signed CultNet request surface, which is a new protocol.

**Recommended: B.** A works on day one, but with root an agent can still start
ad hoc containers, so the forbidden writer is forbidden only by prose. B makes
it structural: the account can request, read verdicts and read logs (`log_root`
group-readable), and nothing else. C is out of scope. F17 shows there is no
client surface to reuse.

Depends: the Cut 3 store split and modes, Cut 6 provisioning, and the Cut 7
negative checks.

**Q-V8: RULED B by the operator, 2026-09-22 ("agreed").** Verify runs as a separate unit, `idunn-verify.service`, which runs `idunn verify-serve` from the same binary. It has its own stores and lock, and its `ReadWritePaths` are narrowed to its own roots and the `/srv/build` cache. It protects daemon survival and privilege isolation.

*History, the question as asked:* **Q-V8. What process topology?**
- A: a verify thread inside `idunn serve`.
- B: a separate unit, `idunn-verify.service`, running `idunn verify-serve` from
  the same binary. It has its own stores and lock, and narrower
  `ReadWritePaths`: its own roots and `/srv/build` cache only, with no
  `/etc/systemd/system`, `/etc/nginx`, `/etc/ufw`, target roots or
  `idunn-authority`.

**Recommended: B.** A new process has to earn its keep by protecting a named
invariant, and B protects daemon survival. It gives failure and resource
isolation: a wedged `docker`, a panic, or a hung pump cannot stall or kill the
survival loop (F15). It also gives privilege isolation for code from arbitrary
branches: the process driving those containers cannot write units, routes or
firewall rules. B adds one unit file and one subcommand, and no new binary.

Depends: Cuts 4 and 6.

**Q-V9: RULED A by Self, 2026-09-22. The operator deferred ("I have no meaningful opinion on Q-V9").** `seccomp` is an explicit per-runner field in the operator binding. It defaults to `default`, and `unconfined` is permitted only where the binding names it. The verdict echoes it. The first user is CultLib's Linux TSan mutation.

*History, the question as asked:* **Q-V9. May a verify runner be bound with `seccomp = "unconfined"`?** CultLib's
linux native TSan mutation requires it (F28).
- A: yes, as an explicit per-runner binding field (default `default`), echoed
  into the verdict.
- B: no. Native linux mutation stays outside verify.

**Recommended: A.** Under B the stopgap cannot die whole, because that job would
need an ad hoc container forever. Only runners the operator names get it, and
the binding is root-installed.

Depends: the Cut 2 schema, Cut 6 CultLib binding, and Cut 7 completeness.

**Q-V10: RULED A by Self, 2026-09-22.** This is a default, not a product fork, and the operator deferred the adjacent Q-V9. Runner images are built from Dockerfiles committed to gamecult-ops, on digest-pinned bases, and the binding pins each image by digest.

*History, the question as asked:* **Q-V10. Who builds runner images?**
- A: the operator, or a Hands pass the operator approves, builds each image once
  on Yggdrasil from a committed Dockerfile with a digest-pinned `FROM`, and pins
  `name@sha256:<id>` in the gamecult-ops verify binding (P3). The images are
  rust+pwsh (a new Dockerfile in gamecult-ops `docker/`), the dotnet SDK
  (upstream, pinned), node (upstream, pinned), and CultLib `quic-native-linux-dev`.
- B: Idunn builds images declared by the binding, which is a new build
  authority.

**Recommended: A.** Image building is a rare privileged ops act like installing
a binding, and B grows Idunn for a job that happens a few times a year.

Depends: Cut 6.

---

## 5. Cuts

All Idunn work is on `F:\Projects\Idunn`, branch `verify/cutN` from `main`, one
cut per branch, merged after Soul. **Idunn builds and tests run only on
Yggdrasil** (build host = Yggdrasil, target platform = linux-x64). Until Cut 6
is installed, Hands and Soul use the stopgap, which the brief explicitly allows
until its deletion line:

```
ygg-verify.sh /f/Projects/Idunn <sha> rust:1.95-bookworm 'cargo test --lib'
```

It needs `/etc/machine-id` mounted, which requires one stopgap edit, or the
README's `docker run … -v /etc/machine-id:/etc/machine-id:ro` form in
`~/eureka-verify/<name>`, capped at `--cpus=4 --memory=8g`. Mutation suites use
`eureka-verify-rust` plus `tools/eureka-mutations.ps1`. The suite files are
committed under `tools/verify-mutations/` in the Idunn repo. The baseline is
P1: 126 passed and 2 ignored.

### Cut 0. Kill the stale docs (subtraction, docs only)

- **Repo/branch:** Idunn `verify/cut0`. Depends on nothing.
- **Deletes first:**
  - `docs/authority-map.md:203-213`, the whole "Current implementation boundary"
    section (11 lines).
  - `docs/deployment-authority.md:61-78`, the whole section (18 lines). Its
    second paragraph (`:71-78`) moves to the end of "Typed deployment state"
    (`:186-214`), reworded in the present tense.
- **Per-file changes:**
  - `README.md:33-40`: the CLI has five commands. List `cancel` and `validate`.
  - `README.md:204`: the `guide.md` row says the previous generation **no longer
    runs anywhere** and the file is kept for migration history. Settle the
    `deploy/legacy` row the same way.
  - `docs/deployment-authority.md`: under "Candidate promotion" (or `:20` "Authority
    map"), add one sentence of present truth. Sealing resolves, freezes, runs
    every recipe step and installs before the deployment brake is consulted.
    The brake gates `Starting`, the first Verse-visible change. Cite
    `advance_sealing`.
- **Not in this cut:** the dead gamecult-ops script (F23) and the Raven
  runbook's brake sentence. Both are recorded in section 8 as follow-ups in
  their own repo.
- **Verification:**
  - negative: `rg -n "do not yet\s*$|drive Git, runners" docs/` gives 0 hits,
    and `rg -n "crates/idunn-daemon" docs/` gives 0 hits (both tested against
    HEAD: they hit `deployment-authority.md:67-68` and `authority-map.md:205`
    only);
  - operator: none. Soul reads `advance_sealing` against the new sentence.
- **Ledger:** −29 lines, +~12.

### Cut 1. One step primitive and one freeze primitive (behaviour-preserving refactor)

- **Repo/branch:** Idunn `verify/cut1`. Depends on Cut 0 being merged, for doc
  hygiene only.
- **Deletes first:**
  - The argv assembly inside `DockerRunnerDriver::run_in_workspace` and
    `base_run_args` (`drivers.rs:1407-1499`) as a driver-private shape. It is
    replaced by the pure lowering below, and no second copy may remain.
  - The `OperatorBinding`/`CompiledDeploymentPlan` coupling of the fetch-and-archive
    core of `GitSourceDriver::freeze` (`:1221-1304`).
- **Adds:**
  - `ContainerSpec`: a plain struct with `image`, `user`, `network`
    (`None|Bridge|Named(String)`), `seccomp` (`Default` only in this cut),
    `memory_mebibytes`, `cpu_quota_percent`, `pids_limit`, `tmpfs_mebibytes`,
    `cache_root`, `secret_mounts`, and `environment` as ordered pairs. It has
    one constructor from `DockerRunnerBinding` plus the step's required
    environment and the source stamp.
  - `fn docker_run_args(spec, workspace, working_directory, argv) -> Vec<OsString>`.
    It is **pure**, with no filesystem effects: the cache-root and secret
    validation stays in the caller, before the call.
  - A `StepPort` trait with one method that runs a lowered step in a workspace
    and returns a `StepOutcome { status, stderr_tail }`. `DockerRunnerDriver`
    implements it with today's blocking `output()` semantics.
  - `ExactSource { origin, checkout, gitlinks, recipe_path }` and
    `GitSourceDriver::freeze_exact(&ExactSource, revision, transaction_id, root) -> (tree_root, snapshot_sha256, recipe_bytes)`.
    `freeze` delegates to it.
- **Per-file changes:**
  - `drivers.rs:1407-1455` becomes: build a `ContainerSpec`, call
    `docker_run_args`, call `docker`. The error text is unchanged.
  - `drivers.rs:1501-1559` (external input) uses the same lowering with argv
    `curl …`.
  - `drivers.rs:1221-1304`: `freeze` computes an `ExactSource` from the plan's
    binding and calls `freeze_exact`. The checks are unchanged:
    `verify_exact_source`, the recipe-bytes equality, hardening.
- **Authority map:** no ownership change. It creates the shared paths that
  Cut 4 must use: there is to be no second docker-argv builder and no second
  archive path.
- **Verification:**
  - builds: `cargo test --lib` on Yggdrasil stays 126 passed, 2 ignored.
  - `container_spec_lowers_to_exact_docker_argv` pins the full argv as an exact
    vector: `--network none` by default, `--cap-drop ALL`, `no-new-privileges`,
    `--read-only`, pids, the noexec tmpfs, the machine-id mount, `--cpus` as
    `quota/100` to 2 dp, and `--memory` as `Nm`. It must kill:
    - the revert mutant: drop `--cap-drop ALL`;
    - the loosening mutant: default network `bridge`;
    - a function-of-input mutant: `--cpus` = `quota/50`, with a probe at
      `cpu_quota_percent = 250`, where `2.50` and `5.00` differ.
  - `required_environment_is_the_only_environment` pins F6. Its mutant passes
    every binding `environment` entry, and the fixture has a binding variable
    the step does not name.
  - `freeze_exact_is_byte_exact_and_recipe_checked` reuses the existing git
    fixture (`drivers.rs:8409` `exact_git_archive_becomes_root_owned_immutable_source_without_git_metadata`)
    through the new entry point. Its mutant skips the recipe-bytes equality.
  - negative: `rg -n '"--cap-drop"' src/` gives exactly 1 hit (verified to
    collide with nothing else at HEAD: 1 hit today, `drivers.rs:1478` in `base_run_args`).
- **Ledger:** about −90 +140 in Idunn, including 3 tests. Net positive only by
  the tests.

### Cut 2. Verify declarations and host policy (pure types, no execution)

- **Repo/branch:** Idunn `verify/cut2`. Depends on Cut 1 and on Q-V1, Q-V3, Q-V6
  (mechanism only) and Q-V9.
- **Adds:** a new module `src/verify.rs`, with no I/O except parse.
  - `VerifyRecipe` (`schema`, `repo`, `source_stamp_environment`,
    `required_gitlinks`, `external_inputs: Vec<ExternalInput>` reusing
    `deployment.rs:79-87`, `steps: Vec<VerifyStep>`). Unknown fields are
    denied. It has no artifacts, service, state or provides, and those fields
    are unrepresentable.
  - `VerifyStep` (`id`, `runner`, `argv`, `working_directory`,
    `required_environment`, `timeout_seconds: u32`,
    `report: Option<ReportDeclaration { schema, path }>`). It has **no
    `phase`**: every verify step is a test step, and `RecipePhase` stays
    deploy-only (F2).
  - `VerifyBinding` (`schema`, `repo`, `repository: { origin, checkout,
    recipe_path, gitlinks }`, `runners: BTreeMap<String, VerifyRunnerBinding>`).
  - `VerifyRunnerBinding` (`image`, `user`, `allowed_programs`, `environment`,
    `cache_root`, `network: VerifyNetwork { None, Bridge }`,
    `seccomp: Seccomp { Default, Unconfined }`, `memory_mebibytes`,
    `cpu_quota_percent`, `pids_limit`, `tmpfs_mebibytes`, `max_step_seconds`).
    It has **no `secret_files` field**. It lowers to `ContainerSpec` (Cut 1),
    and `ContainerSpec.seccomp` gains `Unconfined`, which lowers to
    `--security-opt seccomp=unconfined`.
  - `VerifyHostPolicy` (`schema`, `host`, `max_concurrent`,
    `cpu_budget_percent`, `memory_budget_mebibytes`,
    `runner_cpu_ceiling_percent`, `runner_memory_ceiling_mebibytes`,
    `log_byte_cap`, `resident_terminal`, `history_limit`, `workspace_root`,
    `log_root`, `source_root`).
  - `VerifyBinding::admit(&VerifyRecipe, &VerifyHostPolicy)`.
  - `idunn validate --verify-recipe PATH [--verify-binding PATH] [--host-policy PATH]`,
    which extends `validate` at `control_plane.rs:1507-1539`. It stays offline
    and opens no store.
- **Rules. Each has a test that fails under its own mutant:**
  - **R-V1. A verify runner can hold no secret.** `verify_binding_rejects_secret_files`
    parses a binding carrying `secret_files` and expects an error. Its mutants
    remove `deny_unknown_fields`, and add `#[serde(default)] secret_files`
    while ignoring it.
  - **R-V2. The network is `none` or `bridge`.**
    `verify_runner_rejects_host_network` covers `"host"` and
    `"container:odin"`. Its mutant adds `Named(String)` to `VerifyNetwork`.
  - **R-V3. `argv[0]` must be in *that* runner's allowlist.**
    `verify_admit_checks_the_steps_own_runner`. The fixture has two runners
    with disjoint programs (`rust: [cargo]`, `web: [node]`) and a step
    `runner = rust, argv = [node, …]`. The revert mutant drops the check. The
    loosening mutant checks against the union of all runners' programs, and
    this fixture is the one that kills it.
  - **R-V4. A step's timeout must not exceed its runner's `max_step_seconds`.**
    Probes sit at `max` (admitted) and `max + 1` (rejected). The loosening
    mutants are `<= max * 2` and a comparison against the largest
    `max_step_seconds` of any runner. The fixture gives runners different
    maxima.
  - **R-V5. Each runner's caps must not exceed the host policy ceilings.** The
    CPU and memory ceilings get separate fixtures with different numbers. The
    mutant swaps them, comparing memory against the CPU ceiling.
  - **R-V6. The recipe's runners must equal the binding's runners exactly**, as
    deploy's rule at `deployment.rs:1426-1430`. Mutants: subset instead of
    equality, in both directions.
  - **R-V7. A report path must be relative and inside the workspace, and the
    schema must be known.** Rejects `../x`, `/abs`, and unknown schema
    `gamecult.eureka.mutation_report.v2`.
  - **R-V8. Idunn owns `HOME` and the source stamp.** The binding `environment`
    may not name `HOME` or `recipe.source_stamp_environment`, and a step may
    not list `HOME` in `required_environment`. The Cut 4 runner always sets
    `HOME=/workspace/.idunn-home`, because P2 shows pwsh dies without it.
  - **R-V9. The recipe cannot declare deploy fields.** A recipe carrying
    `[[artifacts]]` or `[service]` is rejected.
- **Verification:**
  - builds: Yggdrasil, `cargo test --lib`.
  - The mutation suite `tools/verify-mutations/cut2.psd1` covers R-V1 to R-V9
    and runs through `eureka-mutations.ps1` in `eureka-verify-rust` on
    Yggdrasil, stopgap era. It needs a no-op control.
  - negative: `rg -n "secret" src/verify.rs` finds only the rejection test's
    fixture text.
- **Ledger:** about +450 lines, including tests, in one new module.

### Cut 3. Stores, records and CLI (no worker)

- **Repo/branch:** Idunn `verify/cut3`. Depends on Cut 2 and on Q-V2 and Q-V7.
- **Adds:**
  - In `src/verify.rs` or a sibling `verify_store.rs`: records
    `VerifyRequest` (`idunn.verify_request`, v1: `verify_id`, `repo`,
    `revision`, `steps: BTreeSet<String>` where empty means all,
    `requested_by`, `requested_at_unix_millis`), `VerifyCancel`
    (`idunn.verify_cancel`, v1), and `VerifyTransaction`
    (`idunn.verify_transaction`, v1: id, request, phase, binding bytes and
    sha256, recipe bytes and sha256, frozen snapshot sha256, per-step
    `StepOutcome`, `terminal: Option<VerifyTerminal>`, timestamps).
  - The CLI commands:
    - `idunn verify <repo> --revision <40-hex> [--step ID]... [--requested-by NAME] [--no-wait] [--timeout-seconds N] [--request-store PATH] [--verdict-store PATH]`
    - `idunn verify-status [--id ID] [--verdict-store PATH]`
    - `idunn verify-log --id ID --step STEP [--log-root PATH]`
    - `idunn verify-cancel ID [--request-store PATH]`
  - Store paths default to `/var/lib/gamecult/idunn-verify/{requests.cc,verify.cc}`.
- **Rules:**
  - **R-V10. A request can carry nothing privileged.** Extend
    `cli_exposes_only_declarative_commands` (`control_plane.rs:7577`) with
    `verify x --revision <sha> --command "sh -c"`, `--argv`, `--image`,
    `--cpus`, `--memory`, `--network`, `--env` and `--seccomp`, each expected
    to be rejected. The mutant makes the verify parser ignore unknown flags.
  - **R-V11. The revision is exactly 40 lowercase hex.** Probes: 39, 41,
    uppercase, a 7-character prefix, and `HEAD`. The loosening mutant is
    `len >= 7`.
  - **R-V12. The CLI writes only the request store.**
    `verify_submit_touches_only_the_request_store`: after submitting into a
    tempdir, the verdict-store path does not exist and its mtime is unchanged.
    The mutant has the CLI pre-create a `Admitted` transaction.
  - **R-V13. Cancel refuses unknown and terminal ids.** The terminal fixture
    comes from a hand-built verdict store.
  - Exit codes for `idunn verify` in wait mode. This is a **rule, not a
    display**, because agents branch on it:
    `Passed → 0`, `Failed → 1`, `Error/Interrupted/Rejected → 3`,
    `TimedOut → 4`, `Cancelled → 5`, and wait timeout `→ 6`.
    `verify_wait_exit_code_is_derived_from_the_verdict` is a table test. The
    mutant maps `TimedOut → 1`.
- **Verification:** Yggdrasil `cargo test --lib`. Mutation suite
  `tools/verify-mutations/cut3.psd1`. Negative:
  `rg -n "verify" src/control_plane.rs` finds only parse and dispatch lines.
  No `Engine` method may reference verify types (checked by reading `impl Engine`).
- **Ledger:** about +350.

### Cut 4. The verify worker (`idunn verify-serve`)

- **Repo/branch:** Idunn `verify/cut4`. Depends on Cut 3 and on Q-V8. This map
  assumes B, a separate process. If the ruling is A, only the entry point
  changes: a thread spawned beside the hub. Every rule below still holds.
- **Adds:**
  - `idunn verify-serve --host NAME --policy PATH --bindings-dir PATH --request-store PATH --verdict-store PATH --source-uid N --source-gid N`.
    It takes its own `ProcessLock` (`control_plane.rs:2637`) on the verdict
    store.
  - The loop, per pass:
    1. Read the host policy.
    2. Observe cancels.
    3. Admit queued requests, oldest first, while `running < max_concurrent`
       and the running jobs' CPU and memory plus the candidate's stay within
       budget. The candidate's figure is the sum over the runners its selected
       steps use.
    4. Each admitted transaction runs on **its own thread**:
       - fetch the exact SHA into the verify source checkout with
         `ExactSource`, as the source uid, and **refuse any origin other than
         the binding's**;
       - read the recipe blob at that revision, parse it, and run `admit`;
       - `freeze_exact`;
       - create per-runner workspace copies and materialize external inputs;
       - run the selected steps in recipe order.
  - **Detached step execution.** This is a new `StepPort` implementation for
    verify. It spawns `docker run --rm --name idunn-verify-<id>-<step> --label gamecult.idunn.verify=<id> …`
    with stdout and stderr piped into a pump that writes the log up to
    `log_byte_cap` and counts the rest. It polls `try_wait` against the
    admitted deadline. At the deadline or on cancel it runs
    `docker kill <name>`. The argv comes from Cut 1's `docker_run_args`, plus
    the Cut 2 seccomp flag.
  - Recovery at start:
    - every non-terminal transaction is sealed `Interrupted`;
    - `docker ps -a --filter label=gamecult.idunn.verify` orphans are killed;
    - workspace directories whose id is terminal or unknown are deleted.
  - Retention: resident terminals over `resident_terminal` move to
    `verify-history.cc`, history is pruned to `history_limit`, and the log
    directories of pruned ids are removed.
  - A unit file `deploy/idunn-verify.service`: `User=root`,
    `ProtectSystem=full`, `ProtectHome=yes`, `PrivateTmp=yes`,
    `ReadWritePaths=/var/lib/gamecult/idunn-verify /srv/build/idunn-verify`,
    and `ReadOnlyPaths=/etc/gamecult/idunn/verify`. It has **no** systemd,
    nginx, ufw, target or authority paths. `Wants=docker.service`.
- **Authority map:**
  - Owner: `verify-serve`.
  - Forbidden writers: `idunn serve`, which never opens `/var/lib/gamecult/idunn-verify`;
    and `verify-serve`, which is denied `control.cc`, topology, brakes and
    `/etc/systemd/system` by the unit and never names them in code.
  - Shared paths: `docker_run_args` and `freeze_exact` from Cut 1.
  - Deletion line: none in this cut.
- **Rules. Unit tests use a fake `StepPort` and a fake container observer. The
  pipeline smoke uses real Docker on Yggdrasil.**
  - **R-V14. After each report-bearing step, the frozen files in that runner's
    workspace must hash to the frozen snapshot**, computed over exactly the
    paths in the frozen tree, so `target/` and `node_modules` are ignored. A
    mismatch seals `Error{RestoreViolated{path}}` and skips the remaining
    steps. The fixture flips one byte of a file without changing its length.
    The revert mutant skips the check. The loosening mutant compares only the
    file count or sizes; the length-preserving fixture kills it.
  - **R-V15. The deadline handed to the step port equals the step's admitted
    `timeout_seconds`.** It is observed at the call site, where the rule is
    decided, and the fake port records the value. Probes at 7 s and 1800 s.
    Mutants: a constant 1800; `timeout * 2`; `min(timeout, 600)`, which is a
    function of the input and dies at the 1800 probe; and `max(timeout, 60)`,
    which dies at the 7 probe.
  - **R-V16. At most `max_concurrent` transactions run, and CPU and memory
    budgets hold.** Table fixtures at the boundary: `running == max` means no
    admission, and a candidate that would exceed CPU but not memory, and vice
    versa, is refused. Mutants: `<` to `<=`; budget counted per repository
    instead of per host; memory checked against the CPU budget.
  - **R-V17. A worker start seals every non-terminal transaction `Interrupted`
    and kills the labelled orphans.** The mutant resumes `Running`
    transactions. A second fixture has a labelled container whose id is
    unknown.
  - **R-V18. Stop at the first failed step.** Later steps are `NotRun`, and a
    failed step's verdict is `Failed`, never `Error`. Infrastructure faults are
    `Error{kind}`: fetch, image, docker, report decode, restore. Mutants: run
    every step; map a non-zero exit to `Error`.
  - **R-V19. Fetch only from the binding origin.** A fixture origin mismatch in
    the verify checkout triggers a re-clone or a refusal, never a fetch.
  - **R-V20. Terminal is immutable.** Replacing a terminal record fails. The
    mutant lets `persist` overwrite.
  - **R-V21. The workspace is removed at terminal.** The mutant is `KEEP`
    semantics.
  - Survival isolation (negative, structural):
    - `rg -n "idunn-verify|verify_store|VerifyTransaction" src/control_plane.rs`
      finds nothing inside `impl Engine` or `fn serve`;
    - the unit diff shows `idunn-yggdrasil.service` unchanged;
    - `systemctl stop idunn-verify` leaves `idunn-yggdrasil` active, and a
      deliberately corrupted `verify.cc` crashes only `idunn-verify` (operator
      check at Cut 6).
- **Verification:**
  - Yggdrasil `cargo test --lib`, and mutation suite `cut4.psd1` (stopgap era).
  - Pipeline smoke on Yggdrasil in a scratch root: a throwaway local bare repo
    with a two-step verify recipe (`true`, then `sh -c 'exit 3'`), a tempdir
    policy, and `alpine@sha256` pinned by P3. Expect `Failed`, step 2 exit 3,
    both logs present with sha256, the workspace gone, and no container with
    the label left.
  - A second smoke with `sleep 30` and `timeout_seconds = 3` expects
    `TimedOut` and the container killed.
- **Ledger:** about +650, the largest cut. It is justified because it is the
  capability itself, and no smaller owner exists. The deploy scheduler is
  single-threaded by design (F15).

### Cut 5. Typed mutation reports (three repositories, three commits)

- **Depends:** Cut 4 and Q-V4.
- **5a. Idunn** `verify/cut5`:
  - `MutationReport { control: Control{Green,Red}, entries: Vec<MutationEntry{ id, verdict: Killed|Survived|NoVerdict|Skipped }> }`,
    decoded strictly (`deny_unknown_fields`) from the step's declared path. The
    counts are derived in Idunn, and a JSON `counts` field is rejected as
    unknown.
  - **R-V22. A report-bearing step passes only if exit is 0, the control is
    Green, and there are zero Survived, NoVerdict and Skipped entries.** Exit 0
    with any of those non-zero is `Error{ReportContradictsExit}`, and so is a
    non-zero exit with a clean report. A missing or malformed report is
    `Error{Report}`, never `Failed`. Table test mutants: treat Skipped as
    passing, which is exactly F27's mutate-cultmesh "skipped is covered by
    nothing" lesson; trust the exit code alone; count `NoVerdict` as killed.
  - **R-V23. Entry ids are unique and the Control entry is not an entry.** The
    fixture has a duplicate id.
- **5b. Eureka** `GameCult/Eureka`: `tools/eureka-mutations.ps1` gains
  `-Report <path>`. It writes the report with temp-then-rename **before**
  exiting or throwing, including on a red control. The report is UTF-8 without
  BOM, because P2's pwsh writes a BOM with `Out-File`, so it uses
  `[IO.File]::WriteAllBytes`. Commit the pending working-tree edit first or
  separately, because it is not this cut's.
- **5c. CultLib**: `scripts/mutate-cultmesh.mjs` gains `--report <path>`
  (main). `scripts/mutate-dotnet.ps1` gains `-Report` **on
  `cultnet/selection-cut1`**, where it lives, and lands with that branch.
- **Verification:**
  - Idunn on Yggdrasil.
  - Each harness is run under verify once Cut 6 is installed. Before that, run
    them with the stopgap, with one no-op-only suite per harness proving that
    the report round-trips through Idunn's decoder via
    `idunn validate --mutation-report PATH`, a small offline decode added in 5a.
- **Ledger:** about +150 in Idunn, about +40 per harness.

### Cut 6. Install, bind, recipe

- **Depends:** Cuts 4 and 5a, and Q-V6, Q-V7, Q-V9 and Q-V10. The build host is
  Yggdrasil (`rust:1.95-bookworm`, `--cpus=4 --memory=8g`, niced, about 1 min
  warm). The install path is the existing
  `gamecult-ops/scripts/install-idunn-yggdrasil-release.sh`. It is extended to
  install `idunn-verify.service` too, or given a sibling
  `install-idunn-verify-yggdrasil.sh`.
- **gamecult-ops adds:**
  - `idunn/yggdrasil/verify/host.toml` (the Q-V6 values);
  - `idunn/yggdrasil/verify/bindings/{idunn,cultlib,huginn,eureka}.toml.in`,
    with image digests as `PROVISIONED_*_IMAGE_DIGEST` tokens, like the
    existing signer tokens;
  - `docker/verify-rust-pwsh.Dockerfile` (digest-pinned `FROM`, pwsh from
    tarball with a sha256 check);
  - a runbook `runbooks/idunn-verify-yggdrasil.md`: install, account, images,
    how to read verdicts, and how to suspend (`max_concurrent = 0`).
- **Yggdrasil:**
  - groups `idunn-verify`;
  - the `eureka` account and key, if Q-V7 is B;
  - roots `/var/lib/gamecult/idunn-verify` (`root:idunn-verify 2750`, with the
    request store at 0660) and `/srv/build/idunn-verify/cache/{cargo,nuget,npm}`;
  - images built and pinned.
- **Recipes (one commit per repo):**
  - `Idunn: deployment/idunn/verify.toml`: `test-lib` = `cargo test --lib`,
    plus one step per committed mutation suite.
  - CultLib: `test-rust`, `test-dotnet`, `test-ts`, `mutate-cultmesh-shared`,
    `build-quic-native-linux` then `mutate-cultmesh-native` on the `quic-native`
    runner, which is `seccomp = "unconfined"` under Q-V9.
  - Huginn, as its campaign needs.
- **Verification:**
  - operator: `idunn verify idunn --revision <cut5 sha>` gives `Passed`, and
    `verify-log` shows 126 passed. On the same host, `systemctl stop idunn-verify`
    leaves `systemctl is-active idunn-yggdrasil` active. Corrupt a copy of
    `verify.cc` (swap the store path) and confirm only `idunn-verify` fails.
  - As the `eureka` user: `sudo -n true` fails, `docker ps` is denied, and
    `idunn verify` works.
  - The first Soul pass that runs **through verify**, not the stopgap, is this
    cut's Soul pass.

### Cut 7. Eureka moves to `idunn verify`; the stopgap dies (deletion line)

- **Repo:** `GameCult/Eureka` plus the Yggdrasil host. Depends on Cut 6 passing
  its Soul pass.
- **Deletes first:**
  - `tools/stopgap/ygg-verify.sh` (91 lines) and `tools/stopgap/rust.Dockerfile`
    (9 lines), with the directory;
  - `SKILL.md:417-424`, replaced by the verify rule. Heavy verification goes
    through `idunn verify` on Yggdrasil: exact pushed SHA, recipe steps,
    verdict exit codes, and `verify-log` for evidence. The Starfire exception
    stays only for Windows-only jobs, per Q-V5.
- **Adds:**
  - `references/briefs.md` Hands and Soul templates gain the verify
    instruction. SKILL.md says "Every Hands and Soul brief says so", but
    `briefs.md` names no verification host today, a gap found in this pass.
  - A changelog entry. The 2026-09-22 entry stays as history.
- **Yggdrasil cleanup:**
  - `rm -rf ~/eureka-verify`;
  - `docker volume rm eureka-cargo-registry eureka-nuget`;
  - `docker image rm eureka-verify-rust`, unless Q-V10's image reused the tag
    (it must not: the new image is `gamecult/verify-rust-pwsh`).
- **Negative proof that the stopgap is dead:**
  - `rg -n "ygg-verify|eureka-verify|stopgap" ~/.claude/skills/eureka --glob '!**/changelog.md'`
    gives 0 hits. Tested at HEAD, this currently hits `SKILL.md:420` and the
    two stopgap files only.
  - `ssh ygg 'test ! -e ~/eureka-verify'` succeeds.
  - `ssh ygg 'sudo docker volume ls -q | grep -c "^eureka-"'` returns 0.
  - `ssh ygg 'sudo docker ps -a --format "{{.Names}}"'` lists only the known
    services plus `idunn-verify-*`.
  - If Q-V7 is B, it becomes structural: the agent account cannot run docker at
    all, so there is no ad hoc container it could start.

### Cut 9 (placeholder). Windows verify host

This cut is activated only by Q-V5 B or C. If B, it is a new
`HostActuatorRequest::VerifyStep` variant and a host-native verify runner, with
the same rules R-V14 to R-V22 lowered for the actuator. Its build host would be
Windows, and `idunn-host.exe` is built on Starfire today, which the load
budget now forbids for heavy builds. That tension belongs to the operator.

---

## 6. Not in scope

- Deploying Eureka artifacts, or any deploy change to managed targets.
- General CI for every GameCult repository. What comes free is that any repo
  with a verify recipe and binding can be verified. Adding recipes beyond
  Idunn, CultLib, Huginn and Eureka is not in this campaign.
- Deploy consulting verdicts, meaning "admit only verified revisions".
- Converting deploy's blocking materialize to the detached step port. F15 is a
  real survival defect, but it is deploy's, and it is recorded in section 8.
- A CultMesh or Eve projection of verify status. By doctrine it is the proper
  operator surface, and it is recorded as a follow-up. The CLI is the interface
  for now.
- Resuming interrupted verifies, memoizing verdicts by `(repo, revision,
  recipe, step)`, and running steps in parallel within one transaction.
- Deleting `RecipePhase` from deploy (F2). It is decorative, but removing it
  changes `gamecult.idunn.target_declaration.v1` because unknown fields are
  denied, so it is a deploy schema bump.

---

## 7. Subtraction ledger and build budget

| Cut | Removed | Added | Deps / targets / formats | Build host → target |
|---|---|---|---|---|
| 0 | 29 doc lines | ~12 doc lines | — | none |
| 1 | ~90 (argv builders, plan-typed freeze core) | ~140 incl. 3 tests | — | Yggdrasil → linux-x64 |
| 2 | 0 | ~450 incl. tests | +3 schemas (recipe, binding, host policy) | Yggdrasil → linux-x64 |
| 3 | 0 | ~350 | +3 record types, +2 stores, +4 CLI verbs | Yggdrasil → linux-x64 |
| 4 | 0 | ~650 | +1 subcommand, +1 unit; **no new binary, crate or dependency** | Yggdrasil → linux-x64 |
| 5 | 0 | ~150 Idunn, ~40 × 3 harnesses | +1 xenos JSON schema (report) | Yggdrasil → linux-x64 (Idunn); harnesses need no build |
| 6 | 0 | ~150 config + runbook, 1 Dockerfile, 3–4 recipes | +1 image build (rust+pwsh) plus pinned upstream images | Yggdrasil builds the image |
| 7 | 100 lines (stopgap), ~8 SKILL lines; Yggdrasil: `~/eureka-verify` (14 MB), 2 volumes, 641 MB image | ~15 skill/brief lines | −1 ad hoc runner path, −1 free-string command surface | none |

The net is about +1,900 lines. This is a positive delta bought for an
explicitly requested capability, and each part of it has a reason:

- Verify retires three verification paths: the stopgap, workstation runs, and
  (by follow-up) the Epiphany text receipts.
- It makes one of them structurally impossible (Q-V7 B).
- The cheaper-looking alternative routes through the survival scheduler (§2,
  rejected path).

Hands may escalate a miss, with an argument, on Cut 4 especially.

Build budget: every Idunn build is one crate, the `idunn` package, profile
`test` (plus `release` in Cut 6), with no features and target linux-x64. It is
built on Yggdrasil in `rust:1.95-bookworm` at 4 CPU and 8 GiB, niced, with the
cargo registry cached. Measured warm test compile: 31.9 s (P1). Its footprint
is one `target/` per workspace (no `target/` is shared between checkouts),
deleted with the workspace. Nothing is compiled on Starfire in any cut.

---

## 8. Follow-ups outside this campaign

- **Deploy materialize blocks the survival tick** (F15;
  `control_plane.rs:3847`, runbook `idunn-host-raven.md`). Move deploy steps
  onto Cut 4's detached step port. This can wait because it is today's
  behaviour, and verify no longer adds to it.
- **The Raven runbook says the deployment brake prevents building during a
  stream** (`runbooks/idunn-host-raven.md`, "Known edges"). F16 shows the build
  precedes the brake. The fix is in gamecult-ops. It can wait because it is doc
  truth, not behaviour.
- **`scripts/request-idunn-bounded-redeploy-yggdrasil.sh` is dead** (F23). Delete
  it in gamecult-ops. It can wait because nothing can execute it successfully.
- **Epiphany's legacy test receipts** (`deploy-epiphany-yggdrasil.sh:171-208`)
  become a verify verdict when Epiphany moves to the new generation.
- **`/var/lib/gamecult/idunn/staging` has no retention** (F18: 382 entries,
  4.1 GiB). Deploy owns this.
- **Verify status as a CultMesh/Eve projection** (doctrine).

## 9. What this pass could not probe

- The exact revision of the installed `/usr/local/bin/idunn`. It has no version
  stamp. It predates `5b3f646`.
- Whether GitHub serves `git fetch origin <sha>` for a SHA reachable only from a
  non-default branch, under the verify source driver's uid and filtered clone. I
  expect yes, since GitHub allows fetching reachable SHAs, but it is unprobed.
  Cut 4's smoke must include one fetch from `cultnet/selection-cut1`.
- CultLib's native TSan mutation inside Idunn's hardening, meaning
  `--read-only`, `--cap-drop ALL`, user 65532 and a noexec `/tmp`, combined with
  `seccomp=unconfined`. `setarch -R` may need more than seccomp relaxation.
  This is the first Cut 6 smoke and may reopen Q-V9.
- dotnet test and npm under the hardened shape with a bridge network and a
  NuGet cache on `/cache`. Only the restore of an empty console app was probed
  (P2).

## Progress (Self)

- **2026-09-22: Cut 0 landed** at `2eb25b6` (docs only).
  - Both stale "Current implementation boundary" sections are deleted.
  - The README lists five commands.
  - The docs now state that the build runs before the brake.
- **2026-09-22: Cut 1 landed** at `95e15ef` (Sonnet, verified on Yggdrasil through the stopgap).
  - Adds `ContainerSpec` and `docker_run_args`, the only Docker argv lowering in the codebase (`--cap-drop` has one site).
  - Adds `ExactSource` and `freeze_exact`.
  - Tests: 126 to 130. `tools/verify-mutations/cut1.psd1` kills 5/5, with M0 green.
  - Discrepancies Hands reported:
    - The external-input curl step now goes through the shared lowering. It gains `--workdir` and one inert environment variable.
    - The gitlink cache directory is named by a hash of the checkout path, because `ExactSource` has no target.
    - The size is `drivers.rs` −178/+426, against a ledger of about −90/+140. The type surface and a fourth, negative test account for the difference.
  - **Soul's pass is dispatched.**
- Rulings so far: Q-V1 A, Q-V2 A, Q-V3 A, Q-V4 B via `Add-Type`, Q-V5 A, Q-V7 B, Q-V8 B.
- **2026-09-22: Soul on Cuts 0–1** (Opus, on Yggdrasil).
  - **Held.**
    - Behaviour is preserved. A fake-docker probe over 8 real call paths found the security flags byte-identical and in order.
    - Only the declared differences appeared, plus one extra: secret environment variables are reordered (F4). That is harmless in Docker and is **accepted, now declared**.
    - The numbers reproduce: tests 126 to 130, and cut1 5/5. Both needed a machine-id; see F10, fixed in the stopgap at Eureka `2fdc056`.
  - **F1 (high): the argv test pins only the default runner path.** Thirteen of 14 mutants survive, including:
    - `--cap-drop ALL` dropped only on `bridge`;
    - `--cpus` rounded to 0.5;
    - environment passthrough by prefix;
    - ambient `GAMECULT_*`/`IDUNN_*` environment;
    - writable secret mounts;
    - explicit `none` lowered to `bridge`;
    - `--read-only` dropped on a named network;
    - an extra mount of the cache's parent;
    - the secret and cache-root validation calls removed.
  - **F2 (medium): `freeze_exact` is `git archive`, so it is not byte-exact.** Non-recipe files come through with `eol=crlf` applied, `export-ignore` dropped, `export-subst` and `ident` expanded, and LFS left as pointers. The recipe check does catch transforms on the recipe itself. The test asserts no byte-exactness at all.
  - **F3:** deploy's `freeze` fetches twice.
  - **F5:** the gitlink loop is untested; skipping it survives.
  - **F7:** `StepPort` and `StepOutcome` are dead code, and `run_step` duplicates `docker()`.
  - **F8:** the `--cap-drop` negative check also matches a test.
  - **F9:**
    - `docs/guide.md:11` is still stale.
    - These gamecult-ops files still route through the dead `idunn redeploy` script: `runbooks/odin-yggdrasil.md:88`, `ghostlight-dungeon-yggdrasil.md:134,243`, `heimdall-discord-launch.md:125,144`, `epiphany-yggdrasil-deploy.md:156`, and the `bootstrap-ghostlight-yggdrasil.sh` and `bootstrap-codex-connector-yggdrasil.sh` scripts.
  - **Pre-existing:** a binding environment variable with the source stamp's name overrides the stamp.
- **Self's rulings for the Cut 1 fix batch, 2026-09-22:**
  - **F1.** The lowering is pinned on every branch: network `none`, `bridge` and named; cache present and absent; secrets present; required environment present; a range of CPU quotas (100, 150, 250, 333, 800). Every Soul mutant becomes an entry and must die. The validation calls are pinned by tests that go through `ContainerSpec::for_step`.
  - **F2. The frozen tree is the commit's blobs, exactly.** Blobs are written raw from the object store (`ls-tree -r -z` plus `cat-file --batch`), with no attribute transforms. Modes and symlinks are preserved as the tree records them, and gitlinks are handled as today. LFS content is **not fetched**: a pointer is the blob, and the verdict records that the tree contains LFS pointers. Verifying an LFS repository's real content is a follow-up. **Deploy shares this freeze, so its behaviour would change.** Before changing it, Hands surveys the `.gitattributes` of every Idunn deploy target (the target repos named in the gamecult-ops bindings) for `export-ignore`, `export-subst`, `ident`, `eol`/`text`, `filter` and LFS. If any deploy target depends on an archive transform, **stop and report**: that is a fork for the operator. The test asserts byte equality against `cat-file` for every file of a fixture that has CRLF, `export-ignore`, `export-subst`, `ident`, a symlink, the executable bit and a gitlink.
  - **F3:** `freeze` fetches once.
  - **F5:** add a gitlink test and entry.
  - **F7:** delete the dead `StepPort`/`StepOutcome`. There is one Docker spawn path; `run_step` goes through `docker()`.
  - **F8:** make the negative check `rg` over `src/` excluding `#[cfg(test)]`, or state its hit count.
  - **F9:** fix `guide.md:11`, and repoint every listed gamecult-ops runbook and bootstrap script at the current procedure (`idunn up` plus a brake release, as in `runbooks/idunn-host-raven.md`), or delete the steps. Delete the dead script.
  - **Stamp override:** refuse a binding environment variable whose name collides with the source stamp.
- **2026-09-22: the Cut 1 fix batch landed** (Sonnet, verified on Yggdrasil).
  - **Commits:** Idunn `7a5d32f` (F1, F3, F5, F7, F9, stamp override) and `95016e8` (F2); gamecult-ops `62b41b1` (runbooks and bootstraps moved off the dead `idunn redeploy`, script deleted).
  - **Tests:** 130 to 142. `cut1.psd1` 17/17 killed. The revert to `git archive` dies on `freeze_exact_is_byte_exact_across_every_attribute_transform`.
  - **The F2 survey found one attribute:** Eve's `packages/*/dist/** text eol=lf`, reached through Ghostlight's gitlink. **Self ruled it not a fork.** `eol=lf` transforms nothing on stored-LF blobs. At Eve rev `672c0c1e`, all 26 `dist` files hash the same through `git archive` and through `hash-object --no-filters`, and none contains a CR. Every other deploy target (Ghostlight, Odin, CodexConnector, Muninn, Heimdall and its CultLib gitlink) is clean. So the byte-exact freeze produces exactly what deploy got before.
  - **Deleted:** `git_archive_into`, `materialize_gitlink_archive`, and the dead `tar_program`.
  - **Not yet reached:** `cut1-freeze-exact-recipe-check`. Both sides are now the same raw blob, so it is kept as defense in depth.
  - **Soul's S8 is not yet reached.** `harden_frozen_source` enforces the same invariants first.
  - **Follow-up:** `freeze_exact` returns an LFS-pointer flag that `FrozenSourceReceipt` does not carry yet. It must reach the verify verdict (Cut 2/4).
  - **Recorded, not this cut's:** `test-voidbot-swarm-yggdrasil.sh` fails on `add-daemon-health-trust-binding`. That failure is pre-existing.
  - **Soul's pass dispatched.**
- **2026-09-22: Soul on the Cut 1 fix batch** (Opus, on Yggdrasil). **Cut 1 does not close.**
  - **Held.**
    - 142 tests; cut1 17/17 killed.
    - Byte-exact on a 3,026-entry fixture: 0 mismatches, against 7 on the old commit. The fixture covers CRLF, export-*, ident, LFS, the three symlink kinds, the executable bit, a gitlink carrying attributes, hostile names, and 6 MB and empty files.
    - Hostile trees are refused.
    - **Deploy equivalence: identical digests on all five live targets.**
    - F3: one fetch.
    - The stamp refusal holds.
    - No live `redeploy` remains.
  - **F1 (high): the raw freeze is 40 to 80 times slower on real targets.** Ghostlight takes 412 s against 5.3 s, Heimdall 457 s against 8.5 s, Odin 90 s against 2.2 s. `cat-file --batch` on the blobless checkout (`drivers.rs:788,1053,1245`) fetches missing blobs lazily in small packs. `read_blobs` also holds the whole tree in memory.
  - **F2 (high, new regression): a tree with duplicate or aliased names makes root write outside the frozen root.** A symlink `a` pointing outward plus a subtree `a/pwn` gives a root-owned `pwn` outside the root, left behind after the freeze fails. `fs::write`, `symlink` and `create_dir_all` follow symlinks and run before hardening (`:1173-1203`, `:1350`). The old tar path refused this tree. The fetch does not set `transfer.fsckObjects`.
  - **F3 (medium):** by the same mechanism, the recipe check *can* fail, and a single alias silently freezes the wrong content. The "not yet reached" claim is false.
  - **F4 (medium):** no test pins the hardening. Removing both `harden_frozen_source` and `validate_frozen_source` from `freeze_exact` survives all 142 tests. The two calls are redundant.
  - **F5 (medium, pre-existing):** a symlink chain (`D -> .`, `L -> D/D/../../../etc/passwd`) passes the lexical check (`:5516`) and resolves to `/etc/passwd`.
  - **F6 (medium):** unpinned combinations. A named network plus a cache drops `--read-only` (N1). A secret mount becomes writable when a plain environment variable is present (N2). The stamp is dropped when a secret and a plain variable are both present (N3). The LFS flag is never set (N5).
  - **F7 (medium):** `bootstrap-ghostlight-yggdrasil.sh` and `bootstrap-codex-connector-yggdrasil.sh` still provision the previous generation (`/srv/odin/deploy-manifests`, `daemon-health-trust.cc`, `/srv/ghostlight/current`, the old identity store). Only their last line changed, and neither has a brake release.
  - **F8 (low):** `heimdall-discord-launch.md` says `idunn up heimdall`, but Heimdall has no installed binding.
  - **F9 (low):** `read_blobs` never drains stderr, and its error paths leave zombie processes.
- **Self's rulings for the second Cut 1 fix batch, 2026-09-22:**
  - **F1:** fetch the exact commit's whole tree in **one bulk fetch** (depth 1, no blob filter), then read blobs locally. Stream to disk rather than holding the tree in memory. Target: within twice the old times on the five live targets, measured on Yggdrasil at the host's git version if the container can match it. Otherwise say which version you measured with.
  - **F2 and F3: two layers, both required.**
    - Fetch with `transfer.fsckObjects=true` (or run fsck on the fetched tree) and refuse duplicate, aliased and case-folding-colliding entries before anything is written.
    - The writer never follows a link: each parent component must be a real directory created by the freeze itself, and symlinks are written last.
    - Fixtures: Soul's duplicate-name and alias trees. Nothing is written outside the root, and the error is refused by name.
  - **F4:** keep one hardening pass, the one that runs last, and pin it. Tests assert that absolute, escaping and chained symlinks are refused and that modes are root-owned 0444/0555. Its removal must die.
  - **F5:** the symlink check resolves the full chain inside the root; a lexical check is not enough. The chain fixture must refuse.
  - **F6:** pin N1, N2, N3 and N5 (the flag is set and tested). Carrying the flag into `FrozenSourceReceipt` stays in Cut 2/4.
  - **F7:** delete the previous generation's provisioning steps from both bootstrap scripts. Keep only what is true under the v2 bindings, and add the brake-release step. If a script has nothing true left, delete it and point to the binding-install runbook.
  - **F8:** mark the Heimdall runbook step as gated on installing its binding.
  - **F9:** drain stderr and wait on the child on every path.
- **2026-09-22: the second Cut 1 fix batch landed** (Sonnet, on Yggdrasil), Idunn `82e0edf` and gamecult-ops `9fc3af0`.
  - **F1:** one bulk `git fetch --stdin` of every blob, plus a streaming writer.

    | Target | Freeze time | Previous | Ratio |
    |---|---|---|---|
    | Ghostlight | 5.17 s | 5.3 s | 0.98x |
    | Heimdall | 10.08 s | 8.5 s | 1.19x |
    | Odin | 2.20 s | 2.2 s | 1.0x |
    | Muninn | 1.93 s | 1.9 s | 1.0x |
    | CodexConnector | 1.76 s | 1.8 s | 0.98x |

    Measured with git 2.39 in the container, not the host's 2.47.
  - **Digests are identical on all five live targets.**
  - **F2 and F3:** `refuse_conflicting_tree_entries` runs before any write, and `ensure_frozen_directory` never follows a link. `transfer.fsckObjects` is kept as defense in depth. Its entry is *not yet reached*, because the refusal always fires first.
  - **F4:** one hardening pass, pinned.
  - **F5:** chain-aware symlink resolution.
  - **F6:** N1, N2, N3 and N5 pinned.
  - **F9:** stderr is drained and the child is waited on.
  - **F7:** the four v1 artifacts are removed from both bootstraps, with a v2 anchor export and a brake release.
  - **F8:** the Heimdall step is gated.
  - Tests go from 142 to 152. cut1 is 28/28 killed.
  - **Operator step, outside this campaign:** the Ghostlight and CodexConnector bindings still carry a placeholder `expected_signer_identity_id`. The scripts print the derived key.
  - **Soul's third pass dispatched.**
- **2026-09-22: Soul's third pass on Cut 1** (Opus, on Yggdrasil).
  - **Held:**
    - 152 tests pass. cut1 killed 26/26 before its deletion; the commit message said 28.
    - Speed is 1.0 to 1.3x across the five targets, and the digests are identical.
    - A Forgejo origin works, and so does protocol v2 without `allowAnySHA1InWant`.
    - Freezing 30k files plus a 200 MiB blob takes two fetches.
    - Duplicate, alias and gitlink attacks are refused, and nothing is written outside the root.
    - The chain resolver works for cycles, depth, and `copy_artifact`.
  - **Cut 1 does not close.**
  - **S1, medium-high:** `resolve()`'s fetch (`drivers.rs:1735`) has no fsck, so fsck never sees the main repo's trees. The claim "not yet reached" is false. Two sibling trees both named `d` freeze merged. fsck-rejected names freeze too: `.GIT`, `git~1`, `.git.` and `.git` with a zero-width non-joiner or a trailing space, `/` inside a name, and zero-padded modes.
  - **S2, medium:** both bootstraps print `provider-health-public-key`, but the binding needs the identity id (`provider-health-identity-id`).
  - **S3, medium:** neither bootstrap can work. The `ghostlight`, `codex-connector` and `odin` templates set `checkout = /srv/build/idunn-sources/*`, but the installed unit uses `--source-root /var/lib/gamecult/idunn/sources`, and the host's `odin.toml` already uses that. The two scripts contradict each other about `codex-connector.service`. v1 provisioning remains in both. The wiring test pins the old unit text.
  - **S4, low:** Heimdall's binding *is* installed on Yggdrasil, so the runbook note is wrong about the host.
  - **S5, low:** these guards are unpinned:
    - root ownership (X04);
    - an exact duplicate path (X06);
    - case-sensitive ancestor alias (X08);
    - `ensure_frozen_directory` containment (X09);
    - pass 1 `create_dir_all` (X10);
    - `copy_artifact` containment (X11).
  - **S6, low:** a symlink is validated against `.partial` but published after the rename. Dangling in-root links are now refused, where the old lexical check accepted them.
  - **S7, low:** the case-folding refusal is partial and inconsistent (files only, `to_lowercase`). The host is case-sensitive ext4.
  - **S9, low, pre-existing:** `hash_frozen_source_tree` reads each file whole. A 200 MiB file costs 232 MB of RSS.
- **Self's rulings for the third Cut 1 fix batch, 2026-09-22:**
  - **S1:** fsck guards every fetch that brings the main repository's objects. That is `resolve()` as well as `freeze_exact`. Independently, `freeze_exact` also runs fsck on the selected tree, so that objects already present in the local store are checked too. Refuse every `.git` look-alike by name, case-insensitively and under fsck's own rules, even though the host is case-sensitive, because a frozen tree can be copied elsewhere. Fixtures: Soul's `dup-trees` fixture and each look-alike. Each must refuse through the production path (`resolve`, then `freeze`), not a hand-built fetch.
  - **S2:** fixed in the S3 deletion.
  - **S3: delete both bootstrap scripts.** Nothing true is left in them, which is the F7 ruling applied. Point every caller at the binding-install procedure, which includes the `provider-health-identity-id` step. **Correct the `ghostlight`, `codex-connector` and `odin` templates to the source root the installed unit uses** (`/var/lib/gamecult/idunn/sources/*`), matching the host's installed `odin.toml`. Delete the wiring test's assertions on old unit text, or the whole test if nothing true remains. This is gamecult-ops hygiene the campaign exposed, and it lands in the same batch.
  - **S4:** correct the Heimdall runbook note. The binding is installed on the host and absent from the repo.
  - **S5:** a behavioural test for each guard, through `freeze_exact` wherever the guard is reachable from there. No mutation suite.
  - **S6:** validate symlinks against their final published location. Allow a dangling link whose target stays inside the root (resolving every component that exists), and refuse one that escapes. Test both.
  - **S7:** delete the case-folding refusal. The host is case-sensitive, and the partial check is inconsistent. The `.git` look-alikes are covered by S1.
  - **S9:** stream the hash.
- **2026-09-22: the third Cut 1 fix batch landed.** Idunn `57ecd43..f540d50`, gamecult-ops `2f3c1b3` and `9d0e37e`.
  - **S1:** fsck guards `resolve()`'s fetch, and `freeze_exact` fscks explicitly. The dup-trees fixture and all six `.git` look-alikes are refused through the production path.
  - **S3:** both bootstrap scripts are deleted (319 lines). Callers point at the binding-install procedure. The three `checkout` templates now match the installed source root. The stale wiring assertions are gone.
  - **S4:** the Heimdall note is corrected.
  - **S5:** six behavioural tests.
  - **S6:** symlinks are judged at their published location. A dangling in-root link is accepted, and the `.partial` case refuses.
  - **S7:** the case-fold refusal is deleted.
  - **S9:** hashing streams. Peak RSS on a 200 MiB blob falls from 232 MB to 25.4 MB.
  - Tests: 152 to 160. Digests unchanged on all five targets, with 0 mismatches over 1,975 entries. Times run 0.96x to 1.17x.
  - **Discrepancies to judge:**
    - No test stands up a real HTTPS origin, so the S1 test reproduces `resolve()`'s fetch invocation and then calls the real `freeze_exact`.
    - `test-ghostlight-yggdrasil-wiring.sh` still pins the static units and the bespoke deploy scripts. Whether that whole generation is superseded by the binding and transient-unit path is a **follow-up**, larger than this batch.
  - **Soul's fourth pass dispatched.**
- **2026-09-22: Soul's fourth pass on Cut 1** (Opus, on Yggdrasil). **Cut 1 does not close.**
  - **Held, and proven harder than the shipped tests show.** Soul stood up a real smart-HTTPS origin in the container, with a self-signed CA and `git-http-backend`, and froze a clean repo end to end over it. Every ingress carries fsck (`:833`, `:1137`, `:1525`, `:1536`, `:1610`, `:1629`, `:1736`). Eleven hostile fixtures, brought in by an **unguarded** fetch first so the objects were already local, were all refused by `freeze_exact`'s own fsck: dup-trees, six `.git` look-alikes, and three attacks nobody had tried (an unsorted tree, a zero-padded mode, and mode 120000 on a tree). S6 holds across eight link shapes. S7 cannot collide on ext4. S9's RSS holds at 27 MB for 30k files plus a 200 MiB blob. The gamecult-ops templates match the host, and no caller invokes a deleted bootstrap. 160 tests; digests identical on four targets over 1,109 entries.
  - **S4-1, medium-high, introduced by this batch: the symlink resolver is exponential.** `resolve_frozen_source_symlink` (`drivers.rs:5878-5945`) recurses per path component, so `MAX_HOPS = 40` bounds depth rather than work. Measured: n=16 2.7 s, n=18 10.7 s, n=20 43.7 s, and n=30 about 6 hours. Through production, `freeze_exact` with n=18 returned `Ok` after 15.9 s and `observe_frozen` spent another 15.8 s. Both run on the single scheduler thread with no timeout, so 40 small symlinks pushed to any bound repository wedge continuity for every target. **It is a regression:** at `82e0edf` the check was `path.canonicalize()`, which did the n=30 fixture in 2 ms and let the kernel's ELOOP refuse it.
  - **S4-2, medium: the streamed hash has no behavioural pin.** Hashing zero bytes of every file, or only the first 64 KiB, leaves 160 tests green. The digest is the receipt anchor, so a content-blind digest lets two trees share one receipt.
  - **S4-3, medium: three of the five S5 tests do not kill their guard.** The duplicate-path check, `ensure_frozen_directory`'s root containment and `create_dir` against `create_dir_all` all survive deletion, because another refusal fires first. The batch note said six tests; five landed.
  - **S4-4, low: all three fsck guards survive deletion**, because the shipped test hand-builds the argv and every fixture is refused at the fetch, so the arm that exercises the mechanism never runs.
  - **S4-5, low:** nothing proves a legitimate in-root `..` link freezes.
  - **S4-6, low, pre-existing:** the writer buffers a whole blob for a symlink. A 64 MiB blob with mode 120000 takes RSS to 206 MB before failing closed.
  - **Could not run, and not this batch's:** Heimdall's digest fails at every revision, including `82e0edf`. Its `vendor/CultLib` gitlink names `dfe07704`, which GitHub no longer serves. **So `idunn up heimdall` would fail on the host today.** Recorded as an ops follow-up.
- **Self's rulings for the fourth Cut 1 fix batch, 2026-09-22:**
  - **S4-1: bound the work, not the depth.** Resolve iteratively rather than recursively: keep one resolved prefix, and count **every link traversal** against one budget, so a chain costs O(n). Memoise resolved prefixes. Keep S6's behaviour exactly: a dangling in-root link freezes, an escape refuses. Add a test with Soul's `a0 -> .`, `ak -> a{k-1}/a{k-1}` fixture at n=20 that must complete in **under a second** and refuse or accept as S6 says. Say which.
  - **S4-2:** pin the hash with two trees that differ only in file content past 64 KiB, and assert their digests differ. The zero-length-buffer and first-chunk-only mutants must die.
  - **S4-3:** each S5 test isolates its own guard. Build a fixture that **only** that guard refuses, so deleting the guard makes the test fail rather than another refusal covering it. Do that for the duplicate path, root containment and `create_dir`.
  - **S4-4:** commit Soul's already-local fixtures as tests. Establish the checkout on a clean commit, bring the hostile objects in with an unguarded fetch, then prove `freeze_exact`'s own fsck refuses. Deleting each of the three guards must fail a test.
  - **S4-5:** test that a legitimate in-root `..` link freezes.
  - **S4-6:** read a symlink target without buffering a whole blob, and refuse a target longer than the platform's limit.
  - **Heimdall:** record as an ops follow-up, **FU-Heimdall-Gitlink**. The binding is installed, and its gitlink names a commit GitHub does not serve. That is not this campaign's to fix, but the deploy is broken today and the operator should know.
- **2026-09-22: the fourth Cut 1 fix batch landed** at `9475dfa` (Sonnet, on Yggdrasil). One commit, `src/drivers.rs` only, +940/-35.
  - **S4-1:** the resolver is iterative and memoised, and counts link traversals against one budget. Soul's n=20 chain goes from **43,700 ms to effectively instant**, and it freezes, which matches S6: every hop resolves inside the root and nothing escapes.
  - **S4-2:** the hash is pinned by two trees differing only past the first chunk. Both the zero-length-buffer and first-chunk-only mutants die.
  - **S4-3:** each of the three guards has a test that isolates it, and all three mutants die.
  - **S4-4:** Soul's 11 already-local hostile fixtures are committed as tests. Deleting the explicit fsck, or the freeze's own fetch flag, each fails a test.
  - **S4-5 and S4-6:** landed.
  - Tests 160 to 168. Digests identical on all five reachable targets over 1,109 entries. Heimdall is still unreachable for the unrelated gitlink reason.
  - **Gap, honestly reported: `resolve()`'s own fsck flag has no test.** Reaching the real `resolve()` needs a validated binding, which needs an HTTPS origin. Rewriting the origin with `insteadOf` fails `ensure_checkout`'s own origin-equality check first, so the shortcut cannot work. **Soul has a working smart-HTTPS rig from its first pass; that is where this closes.**
  - A note on the fsck fixtures: some hostile objects are refused by the git server's own `pack-objects` before any Idunn code runs, so they cannot serve as already-local fixtures. The test asserts that at least one fixture reaches the explicit fsck, so it cannot pass vacuously.
  - **Soul's fifth pass dispatched**, including a judgement on the batch's size.

## Soul pass 5 on Cut 1's fourth fix batch, 2026-09-22

Range `f540d50..9475dfa`, all runs on Yggdrasil (`eureka-verify-rust:471272061cc0`,
4 CPUs / 12 GiB). Baseline at `9475dfa`: 168 passed, 0 failed, 2 ignored.

**Verdict: Cut 1 closes on mechanism.** Soul could not falsify any claim the
batch made. The resolver rewrite is correct on every shape it could build,
memoisation is sound and pinned, the fsck path holds against a real
smart-HTTPS origin, and the digests are unchanged across five real targets
with 1,975 entries hash-compared against `ls-tree` and zero mismatches. What
survives is a bill, not a blocker.

- **S5-1, medium. S4-1 is half fixed, and the line that costs the time is
  dead.** `drivers.rs:6039-6042` calls `canonicalize()` per path component.
  The budget charges distinct symlinks read and never charges components
  walked, so one link with k real components costs roughly O(k^3): 7.35 ms at
  k=50, 255 ms at k=200, **14.92 s at k=800**, extrapolating to about four
  minutes at the `PATH_MAX` ceiling. Through the production walk, depth 400 x
  40 links took **78.6 s**. `frame.resolved` is already canonical there, so
  the call cannot change the answer: replacing the four lines with
  `stack[top].resolved = candidate;` leaves 168/168 green. **This restores
  S4-1's own failure scenario at a lower exponent** - a push adding 40
  symlinks with long targets wedges the scheduler thread, twice.
- **S5-2, medium. The traversal budget is the resolver's only termination
  guarantee and nothing tests it.** `MAX_LINK_TRAVERSALS` to `u32::MAX` leaves
  168/168 green. `cargo mutants --in-diff` found 16 mutants, 11 caught, 3
  unviable, **2 missed, both on `:5963`**, each making the budget never
  decrease. Cycles are refused *only* by the budget - memoisation cannot help,
  because in-progress frames are never inserted. One character turns a
  two-link cycle in a bound repository into an unbounded loop on the scheduler
  thread.
- **S5-3, low-medium. The digest is blind to symlink targets.** Deleting the
  target from the hash at `:6205` leaves 168/168 green. Since `observe_frozen`
  compares `frozen_source_sha256` against the receipt, repointing every
  symlink in a frozen tree to another in-root path passes both containment and
  the digest. S4-2's test has one regular file and no symlink.
- **S5-4, low. The digest is blind to the executable bit.** Deleting the
  `0o111` term leaves 168/168 green, and `validate_frozen_source` accepts both
  `0444` and `0555`, so a frozen file can be made executable undetected.
- **S5-5, low. S4-6 landed with no test, and the batch note called it
  landed.** Both `ensure!`s replaced with `true` leave 168/168 green. The
  mechanism does hold - a 64 MiB blob at mode `120000` is refused with VmHWM
  unchanged - but the threshold is off by one: 4095 bytes is admitted by the
  guard, written, then refused downstream.
- **S5-6, low-medium. A legitimate target through more than 39 symlinked
  components is now refused**, where `f540d50` accepted it. The old recursion
  charged each sibling depth 1, so the ceiling never fired for that shape.
  Fail-closed, undeclared, and the error names no link.
- **S5-7, low. `resolve()` is still unpinned and has no test at all.** Soul
  settled the mechanism on a real rig - self-signed CA, `git-http-backend`
  behind TLS, a validated binding, a clean `--filter=blob:none` clone, then
  `main` moved onto a hostile commit. As shipped: clean admitted, duplicate
  trees refused, `.GIT` refused. With the flag deleted: both admitted. **The
  gap is a test gap, not a mechanism gap.**
- **S5-8, informational. Four containment checks where one suffices**, each
  surviving deletion alone because the loop tail covers all three arms; only
  `:6021-6024` earns its place. `FrozenSymlinkFrame::symlink_path` is an
  `Option` only ever constructed as `Some`, and its doc comment says the
  opposite.
- **S5-9. 940 lines, about 400 nameable as deletion.** The 189-line
  `resolve_style_fetch_then_freeze_exact_refuses_hostile_trees` asserts
  nothing today: its only assertion sits in an `Ok(_)` arm that pass 4 showed
  is never taken, and a newer test runs the same seven fixtures plus four more
  through a harder unguarded fetch. About 190 further lines are boilerplate
  six module-level helpers would collapse.

**Promises that held, with numbers.** S4-1's headline (n=20 chain 43,700 ms to
instant, and it freezes); S4-2's zero-buffer and first-chunk-only mutants both
killed; all three of S4-3's guards killed by isolating tests; S4-4's explicit
fsck and its own fetch flag killed. Memoisation is sound: making the memo
never hit kills the n=20 test, and no stale answer was found by probe or by
reading. Seven of the eight earlier link shapes reproduce identically. The
budget is per call and that is fine: 31,200 symlinks validate in 8.65 s,
linear at about 11 ms per link.

**FU-Heimdall-Gitlink has cleared upstream.** Heimdall froze end to end:
`e0618f7f`, 10.07 s, 945 entries, 0 mismatches; `vendor/CultLib @ b6b1d9c6`
serves again. No Idunn change caused the break or the fix. **The ops follow-up
closes.**

### Self's rulings for the fifth fix batch, 2026-09-22

- **R-I1 (S5-1). Delete `drivers.rs:6039-6042`.** Soul proved the call cannot
  change the answer and that deleting it leaves the suite green. This is not
  an optimisation, it is removing dead code that costs cubic time. **Then pin
  the cost**: a test that freezes a link with a long component chain and fails
  if it is not linear-ish, so the term cannot come back unnoticed.
- **R-I2 (S5-2). The budget gets the two fixtures Soul specified**: a
  two-link cycle that must refuse, and 41 distinct links that must refuse.
  Both must die under their own mutation - a budget that never decreases must
  turn them red. cargo-mutants missed both mutants on that line, which is
  exactly why a hand check belongs here.
- **R-I3 (S5-3, S5-4). The digest covers everything a tamper could change**:
  symlink targets and the executable bit, each with a test that fails when its
  term is removed. A digest that is the tamper check must not be blind to a
  field the validator permits to vary.
- **R-I4 (S5-5). S4-6 gets its test, and the off-by-one is fixed** so the
  guard refuses exactly what the kernel refuses. The batch note is corrected:
  "landed" must not mean "landed unpinned".
- **R-I5 (S5-6). Keep the 40-traversal ceiling** - it matches the kernel's own
  `ELOOP` limit, and a fail-closed refusal at that boundary is the right
  behaviour. **But declare it**: the limit is documented, and the error names
  the link and the count it reached. My earlier ruling said "keep S6's
  behaviour exactly"; this is a deliberate, narrow departure from it, recorded
  as such rather than left as drift.
- **R-I6 (S5-7). Commit the rig.** It buys the only coverage `resolve()` has
  and runs in 1.5 s. It **skips** rather than fails when `openssl` or
  `git-http-backend` is missing, since neither is guaranteed on a workstation.
- **R-I7 (S5-8, S5-9). Take the deletion.** The three redundant containment
  checks, the dead `Option` and its wrong comment, the 189-line test that
  asserts nothing, and the boilerplate the six helpers collapse. **Delete the
  dead test rather than repairing it** - a newer test already covers its
  fixtures through a harder path.

### The fifth fix batch landed, 2026-09-22

Sonnet, three commits on `idunn/cut1-fix5` (`f52cefc`, `0b2ac95`, `87daa06`).
**175 passed, 0 failed, 2 ignored** on Yggdrasil (168 baseline, +8 new, −1
deleted). `src/drivers.rs` only: 988 insertions, 486 deletions.

- **R-I1.** The dead per-component `canonicalize()` is gone. Pinned by a test
  asserting roughly linear cost in component count; restoring the call takes
  four times the components from 11 ms to 359 ms, about 33×, and the test goes
  red.
- **R-I2.** Both budget fixtures added. Under the mutation that makes the
  budget never decrease, the 41-link fixture goes red — and **the two-link
  cycle hangs rather than failing an assertion.** That is the correct
  falsification for this rule: nothing but the budget bounds a cycle, so a
  timeout is exactly what a broken budget produces.
- **R-I3.** Digest tests for the symlink target and the executable bit; each
  goes red when its term is deleted.
- **R-I4.** Off-by-one fixed to `<`. Verified against the container's real
  kernel: a 4095-byte target writes, 4096 fails `ENAMETOOLONG`. The guard now
  refuses exactly what the kernel refuses.
- **R-I5.** The 40-traversal ceiling is documented as matching Linux's
  `MAXSYMLINKS`, and the error names the link and the count it reached.
- **R-I6.** The HTTPS rig is committed, runs in about a second, and skips
  cleanly where `openssl` or `git-http-backend` is missing. Deleting
  `transfer.fsckObjects=true` from `resolve()`'s fetch turns it red on the
  first hostile fixture — `resolve()` now has coverage where it had none.
- **R-I7.** The dead `Option` and its backwards comment are gone, the
  189-line test that asserted nothing is deleted, and the hostile-tree
  boilerplate is collapsed into module-level helpers.

**Two findings that came back the other way, both worth more than the batch.**

**S5-8 was wrong, and my ruling repeated it.** Soul called three containment
checks redundant and I ruled "delete them". Hands deleted two, hand-traced the
third, and found that with all three gone a symlink whose entire target is
`".."` at the root — `root/escape -> ".."` — resolves to the parent of the
root and returns `Ok`, because no later `Normal` step ever runs to trip the
other arm's check. **That is a containment escape**, the exact class this
resolver exists to prevent. Hands kept the check, added
`validate_frozen_source_symlink_refuses_a_bare_parent_reference_at_the_root`
to pin it, and recorded the deviation in code rather than quietly keeping it.
Deleting the surviving check makes that test fail exactly as predicted. **Both
Soul and Self were wrong here; a Hands hand-trace caught it.**

**Soul's rig was not recoverable.** The brief said to reuse
`soul-idunn05-notes.md`, `probe5_https2.rs` and `s5-https-run.sh`. None
survived: no session, no Yggdrasil work directory, no scratch trace. Hands
rebuilt the rig from this map's description of it, which worked only because
the description was detailed. **A probe that is going to be committed must be
handed over while it still exists**, not left in a scratchpad to be
reconstructed from prose.

**Scope tradeoffs Hands named:** three commits rather than seven, because
R-I1, R-I5 and R-I7's resolver edits sit inside one ~120-line function and
splitting them by hand was the riskier option; and the boilerplate collapse
touched only the three tests tied to the deleted one, leaving about fourteen
other pre-existing sites alone to bound risk. Both judgements accepted.

## Soul pass 6 on Cut 1's fifth fix batch, 2026-09-22

Opus. Counts reproduced independently four times on Yggdrasil: 175 passed, 0
failed, 2 ignored (the 2 are unrelated `control_plane.rs` live-store tests).
13 hand-built mutations, 6 of them not plain reverts.

**Verdict: five of seven rulings hold under attack. R-I1's pin and R-I3's
coverage promise are not delivered.** The containment argument — where the
pass was told to be most suspicious — survived everything Soul could invent.

- **F1, CONFIRMED, medium. R-I1's pin is a wall-clock assertion wearing a
  ratio's clothes.** `drivers.rs:11636-11642` reads
  `large < small.max(0.01) * 20.0`. In all eight runs `small` was 2.7–9.4 ms,
  always under the 10 ms floor, so the clamp fires and the test is simply
  `large < 200 ms`. Clean code measured 63.2 … **198.9 ms**, the last with
  three concurrent suites in one container — which is exactly the three-slot
  Yggdrasil now in use. **A loaded slot turns a clean revision red**, and
  under Cut 4 that is a false `verify` verdict on unattended work. The other
  direction is worse: with the `canonicalize` restored Soul measured
  324.9–377.3 ms, a separation of **1.63×, not the 33× Hands reported**.
  Hands' 11 ms / 359 ms pair straddled the clamp boundary, which is what made
  the ratio look decisive. A host 1.7× faster and the mutant survives.
- **F2, CONFIRMED, medium, and it is a privilege finding.** R-I3 is not met:
  **setuid, setgid, sticky and gid are all fields the validator permits to
  vary and the digest does not cover.** The validator masks `& 0o777`, so
  `0o4555` satisfies `== 0o555`, and it checks `uid() == 0` but never gid.
  The digest hashes only `mode & 0o111 != 0` for files and nothing for
  directories. Measured on a real root-owned 0555 tree: setuid file `04555`
  accepted, digest unchanged; setgid file, setgid dir, sticky dir, gid 12345
  — all the same. `observe_frozen` is exactly these two calls, so **a frozen
  source whose binary has been made setuid-root re-observes clean against its
  receipt**, and the tree is bind-mounted into runner containers with no
  `nosuid` while the workload runs as `65532:65532`. `harden_frozen_source_tree`
  normalises modes at write time, so this is post-write tamper — which is
  precisely what the digest exists to catch.
- **F3, CONFIRMED, low-medium. The cycle fixture's hang is real and nothing
  bounds it.** Under the budget mutation, `timeout -s KILL 90` returned 137.
  `cargo test` has no per-test timeout and the stopgap has no wall-clock cap;
  each traversal pushes a frame, so it grows toward the 12 GiB cap. Hands'
  argument that the hang is the correct falsification stands — **the missing
  piece is a wall-clock cap on the verify runner, which belongs to Cut 4**,
  not a change to this test. Separately the fixture asserts only `is_err()`,
  not which error.
- **F4, PLAUSIBLE, informational.** "Refuses exactly what the kernel refuses"
  is slightly overstated. `libc::PATH_MAX` is per-target and `< PATH_MAX`
  matches the VFS rule on all of them, so the constant is not merely correct
  in this container — the platform question answers itself. But the ceiling
  that actually applies is the filesystem's: an ext4 slow symlink is bounded
  by block size, so on a 1 KiB-block filesystem the kernel refuses targets
  this guard admits. Doc accuracy, not a defect.
- **F5, informational, and it runs the opposite way to S5-8.** Two
  containment checks remain where one suffices, and deleting the **Normal**
  arm's leaves the suite and a 20-shape escape sweep green — because
  `frame.resolved` mutates only inside the match and the tail check runs after
  every iteration. **Deleting the tail check alone turns the bare-`..` test
  red.** If anyone revisits this, `:6105` is the one that must survive. Two
  further mutants are genuinely equivalent: component-wise `starts_with`
  versus string-prefix (every escape transits the root's parent first), and
  dropping `ensure!(target.is_relative())` (`frozen_symlink_steps` bails on
  `Component::RootDir` regardless).
- **F6, informational, outside the batch.** `digest_tree` — the **artifact**
  digest — still has the exact blindness R-I3 just fixed for frozen source:
  no executable-bit term, no `\0` after the symlink target. No ruling covered
  it, so it must not be assumed fixed by association. Also: **no test calls
  `observe_frozen` at all**; its tamper check is exercised only through its
  two constituents.

**Promises that held, with numbers.** R-I1's *behaviour* claim confirmed by
reading plus call-site check: both production entries discard the resolved
path, so removal cannot change an answer any consumer sees. R-I2's 41-link
fixture dies under two different mutations, including moving the ceiling to
400 — the constant is pinned, not just the direction. R-I3's symlink-target
and exec-bit tests each die under their own term's deletion. R-I4's test dies
under the reverted comparison. R-I5's message names the link and yields 41
where it fires. **R-I6's rig ran, it did not skip**: 1.09 s, and deleting
`transfer.fsckObjects=true` from `resolve()`'s own fetch turns it red, so the
refusal is genuinely fsck's. Since `validate` requires an `https://` origin,
an HTTPS rig is structurally the only way `resolve()` can be covered — the
"only coverage" claim is correct. R-I7's deleted 189-line test had exactly
the first seven of the eleven fixtures that replaced it; nothing real was
lost, and the collapse is faithful at every site.

**Containment, built by Soul rather than inherited.** Twenty shapes — bare
`..`, `../..`, `./..`, `../.`, `././././..`, real-dir-then-two-up,
nonexistent-then-two-up, three-up from depth 2, absolute target, absolute
in-root target, link-to-escaping-link, link through an escaping dir-link,
out-and-back-by-name, sibling-root prefix confusion, memoised-then-escape,
dangling-then-escape, plus four that must be accepted. **All correct. No
escape.**

**Did the addition buy its keep?** Production 7900 → 7947 lines, and the logic
is net-negative; the +47 is R-I5 and R-I7 justification comments. Tests 4472 →
4927 net, about 830 gross against −189 dead test and −190 boilerplate. Yes.

**What Soul could not run:** no non-Linux check of R-I4 (the path is
`#[cfg(unix)]` and the other arms bail, so F4 is reasoning rather than
measurement); the container runs as root, so F2's probe got its
"root-owned" precondition free and **did not prove a non-root attacker can
reach it**; and no end-to-end `observe_frozen` against a real receipt, because
none exists and building a `CompiledDeploymentPlan` was out of budget.

### Self's rulings for the sixth fix batch, 2026-09-22

- **R-I8 (F1). Replace the timing pin with a deterministic one.** Count the
  `canonicalize`/`symlink_metadata` calls through a test-only counter and
  assert the count, not the clock. A wall-clock assertion on a host that runs
  three jobs at once is a coin toss in both directions, and Cut 4 turns a
  false red into a false `verify` verdict on unattended work. **Hands'
  reported 33× was an artifact of the clamp**, not a measurement — that goes
  in the record, because the number is what made the pin look adequate.
- **R-I9 (F2). A frozen source may not carry setuid, setgid or sticky at
  all.** The validator refuses them outright rather than masking them away;
  nothing in a frozen source has any business with them. **The digest covers
  the full mode and the owning gid**, not `& 0o111`, and covers directories
  as well as files. Each term gets a test that fails when it is removed.
  Additionally, **the bind mount gets `nosuid`** — defence in depth, since the
  digest is a detection and the mount flag is a prevention, and the cost is a
  mount option.
- **R-I10 (F3). The wall-clock cap is Cut 4's**, recorded as a requirement on
  the verify runner: no step runs unbounded. It is not a reason to weaken this
  fixture. The fixture does gain an assertion on **which** error it got.
- **R-I11 (F4). Soften the claim to what is true**: the guard bounds the
  buffer at `PATH_MAX` and the filesystem may refuse less. Doc only.
- **R-I12 (F5). Record that `:6105` must survive**, in a comment at the check
  itself. The two equivalent mutants are recorded as equivalent. No deletion.
- **R-I13 (F6). `digest_tree` gets the same treatment as the frozen-source
  digest**, with its own tests, and **`observe_frozen` gets an end-to-end test
  against a real receipt**. A tamper check with no test of its own is the
  thing this campaign keeps finding.

### The sixth fix batch landed, 2026-09-22

Sonnet, two commits on `idunn/cut1-fix5` (`f873b54`, `9e572bd`).
**183 passed, 0 failed, 2 ignored** (175 + 8). `src/drivers.rs` only: 701
insertions, 41 deletions, no new dependencies.

- **R-I8.** The wall-clock pin is gone. A `#[cfg(test)]` thread-local counter
  records every `canonicalize()`/`symlink_metadata()` call inside the
  resolver, and the test asserts an exact formula (`components + 2`) rather
  than a clock. Deleting one probe call site turns it red: 52 expected
  against 1 measured. **This is what the pin should have been from the
  start** — deterministic under any load, where the old one was a coin toss
  on a three-slot host.
- **R-I9, parts 1 and 2.** The validator's mask widens `0o777` → `0o7777`, so
  setuid, setgid and sticky are refused outright instead of masked away. The
  digest hashes the full mode and the gid, for directories as well as files.
  Five new tests, each killing its own mutation.
  **A real bug surfaced while writing them:** the container's `chown` silently
  clears a freshly-set setuid bit even when the owner does not change, so the
  fixture had to chmod last. A fixture that sets the bit and then chowns is
  testing nothing, and would have looked like a pass.
- **R-I10.** The cycle fixture now asserts the error names the 40-traversal
  ceiling rather than only `is_err()`. Rewording the message turns it red.
- **R-I11.** Doc softened to what is true: the guard bounds at `PATH_MAX` and
  a filesystem may refuse less.
- **R-I12.** Comments record that the loop-tail check is the one that must
  survive, that the Normal arm's is redundant, and that the two mutants Soul
  identified are equivalent. No deletion, as ordered.
- **R-I13.** `digest_tree` gains an executable-bit term, pinned and
  mutation-killed. **The symlink-target terminator is deliberately not
  pinned**: a real encoding collision needs a NUL inside a symlink target,
  which `symlink(2)` cannot produce. Hands built a two-name boundary-shift
  fixture by hand, confirmed it does not go red under the term's removal, and
  **removed it rather than keep a test that lies about what it proves.** That
  is the correct call. `observe_frozen` now has a real end-to-end test through
  `compile_deployment_plan` and `freeze_exact` against a real recipe and
  binding: it accepts an intact receipt and refuses a post-freeze tamper.

**R-I9's third part was wrong, and Hands proved it against the real tool.**
I ruled the bind mount should carry `nosuid`. Docker exposes no such
per-bind-mount flag through either syntax — `--mount type=bind,…,nosuid` and
`-v host:dst:nosuid` are both rejected outright. The container already runs
with `--security-opt no-new-privileges`, which is the standard mitigation for
this class and is process-wide rather than per-mount, so it is stronger than
what I asked for. **The ruling is withdrawn**; the prevention was already
there and I had not checked before ordering it.

**A second fixture that would have lied.** The first `observe_frozen` test
tampered with the recipe file, which a *different* check inside
`observe_frozen` also catches — so it passed while the digest check was
deleted. Hands found this by running the mutation, not by reading, and fixed
the fixture to tamper where only the digest can see it. **A test that goes
green for the wrong reason is the failure this campaign keeps producing**, and
it is caught only by mutating against the final spelling of the code.

## Soul pass 7 on Cut 1's sixth fix batch, 2026-09-23

Opus. 183 passed / 0 failed / 2 ignored, reproduced twice on Yggdrasil. 12
hand mutations, 9 not plain reverts, plus a privilege-surface probe on a real
root-owned 0555 tree.

**Verdict: four of six rulings hold. What this pass found is three pieces of
verification that do not verify** — a pin blind to its own regression, a test
that cannot fail, and a digest that got half its ruling.

- **F1, CONFIRMED, medium. R-I8 traded a flaky pin for a deterministic blind
  one.** The counter at `:11748` fires only where
  `record_frozen_symlink_probe_call()` was hand-placed. Soul **restored the
  original S5-1 bug** — the per-component `canonicalize()` this whole line of
  rulings exists to keep out — **without instrumenting it. The test passed, in
  0.39 s.** The count stays at 52/202 because the reintroduced syscall is not
  counted. Hands' "deleting one probe call site turns it red" mutates the
  instrumentation, not the code under test. **The batch record claimed the
  regression "cannot come back unnoticed"; the test's own doc comment, four
  lines below, says it "cannot catch its own regression by call count alone".
  The code was honest and the record was not.** The formula itself is right —
  `(symlinks opened) + (Normal steps walked)`, asserted only against its own
  fixture, and a memoised revisit reduces it — so it is not a false red the
  way the clock was. It is simply blind.
- **F2, CONFIRMED, medium. The test the record says was deleted for lying is
  in the tree, still lying.**
  `digest_artifact_differs_across_a_symlink_target_boundary_shift` (`:12292`)
  landed in `9e572bd`. Deleting the terminator it names leaves **the entire
  suite green**. It cannot fail: the name is already NUL-terminated before the
  target, so the two spellings differ whatever happens after it. Hands'
  underlying reasoning was sound and Soul tested it — `symlink("a\0b")` and a
  NUL in a filename both fail at `InvalidInput` before the kernel, and the
  same closure covers path components, so the collision class really is
  unreachable. **The gap is honest; the fixture built on top of it is not, and
  it sits in the slot where this map says a fixture was removed for exactly
  this.**
- **F3, CONFIRMED, low-medium, privilege. A file capability is invisible to
  both the validator and the digest.** On a real root-owned 0555 tree,
  `security.capability` set to `cap_setuid=ep`: validator **ACCEPT**, digest
  **unchanged**. That is the grant setuid used to be, with no bit for
  `mode & 0o7777` to see. Any other xattr behaves the same; mtime and
  hard-link count are also invisible and harmless, since content is hashed.
  **What holds the line is not the digest** — both the runner and the systemd
  workload require `no-new-privileges`, which neuters file capabilities
  exactly as it neuters setuid, the same mitigation cited when the `nosuid`
  ruling was withdrawn. So this is a **detection** gap, not a live escalation
  on the deployment path, and it matters wherever anything else on the host
  executes that tree. Under Cut 4 it means a `verify` verdict of "this is the
  tree we froze" that is false for xattrs.
- **F4, CONFIRMED, medium. R-I13's digest got half its ruling.** The ruling
  said `digest_tree` gets the same treatment as the frozen-source digest,
  which now hashes `mode & 0o7777` and gid for files and directories.
  `digest_tree` got **a single executable boolean**. Measured blind: setuid,
  setgid, uid, gid, and the read bits (`0o500` hashes as `0o555`); directories
  get no mode term at all. This is the **artifact** digest, re-derived against
  the sealed receipt immediately before launch, so **a post-install tamper
  adding setuid to an installed binary re-verifies clean.**
- **F5, CONFIRMED, low. R-I9's "each term gets a test" holds for the digest,
  not the validator.** Narrowing the file mask to `0o5777` (blind to setgid on
  a file) or the directory mask to `0o3777` (blind to setuid on a directory)
  each leave 183 passing. **Only setuid-on-a-file is pinned.** A future edit
  narrowing either mask readmits setgid or sticky with a green suite, and
  setgid on a directory is the one with real semantics.
- **F6, informational.** `nix_group_or_skip` (`:12248`) never skips — it
  returns `Ok(1)` unconditionally, and its doc's reasoning about `/etc/group`
  is moot because `chown(2)` never consults it. Three tests now hard-require
  root. Not a new class, but the name says something the function does not do.

**Promises that held.** **R-I13's `observe_frozen` fixture fix is real**:
neutering the digest comparison turns the new end-to-end test red, so the
second fixture does tamper where only the digest can see it. **R-I9 verified
on a real filesystem rather than by reading the mask**: setuid, setgid and
sticky on a file are each refused by name and each move the digest; a foreign
gid is accepted by the validator, which still checks only uid, and now moves
the digest. R-I10 holds. R-I11 and R-I12 match what Soul could re-derive, and
the `:6105`-class check is correctly identified.

**What Soul could not run.** No non-Linux arm. **No non-root attacker model** —
the container is root, which gave F3's `setxattr` and every root-owned
precondition free; nothing here shows an unprivileged process can reach any of
these tampers. No cargo-mutants sweep, because two other campaigns held slots,
so the budget went to twelve targeted hand mutations instead.

**Rig parked at `F:\Projects\eureka-rigs\idunn-cut1-soul7\`** — outside the
repo and the scratchpad: `privilege-surface-probe.rs`, `mutations.py` (the
twelve, self-restoring), `README.md`.

**Soul's judgement: Cut 1 is not done, but what is left is small and named.**
The mechanism closed two passes ago and nothing built against it has moved.
"If a seventh batch turns up the same shape again — a green test that proves
nothing — that is a signal about how this file is being tested, not about the
code."

### Self's rulings for the seventh fix batch, 2026-09-23

- **R-I14 (F1). Make the count total rather than conventional.** Put
  `canonicalize` and `symlink_metadata` behind a **narrow injected port**, so
  the resolver cannot reach the filesystem except through the counted path.
  Then restoring the S5-1 bug raises the count and the pin fires. Doctrine
  already asks for this shape — inputs as narrow ports, mockable probes — and
  a counter that only counts the calls someone remembered to annotate is an
  honour system. **Also correct this map**: it claimed the regression could
  not return unnoticed, which was false when written.
- **R-I15 (F2). Delete the boundary-shift test.** Not repair it — it cannot
  fail, and the class it names is unreachable. The production comment saying
  the term is not independently pinned is the true one; the test's doc comment
  is the false one and goes with it. **The map's claim that this fixture was
  already deleted is corrected above.**
- **R-I16 (F4). `digest_tree` gets the ruling it was actually given**: full
  `mode & 0o7777` and gid, for directories as well as files, each term pinned
  by a test that fails when the term is removed. A tamper check re-derived
  immediately before launch must not be blind to the bit that grants
  privilege.
- **R-I17 (F5). Each validator mask term gets its own test** — setgid and
  sticky on a file, setuid, setgid and sticky on a directory — so narrowing
  the mask cannot pass.
- **R-I18 (F3). The digest covers extended attributes**, names and values, in
  a defined order. `no-new-privileges` is a **prevention on our own deployment
  path**; the digest is the **detection**, and Cut 4's verdict asserts "this
  is the tree we froze", which is false today for any xattr. Record plainly
  that the probe ran as root and does **not** establish that an unprivileged
  attacker can set one.
- **R-I19 (F6).** `nix_group_or_skip` either skips or is renamed for what it
  does. A helper whose name promises a skip it never performs will mislead the
  next reader.
