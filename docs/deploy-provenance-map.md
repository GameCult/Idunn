# Deployment provenance: map

Status: map, Imagination, 2026-10-23 (campaign `idunn-deploy-provenance`,
session `self-2026-10-23-eureka-b`). The ends are in
[`deploy-provenance-target.md`](deploy-provenance-target.md). Questions,
rulings, cut specs and follow-ups live in Eureka's mind; this page holds what
has no kind: body facts, rationale and sequencing. Line numbers in T1-T12 are
at `d74ac8d` (origin/main on 2026-10-23); T4, T9 and T13-T16 are at `fd040f0`,
the head of Hands attempt 1, which revision 2 of the spec builds on.

## Cut `tree-executable-members`

Spec `idunn-deploy-provenance:cut_spec:cut-tree-executable-members.r2`
(revision 2, after Soul pass 1 found the real interpreter tree refused). It
serves `motion-pipeline:follow_up:idunn-tree-executable-members` (Vili's
admission waits on it) and will serve gamecult-chat C3 too: the presence
launcher runs a child such as `upstream/bin/daemon` from inside a tree artifact
(README, launcher fragment), and Synapse needs a Python interpreter there.

### Body facts

- T1. `ArtifactOutput` (`src/deployment.rs:106-117`) carries one `executable`
  bool. `ArtifactReceipt` (`src/deployment_plan.rs:731-737`) copies it and
  `validate` (`:740-760`) requires it to equal the declaration.
- T2. `harden_installed_release` (`src/drivers.rs:6848-6855`) builds its
  executable set from `root.join(receipt.destination)` only. `harden_root_tree`
  (`:6858-6892`) seals every directory 0555 and every file 0444 unless that
  exact path is in the set. A directory artifact with `executable = true` is
  therefore silently meaningless today: its files are all 0444. Nothing refuses
  it. No live recipe declares one (CodexConnector, Ghostlight, Heimdall, Odin,
  StreamPixels service and web surveyed): `executable = true` is on files only.
- T3. A tree output is copied by `copy_artifact`/`copy_tree`
  (`src/drivers.rs:8053-8126`), which recreate symlinks verbatim. The digest
  (`digest_tree`, `:8142-8175`) covers names, file bytes and link targets, not
  mode bits. So runner or git exec bits never reach the release: harden is the
  only mode authority.
- T4. `validate_release_symlink` (`src/drivers.rs:6897-6916`) keeps a symlink
  only if its target is relative with no `..`. It runs only inside
  `harden_root_tree`, at install, so a tree with a `..` or absolute link copies,
  digests and seals, then fails at `install_release` (Soul s1: copying and
  digesting the real tree twice took 160 s before harden refused it). The rule
  is sound but too narrow: it refuses the real interpreter tree (T13). The
  frozen-source path has a physical resolver (`resolve_frozen_source_symlink`,
  `:6617`) that walks the filesystem with a 40-link budget and a memo. Its
  first version was exponential (Soul S4-1, recorded in `d562a12`), and it needs
  the tree on disk to answer. Revision 2 does not reuse it (T14).
- T5. The sealed release id hashes `(schema, plan_id, artifacts,
  external_inputs)` (`src/deployment_plan.rs:1000-1011`), and `plan_id` hashes
  the whole plan including `recipe_blob`. A changed member list is therefore
  already a new plan and a new release. The receipt must still carry the list,
  because `harden_installed_release` reads receipts only; its two callers
  (`install_release` `:2788`, `validate_installed_release` `:2822`) re-harden
  every installed release on each observation.
- T6. Encoding is `rmp_serde::to_vec`, positional. A trailing field with
  `#[serde(default, skip_serializing_if = "BTreeSet::is_empty")]` keeps every
  existing receipt's bytes and release id: the plan's `deadlines` field
  (`:574-577`) is the precedent.
- T7. `digest_artifact` and `digest_tree` (`:8128-8175`) `fs::read` each file
  whole. A torch CPU tree's largest file (`libtorch_cpu.so`, hundreds of MB)
  is held in Idunn's memory three times per install (seal, install, every
  validate). `hash_frozen_source_tree` (`:6788-6840`) already streams in 64 KiB
  chunks (S9). The cut collapses both onto one streaming helper.
- T8. `require_leaf_path` (`src/deployment.rs:1951-1958`) only keeps the
  artifact destination a single filename. Members are paths inside that
  destination, so it does not change. `require_relative_path` (`:1936-1949`)
  with `allow_current = false` already refuses absolute, `..`, root and prefix
  components.
- T9. The host actuator re-spells the seal (`src/host.rs:1050-1086`): copy,
  mode check, digest, pinned-digest check and receipt, the same steps as
  `collect_artifact` (`src/drivers.rs:2305-2326`). Soul s1: deleting its mode
  check survives every test. Revision 2 collapses both onto one `seal_artifact`
  in `drivers.rs`. Each caller keeps only how it finds the source path.
- T10. The transient unit runs with `ProtectSystem=strict` and the release in
  `ReadOnlyPaths` (`src/drivers.rs:3675-3698`), `/tmp` mounted `noexec`, and no
  `MemoryDenyWriteExecute`. Executing a 0555 file from the release works.
  Shared libraries need no exec bit.
- T11. Live body on Yggdrasil: `/usr/local/bin/idunn` sha256 `44cf8a14...`,
  installed 2026-10-20 02:34 CEST, before `6e2a31c` (02:40 CEST). `src/` is
  identical between `8760a4f` and `6e2a31c`, so the live code is the
  reincarnation c2 merge. `origin/main` is 26 commits ahead (+4194/-112 in
  `src/`: the presence launcher and member route). Installing this cut ships all
  of them. Idunn's own body changes only through gamecult-ops
  `scripts/install-idunn-yggdrasil-release.sh` (staged binaries, unit,
  bindings). The stage at `/home/gamecultadmin/idunn-stage` is stale (2026-09-11).
- T13. Link census of the real interpreter tree (Yggdrasil probe of
  `cpython-3.12.15+20261021-x86_64-unknown-linux-gnu-install_only.tar.gz`,
  sha256 `5ff9f060...77191124`; torch adds no links, as Soul s1 counted 1,049
  after pip): 1,049 links. 0 are absolute, 749 have no `..`, and 300 have all
  their `..` components before the first name (every one is
  `share/terminfo/<c>/<name> -> ../<c2>/<name2>`, one `..` each). 0 have a `..`
  after a name, 0 climb above the tree, 0 dangle and 0 resolve outside it.
  Blender 5.2.2 has 79 links, none absolute or with `..` (Soul s1).
- T14. Lexically normalising `<link's directory>/<target>` does not check
  containment. The kernel resolves a target one component at a time, so a `..`
  after a symlinked name climbs from where that link points, not from where its
  name sits. Probe on Yggdrasil: `root/a/s -> ..` and `root/a/L -> s/../x`
  normalise to `root` and `root/a/x`, both inside, yet `L` reads the file `x`
  beside `root`. `root/a/d -> .` with `root/a/M -> d/../../x` escapes the same
  way. Idunn has met this before: verify-transaction-cut F5 (`D -> .`,
  `L -> D/D/../../../etc/passwd` passed a lexical check).
  The sound lexical rule constrains the target's shape: all its `..` components
  come before its first name, and they number no more than the link's depth
  below the artifact's top. The climb then starts from the link's own
  directory, which the walk reached through real directories only (copy and
  digest never descend through a link), so it lands on a real ancestor inside
  the artifact. After that the target only descends, and any link it passes
  through obeys the same rule. By induction on the kernel's hops (capped at 40,
  then ELOOP), every hop stays inside. The check is one pass over the target's
  components with no filesystem reads, so its cost is linear in the tree and no
  resolver is needed. The shapes it refuses (`s/../x`) occur in neither real
  tree (T13).
  Prior art, from documentation as known to Imagination (high confidence for Go
  and securejoin, medium for the tar details):
  - Go `path/filepath.IsLocal` (1.20) is lexical and says so: it does not account
    for symlinks. It would admit `s/../x`. Used alone, it is the T14 bug.
  - Go `os.Root` (1.24) and `cyphar/filepath-securejoin` (used by runc,
    containerd and umoci) resolve each component inside the root when a path is
    opened, refusing or clamping an escape. They guard one later access, need
    the tree on disk, and check again on every open. Idunn instead certifies a
    sealed tree once, so that the kernel's own resolution stays inside on every
    later access.
  - Zip-slip defences and archive extractors check entry names lexically and
    protect against link targets by refusing to write through links
    (libarchive's `ARCHIVE_EXTRACT_SECURE_SYMLINKS`, and `SECURE_NODOTDOT` for
    names; GNU tar creates such links last). They protect the extraction, not
    what a kept link points at afterwards.
  - OCI layer extraction keeps link targets verbatim, absolute ones included,
    because the runtime resolves them inside the container's root. Idunn runs a
    release in the host's mount namespace, so absolute targets stay refused.
  - Nix keeps link targets verbatim and tracks dependencies by scanning for
    references. The whole store is its root, so it has no per-artifact
    containment to compare with.
  None of these is a static per-link rule for a tree that runs unconfined. The
  T14 shape rule fits that case.
- T15. Host targets: `idunn-host` observes, starts and stops processes on
  Windows only (the non-Windows `platform` module in `src/host.rs` bails), and
  `host.rs` sets no file modes. On Windows a file runs by its type, not by a
  mode bit, so `executable` and `executable_members` have nothing to set there.
  The file `executable` flag has always been carried and never applied on host
  targets. A recipe does not know which workload kind its binding will choose.
- T16. The release root holds only artifact destinations. Runner workspaces,
  where external inputs land, are removed before `SealedRelease::new`
  (`src/drivers.rs:2421-2423`). `install_release` (`:2781-2788`) and
  `validate_installed_release` (`:2815-2822`) digest every receipt's
  destination before calling harden. So a rule in the digest walk runs at seal,
  at install and at every validate, over every link harden would see.
- T12. Disk: `/` on Yggdrasil has 1.3 TB free. A tree release costs about three
  copies (runner workspace, staging, installed). Nothing collects old releases or
  staging (`ghostlight-verse:follow_up:idunn-generation-retention`), so every
  Vili deploy leaves about 7 GiB behind.

### Contract, corrected against the body

- Any directory artifact may declare `executable_members` (a set of relative
  paths). The follow-up limited it to runner-output; that limit protects no
  invariant, and the launcher's worktree or fetched trees need the same feature.
- `executable = true` names a file artifact. On a directory output the release
  is refused, and `executable_members` on a file output is refused (T2).
- A member is refused at recipe parse if it is empty, absolute, or holds `.`,
  `..`, a root or a prefix. At seal it is refused if it is missing, is a
  symlink, passes through a symlink or non-directory, or is not a regular file.
  Refusal happens before `SealedRelease::new`, so no sealed release can name a
  member it cannot seal.
- Harden seals each member 0555 and fails if any declared executable path was
  not visited as a regular file. "Every declared executable is sealed 0555, or
  the install fails" holds structurally, not by a lookup that can miss.
- Symlinks: a link in a tree artifact is kept when its target is relative, its
  `..` components all come before its first name, and they number no more than
  the link's depth below the artifact's top (T14). One function,
  `check_release_link`, decides this. Only the digest walk calls it, so a bad
  link refuses the release at seal, before `SealedRelease::new`, and again at
  install and validate (T16). Harden no longer judges links. The climb is
  bounded by the artifact's top, not the release root: no live release has a
  `..` link (the old rule refused them all), so nothing installed is newly
  refused, and each artifact's digest describes a closed tree.
- Host targets carry `executable` and `executable_members` in the receipt,
  shape-checked at seal by the shared `seal_artifact`, and do not apply them
  (T15). Refusing members there would make a recipe's validity depend on its
  binding.
- Vili's trees must
  not come from `python -m venv`: its `bin/python` is an absolute link and its
  `pyvenv.cfg` names the build workspace. Install packages into the relocatable
  interpreter tree instead (`motion-pipeline:follow_up:vili-relocatable-python-tree`).

### Authority

Owner: the seal (`seal_artifact`, called by `collect_artifact` for systemd
targets and by `materialize` in `host.rs` for host targets) decides
membership from the recipe and writes it into the receipt. Harden enforces the
receipt and decides nothing. Runner exec bits and git `100755` modes are dead
inputs for release modes. Link containment belongs to `check_release_link`,
which only `digest_tree` calls. `validate_release_symlink` is deleted, and
harden's symlink arm leaves links as they are.

### Acceptance sizing

The big-tree job (spec `verification.operator`) needs about 12 GiB of scratch
on Yggdrasil: a 3.5 GiB tree, its staging copy, downloads and pip cache. It
removes it at the end. Revision 2 runs it on the trees as shipped, with the 300
terminfo links (T13). If a tree holds a link the T14 rule refuses, the job
prints it and stops, and the link is reported to Self, not deleted.

## Cut `isolation-credential-evidence`

Spec `idunn-deploy-provenance:cut_spec:cut-cut-isolation-credential-evidence.r1`,
at `112a7a5` (origin/main, live on Yggdrasil since 2026-10-24 11:29 UTC). It
serves Vili's admission (`motion-pipeline:cut_report:cut-vili-admission.h4`,
first deviation): every `idunn up vili` is refused before fencing.

### Body facts (probed read-only on Yggdrasil, 2026-10-24)

- I1. `idunn up vili` runs 6 and 7 failed with
  `systemd workload isolation properties differ from the Idunn contract`, the
  message of `observe_unit` (`src/drivers.rs:2975-3005`). Of its conjuncts,
  only one differs on the failed Vili unit: `LoadCredential`. Idunn expects
  `""` because Vili has no service credential besides the presence identity
  (`service_credential_names` empty); systemd shows `[unprintable]`. Every
  other conjunct matches, InaccessiblePaths included (one value across all
  five idunn-vili/odin units).
- I2. systemd 257.13 `systemctl show -p LoadCredential` prints
  `LoadCredential=[unprintable]` for every unit, whatever the credentials:
  `ssh.service` and `systemd-journald.service` (not DynamicUser, zero
  LoadCredential), the four failed `idunn-vili-*` units (zero), and the Odin
  unit (one). The display form carries no information, so the conjunct was
  vacuous whenever credentials existed and always false when none did. Odin
  and StreamPixels passed only because each carries a regular credential.
- I3. The exact value is readable: D-Bus
  `org.freedesktop.systemd1.Service.LoadCredential` (`busctl get-property`)
  answers `a(ss) 0` for the Vili units and ssh, `a(ss) 1
  "ODIN_TOPOLOGY_IDENTITY" "<path>"` for Odin.
- I4. What the workload can actually read is the delivered directory. On the
  host, `/run/credentials/` holds a directory only for units with credentials
  (getty, journald, Odin, StreamPixels service and web). Inside Odin's MainPID
  root, `/run/credentials/<unit>/` holds exactly `ODIN_TOPOLOGY_IDENTITY`
  (`-r--r-----+ root root`). The presence identity is a parent-only `OpenFile`
  descriptor, never a credential.
- I5. The rule arrived whole in `1fc0dad` (2026-09-03, "Rebuild Idunn
  deployment transaction authority"), with its twin clause in
  `validate_launch_observation` (`:3468-3472`, `:3513-3516`); no later commit
  touched it. No ruling or finding in any campaign concerns workload isolation
  verification; gamecult-chat's foreign-workloads cut touched
  `CREDENTIALS_DIRECTORY` reservation, not this check.
- I6. `SystemdWorkloadObservation` is persisted by previous Idunns in both
  MessagePack encodings, compact (positional) included
  (`persisted_systemd_observations_decode_as_the_systemd_variant`,
  `:9915-9930`), so deleting the `load_credential` field shifts every later
  slot of a live incumbent's record. The field is also in the drift list
  (`:2462`) that `stop` (`:4064-4070`) uses to refuse stopping a changed
  workload: changing what the field holds without leaving that list would make
  the next Idunn refuse to stop the live Odin and StreamPixels incumbents.
- I7. Fixtures: the verbatim 257.13 `systemctl show` output of base's 27
  properties for a Vili unit (presence identity only) and the Odin unit
  (presence identity plus one service credential), captured 2026-10-24 into
  this Imagination's scratchpad `fixtures/`.
- I8. The new rule holds for every live systemd incumbent: inside each
  MainPID root the delivered entries equal its binding's `secret_files` keys
  minus `GAMECULT_RUNTIME_PRESENCE_IDENTITY` (Odin 1, StreamPixels service 11,
  StreamPixels web 1; key names compared, no values read). An Idunn restart onto
  this cut re-observes them (`:3955-3970`) and passes.

### Decision

A property systemd cannot display does not belong in the comparison. The
invariant it reached for, that the workload holds exactly the credentials Idunn
delivered, is observed where it is decided: the entries of
`/run/credentials/<unit>` in the MainPID's root equal Idunn's service
credential names, absent directory equal to none. That covers
`LoadCredential`, `SetCredential`, `ImportCredential` and drop-ins alike, and
needs no new program. The D-Bus read (I3) is exact but sees only one of those
properties and adds a second systemd client and parser; rejected.

### Authority

Owner: `observe_unit` decides the credential rule, from the delivered
directory. `LoadCredential`'s display form is no longer read or compared; the
`load_credential` field is a retired wire slot, written empty, out of the drift
list, kept only so persisted records decode. Forbidden writers: the
`observe_unit` conjunct and the `validate_launch_observation` clause. Dropping
the slot waits on the next schema revision of the observation.

### Deploy

Idunn's body changes only through gamecult-ops
`scripts/install-idunn-yggdrasil-release.sh` (stage `idunn` only; unit and
bindings unchanged). After it, Vili's Hands reruns from step 4 of
`motion-pipeline:cut_report:cut-vili-admission.h4`'s undone list.

## Cut `runner-failure-evidence`

Spec `idunn-deploy-provenance:cut_spec:cut-runner-failure-evidence.r1`, at
`804ed75` (origin/main, live on Yggdrasil). It serves the StreamPixels-area
Self: `idunn up vili` (command `up-4866467c-ee98-4a2b-b4a9-a008d7c44acd`,
transaction `tx-a04b68c7-c742-4d7d-8220-8aa08b9febae`, 2026-10-25 00:50-00:55
UTC) failed in Sealing with `Docker runner exited with exit status: 1: ` and
nobody could tell which recipe step failed.

### Body facts (source read at `804ed75`; host read-only, 2026-10-25)

- R1. The runner is `docker run --rm` (`src/drivers.rs:2088-2090`), launched by
  `DockerRunnerDriver::docker` (`:2173-2195`) with stdin null and
  `Command::output()`: stdout and stderr are each buffered whole in the
  daemon's memory, unbounded. On a non-zero exit the error is
  `"Docker runner exited with {status}: {stderr}"`; stdout is dropped. The
  container is gone the moment it exits, so `docker ps -a` and `docker logs`
  hold nothing.
- R2. The empty message: the failing process wrote nothing to stderr, and
  stdout, the only stream that carried its report, is discarded at
  `:2182-2193`. The same interpolation echoes whatever a step does write to
  stderr into `last_error`, `pre_fencing_abort.error` and
  `completion.error` (`bounded_detail`, 2048 bytes), and from there into
  `idunn up`'s `deployment failed:` line and `idunn status`.
- R3. No step identity reaches the error. `materialize` (`:2368-2391`) loops
  over `declaration.steps` and calls `run_in_workspace` with argv only;
  `step.id` is used only in the environment precondition. External inputs
  (`curl` in the runner, `:2247-2258`) fail through the same helper with the
  same anonymous message.
- R4. Which step failed on 2026-10-25, inferred from the failed transaction's
  staging root, which survives (926 MiB,
  `/var/lib/gamecult/idunn/staging/tx-a04b68c7-.../`): in `.runner-node-build`,
  `node_modules/.package-lock.json` was written at 00:51:24 (so
  `install-vili-dependencies`, `npm ci`, finished), nothing outside
  `node_modules` is newer, and `release/vili` (the next step's output) is
  absent; the python workspace holds only fetched inputs. The failing step was
  `test-daemon` (`node --test ... scripts/test-vili-daemon.mjs`), whose test
  reporter writes to stdout: exactly the stream R1 discards. Inference from
  residue, not from output.
- R5. The failure path: `resume_one_transaction` (`src/control_plane.rs:6291-6320`)
  sends a pre-fence error to `begin_pre_fencing_abort` (`:9916-9935`), which
  persists `bounded_detail(format!("{error:#}"))`. Nothing reaches the journal
  on this path: the idunn-yggdrasil journal for 00:45-01:10 holds only
  provider-exclusion lines.
- R6. Stores. `control.cc` (root 0640) holds commands, transactions
  (`idunn.deployment_transaction.v4`, keys 0-36), generations, supervision and
  expiry requests; `ControlSnapshot::read` refuses any other type
  (`:3420`). Records must re-encode byte-exactly (`decode_record`,
  `:3717-3727`), so a new transaction field is a schema bump and a v4 lift
  (`:4100-4110`). Terminal transactions move verbatim to `history.cc`
  (`:4366-4396`; 39.8 MB on the host), which status reads for `--command`.
  `incidents.cc` lives in the published projection directory and by its own
  doc (`src/incident.rs:1-11`) carries no free text and is never `control.cc`,
  because a type added there stops continuity for every target on a rollback.
- R7. The daemon runs as root (`User=root`). Runner secrets reach the
  container as read-only file mounts named by env (`:2041-2047`), so any text
  a step derives from them can land in its output.
- R8. Staging residue: 475 transaction roots, 55 GiB under
  `/var/lib/gamecult/idunn/staging`; a failed Sealing leaves its workspaces
  because `materialize` returns before `remove_tree_inside`.

### Decision

The failure becomes a typed value at the runner, `RunnerStepFailure`: the
subject (recipe step or external input, by id), the runner id, the exit code or
signal, and the last 8 KiB of stdout and of stderr with each stream's total
byte count. Its `Display` carries the subject and status, never output.
Capture streams both pipes through bounded tails, so a verbose build no longer
buffers whole in the daemon.

The tail lives in a new private store, `runner-evidence.cc`, beside
`control.cc` (mode 0600), one record per transaction
(`idunn.runner_failure.v1`), newest 32 kept by the writer. Not in the
transaction record: that is a v5 bump whose rollback makes every older Idunn
refuse its control store (R6), the scar `incident.rs` already names. Not in
`incidents.cc`: published, and free text is barred there. Not in the journal:
journal readers are a wider set than root, so the journal line names the
transaction, the subject, the status and the store, and no output.

No redaction. Matching secret bytes would make the daemon read every secret,
and still misses any encoding, prefix or transform of one; a claimed redaction
that leaks is worse than a stated rule. The rule instead is the reader set: the
store's only readers (root) can already read every secret the runner was
given.

Keeping the failed container (dropping `--rm`) is worse: its output then lives
in Docker's json-file log, unbounded, untyped, outside Idunn's state and
readable by the docker group; containers accumulate with no owner to reap them;
`docker system prune` erases the evidence; and nothing ties a container to the
transaction in `idunn status`.

### Authority

Owner: the runner decides what failed and keeps the bounded tail; the engine's
pre-fence failure path is the one writer of `runner-evidence.cc`.
`last_error`, `pre_fencing_abort.error` and `completion.error` keep only the
failure's `Display`. Forbidden writers: the `stderr` interpolation in
`DockerRunnerDriver::docker`. Retention is by count inside the writer, so the
store needs no external owner; staging roots (R8) stay with
`ghostlight-verse:follow_up:idunn-generation-retention`.

### Deploy

Idunn's body changes only through gamecult-ops
`scripts/install-idunn-yggdrasil-release.sh` (stage `idunn` only; unit and
bindings unchanged). The next failed Sealing then shows its step and tail on
`sudo idunn status --command <id>`.
