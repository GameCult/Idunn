# Deployment provenance: map

Status: map, Imagination, 2026-10-23 (campaign `idunn-deploy-provenance`,
session `self-2026-10-23-eureka-b`). The ends are in
[`deploy-provenance-target.md`](deploy-provenance-target.md). Questions,
rulings, cut specs and follow-ups live in Eureka's mind; this page holds what
has no kind: body facts, rationale and sequencing. Line numbers are at
`d74ac8d` (origin/main on 2026-10-23).

## Cut `tree-executable-members`

Spec `idunn-deploy-provenance:cut_spec:cut-tree-executable-members.r1`. It
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
- T4. `validate_release_symlink` (`:6895-6914`) keeps a symlink only if its
  target is relative with no `..` and lexically inside the release. That rule is
  sound without physical resolution (no `..` means a target can only descend).
  Absolute links and `..` links refuse the release. The frozen-source path has
  a physical resolver (`resolve_frozen_source_symlink`, tested at `:11826`) that
  admits in-tree `..`; reuse it if a real tree needs `..` links, not a new rule.
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
- T9. The host actuator builds receipts separately (`src/host.rs:1043-1081`)
  and must carry the same field and the same shape check.
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
- Symlinks: relative, no `..`, in-tree links survive (T4). Vili's trees must
  not come from `python -m venv`: its `bin/python` is an absolute link and its
  `pyvenv.cfg` names the build workspace. Install packages into the relocatable
  interpreter tree instead (`motion-pipeline:follow_up:vili-relocatable-python-tree`).

### Authority

Owner: the seal (`collect_artifact`, and `host.rs` for host targets) decides
membership, from the recipe, and writes it into the receipt. Harden enforces
the receipt and decides nothing. Runner exec bits and git `100755` modes are
dead inputs for release modes.

### Acceptance sizing

The big-tree job (spec `verification.operator`) needs about 12 GiB of scratch
on Yggdrasil: a 3.5 GiB tree, its staging copy, downloads and pip cache. It
removes it at the end. If the trees hold any absolute or `..` symlink, the job
stops and the T4 resolver reuse becomes a revision of this spec.
