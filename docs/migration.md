# Migrating a target onto the current Idunn

Status as of 2026-09-05: **no target has been migrated.** The binary installed
on yggdrasil (`/usr/local/bin/idunn`, 2026-09-02) is the previous generation,
driven by `--swarm-profile` and a root shell actuator. The current binary
rejects that invocation outright:

```
$ idunn --swarm-profile yggdrasil-local --store ...
Error: unknown Idunn command "--swarm-profile"
```

This document is the route from one to the other. It is written per target,
because the cutover is per target — there is no flag-day.

## What changes, in one paragraph

The old generation kept its target list *in Rust* (`swarm_targets()`, removed by
`330e4cd`) and its actuation *in shell* (`deploy/legacy/`). The current
generation keeps capability in a **recipe** inside each target's own repository,
and privilege in an **operator binding** on the host. Idunn seals source, builds
in a runner, admits one incarnation, warms a candidate under a distinct UID and
namespaces, fences the incumbent, promotes the route, and drains. Nothing in
that path takes a command string.

## Order

Migrate in dependency order, and migrate the authority's own dependencies last:

0. **`codex-connector` — do not migrate. Retire it.** The operator is not
   returning to Codex, so a credential-isolated transport for Codex subscription
   inference has nothing left to isolate. It is already dead in practice: the
   deployed daemon has been failing every three minutes with
   `401 ... token_expired` against `chatgpt.com/backend-api/codex/models`, with
   no established connections on its port. Migrating it would mean writing a
   binding for a corpse. See *Retiring Codex* below.
0b. **`gjallar` — do not migrate. Retired 2026-09-06.** Had a brief useful life
   on Nightwing's framebuffer, then broke and was never fixed. The Yggdrasil
   incarnation ran `--headless --refresh-hz 2` for eleven days: a compositor
   with no display, recomposing twice a second, 55 minutes of CPU and 125 MB
   resident to produce frames nothing rendered. The only reference to
   `gjallar.overview` in the estate is one Odin test file. Stopped and disabled;
   420 MB under `/srv/gjallar` is reclaimable.
1. **`heimdall`** — first. Most release-shaped of what remains, and its one
   prerequisite follows an existing estate pattern: it takes CultLib from a
   sibling checkout via `file:../CultLib`, so it needs a `vendor/CultLib`
   gitlink before its source can be sealed. See *The first three targets*.
2. **`repixelizer`** — second. No CultLib dependency at all, but runs a venv
   interpreter against a checkout, so it needs an artifact story for Python
   before a recipe means anything. Unit is `repixelizer-gui`.
3. **`streampixels`** — **blocked, like Ghostlight.** The product has changed
   direction: 2D animation could not express what the spec required, so it is
   moving to a 3D-rendered overlay, and whether that runs in WebGL or needs
   rendering infrastructure is undecided. Its deployment shape depends on that
   answer — a WebGL overlay is the current two-unit shape, rendering
   infrastructure is a new target class with hardware requirements. Do not
   migrate it, and do not "fix" the 2D animation path: it is being replaced, not
   repaired. There is a paying client, so coordinate rather than assume.

   **Their continuity is already broken, and has been.** The legacy actuator
   restarts all three with
   `docker compose -f /srv/compose/yggdrasil-apps.yaml restart <name>`, and that
   file **does not exist** — `/srv/compose` holds only `odin`, `voidbot` and
   `voidbot-retrieval`. The path is guarded by `require_root_owned_regular_path`,
   so every restart of these three has been failing at the guard. They are
   running only because nothing has asked them to restart. This is the
   difference between a supervised service and a service that happens to be up.
4. **`ghostlight`** — **blocked, not merely later.** A world-elaboration and
   ontology rebuild is running in it right now: commits landed 2026-09-05 across
   178 branches, with work parked on `codex/ghostlight-dungeon-mvp`. Adding
   `deployment/idunn/recipe.toml` to that tree collides with live work. Migrate
   it when the rebuild lands, and coordinate rather than assuming.
5. **`bifrost-persona-feedback`** — the only target that carries signed-release
   authority. Migrate it *after* at least three ref-head targets work, because
   it is the one that exercises `selection = "signed-release"`.
6. **`epiphany`** and **`epiphany-capstone-17`** — two targets sharing a
   repository. Prove the binding-per-target model here.
7. **`voidbot`** — deployed, live retrieval path, 5.7 GB of state. Not early.
8. **`odin`** — last of the targets. Idunn reads Odin's topology to gate
   promotion, so migrating Odin changes the thing that gates the migrations.
9. **Idunn itself** — the installed unit and binary. See *Cutting over the
   authority* below.

## Per target

### 1. Fix the ref before you carry it forward

Seven of eleven targets are pinned to a `codex/...` branch, not `main`. The
swarm goal is everyone on `main` and current with each other. A migration that
copies `admitted_ref = "refs/heads/codex/ghostlight-dungeon-mvp"` into a binding
has laundered a branch pin into a typed contract and made it harder to see.

Land the branch, or record in the binding's commit message why this target
cannot be on `main` yet.

### 2. Write the recipe, in the target's repository

`deployment/idunn/recipe.toml`, schema `gamecult.idunn.target_declaration.v1`.
It declares capability only — steps, artifacts, service argument shape, health
contract, state slots, provides. It must name no host path, no image, and no
privilege; `TargetDeclaration::parse` rejects unknown fields, and a recipe that
reaches for host authority is rejected by `admit()` rather than honoured.

**Several targets already have one.** Recipes exist in `Odin`, `Ghostlight`,
`Ghostlight-interruptfu`, `CodexConnector`, and the four `Ghostlight-worlds`
variants — eight in total. So recipe authoring is *not* the bottleneck for those
targets; the missing halves are the operator bindings and the daemon cutover.

`Odin/deployment/idunn/recipe.toml` is the reference. It is minimal, and
`idunn validate` accepts it.

**Do not copy `CodexConnector`'s.** It is the richest-looking example and it is
**stale**: it declares `required_gitlinks = ["vendor/cultcache-rs",
"vendor/cultnet-rs"]`, but that repository has no `.gitmodules`, no `vendor/`,
and now takes CultLib as a pinned cargo git dependency. The recipe describes a
repository shape that stopped existing.

That drift is the general hazard, not a CodexConnector quirk: a recipe lives in
the target's repo and nothing re-checks it when the repo changes underneath.
`idunn validate --recipe` catches malformed recipes; it does not yet catch a
recipe whose declared gitlinks are absent from the tree. Run it in each target's
CI so drift fails at the repo, not on the host.

### 3. Write the binding, on the host

`/etc/gamecult/idunn/bindings/<target>.toml`, schema
`gamecult.idunn.operator_binding.v2`. This is where privilege lives: origin and
admitted ref, the runner image pinned **by digest**, workload roots and
hardening, runtime trust anchor, route, brakes, rollout, placement, and the
profiles this target joins.

Two brake stores, and they must differ:

```toml
[brakes]
deployment_store = "/var/lib/gamecult/idunn-authority/<target>-deployment-brake.cc"
lifecycle_store  = "/var/lib/gamecult/idunn-authority/<target>-lifecycle-brake.cc"
```

A binding naming one store for both is rejected — *"deployment and lifecycle
brakes are identical"*. That rejection is the doctrine the old generation could
not express: a deployment brake must not be able to suspend daemon survival.

The binding must name every affordance the recipe's runners derive. Omitting one
fails `admit()`. This is deliberate — a compromised target repository can change
what gets built, but not what it may touch.

### 4. Dry-run, then admit

```bash
idunn up <target> --requested-by "$USER" --no-wait
idunn status --command <id>
```

Verify before promotion, not after: the candidate must hold a **distinct UID,
PID namespace and mount namespace** from the incumbent (`prove_isolation`), and
signed `warming` presence must be observed before the fence. If the target
cannot publish signed health yet, that is the work — not a reason to widen the
trust anchor.

### 5. Retire the legacy path for that target

Remove its line from the sudoers `Cmnd_Alias`, then its `case` arm, then its
manifest under `/srv/odin/deploy-manifests/`. Sudoers first: the grant is the
privileged half and must not outlive its consumer.

Validate with `visudo -c -f` **before** installing, and diff the resulting grant
set against a backup afterwards. Removing the last entry from a `Cmnd_Alias`
leaves a dangling `\` continuation, and repairing that by hand is how you
silently drop a `*` from a neighbouring grant and break a live target's deploy
path — as happened here on 2026-09-06 to `deploy streampixels`, caught by
diffing and not by reading.

## The first three targets, and what each needs first

Surveyed 2026-09-06. **None of the three can take a recipe as they stand.** Each
needs a change in its own repository first, and the changes are not the same
shape, so they are not one piece of work. Ordered by how much has to happen.

### `streampixels` — blocked on a product redirection, do not migrate

**Superseded 2026-09-06.** The findings below were surveyed before learning the
product is changing direction. 2D animation could not express what the spec
required; StreamPixels is moving to a **3D-rendered overlay**, and whether that
is achievable in WebGL or needs dedicated rendering infrastructure is still
open. The client funding it reportedly cannot absorb rendering overhead, which
is what makes the WebGL question load-bearing rather than a preference.

Its deployment shape is downstream of that decision. A WebGL overlay keeps
roughly the current two-unit shape; rendering infrastructure is a new target
class with hardware requirements Idunn has never expressed on this host. Writing
a binding now would pin the shape being replaced.

Keep the survey below only as a record of the current deployment's defects, and
do not act on it — in particular, do not repair the `tsx`-at-runtime path. It is
being replaced, not fixed.

Both apps already have build scripts: `apps/service` runs `tsc -p
tsconfig.json`, `apps/web` runs `next build`, and the root has `pnpm -r build`.
No submodules, no `file:` dependencies. The obstacle is not the build.

- **The service does not use its own build.** The unit runs
  `node node_modules/tsx/dist/cli.mjs apps/service/src/index.ts` — TypeScript
  transpiled at startup from a checkout at `/srv/streampixels/app`. Idunn seals
  artifacts; there is nothing sealed about a source tree plus a transpiler. Point
  the unit at the `tsc` output.
- **One legacy target is two Idunn targets.** A recipe declares a single
  `[service]`, and a binding a single `unit_prefix`. `streampixels-service` and
  `streampixels-web` have different environments, different ports, and an
  ordering dependency (`web` is `After=` `service`). They were one target only
  because the shell actuator restarted both with one `docker compose` line.
  Split them; the dependency belongs in the recipe's `[[provides]]`/dependency
  surface, not in a shared name.
- Runtime artifact question to settle: a Node service's release is `dist/` plus
  its production `node_modules`, unless it is bundled. Decide which before
  writing `[[artifacts]]`.

### `repixelizer` — needs an artifact story for Python

Simplest dependency story of the three: `pyproject.toml`, no CultLib dependency,
no submodules. But it runs `/srv/repixelizer/.venv/bin/python
/srv/repixelizer/app/scripts/run_gui.py` — an interpreter from a venv against a
checkout, so there is no artifact to seal and no revision observable from the
process. It needs a build step producing something exact (a wheel, or a venv
materialized into the release root) before a recipe means anything.

Note also the name mismatch: the target is `repixelizer`, the unit is
`repixelizer-gui`.

### `heimdall` — needs CultLib as a gitlink

Ironically the most release-shaped of the three and the one needing the deepest
change. It already deploys to `/srv/heimdall/app/releases/<sha>/app` behind a
`current` symlink, which is exactly what `release_root` expects, and it builds
with `tsc` to `dist/`.

But its `package.json` takes CultLib as
`"cultcache-ts": "file:../CultLib/packages/cultcache-ts"` — a **sibling checkout
outside the repository**. A sealed source cannot reach it, and no revision of
Heimdall pins which CultLib was used. The current deploy manifest works around
this by cloning CultLib separately and hardcoding `cultlib_commit=5cefa0db...`
in shell, which is precisely the untyped authority the recipe model exists to
replace.

The estate's answer is a vendor submodule: Ghostlight has `vendor/eve`, and
`required_gitlinks` plus the binding's `[repository.gitlinks]` is how a recipe
declares one. Heimdall needs `vendor/CultLib` as a gitlink and its `file:`
dependencies repointed at it. Then the CultLib revision is pinned by the
Heimdall commit, and the seal covers both.

Do this one last of the three, and expect it to be a real change to how Heimdall
builds rather than a deployment edit.

### What this survey says generally

Every one of these targets runs from a checkout or an unpinned sibling, and each
was fine as long as a shell script did the deploying, because a shell script can
just `cd` somewhere. The recipe model asks a question the old one never did —
*what exactly is this release, and can you name it?* — and for three of eleven
targets the honest answer today is no. That is the migration's real cost, and it
is worth paying: it is the same question as "is everyone on main and current
with each other", asked where it can be enforced.

## Why retirement forces the migration

The previous generation **cannot express that a target is retired.** Its target
list is compiled into the binary; `idunn --help` on the installed daemon offers
`restart` and `redeploy` and no way to remove a daemon from supervision. So when
`codex-connector` was stopped on 2026-09-05, the supervisor immediately began
deciding:

```
Idunn decision for yggdrasil-codex-connector: restart
  (health is failed; restart authority is available)
```

— eighty log lines in ten minutes, against a service deliberately retired. The
only brake that generation has is the **deployment** brake, and doctrine forbids
using it here: a deployment brake must not suspend continuity. Retiring a target
means suspending continuity *for that target only*, which is exactly what the
current generation's **lifecycle brake** is, and exactly what the old one lacks.

Until cutover, the available mitigation is to remove the sudo grant, so the
restart attempts fail at the privilege boundary rather than half-succeeding.
That is a compensator, not an owner, and it is worth noticing that the estate
found the argument for its own migration by trying to throw something away.

Gjallar, by contrast, was handled correctly, because its failure classified as
`dependency-unavailable` rather than `failed`:

```
Idunn decision for yggdrasil-gjallar: alarm
  (local restart/deploy is not the owner of this failure)
```

Alarm, not resurrection. The distinction the old generation *can* draw is
between failures it owns and failures it does not — not between a target that
is broken and a target that is finished.

## Host prerequisites for any routed target

Done on yggdrasil 2026-09-06. These are shared, not per-target: the first routed
migration needs them, every later one reuses them.

**nginx stream context.** Debian ships `ngx_stream_module` as a dynamic module
in a separate package, and it was not installed — `--with-stream=dynamic` in
`nginx -V` with no `.so` present. Installed `libnginx-mod-stream` at
`1.26.3-3+deb13u7`, exactly matching the running nginx, so nothing else moved.
Added a top-level `stream` block to `/etc/nginx/nginx.conf` including
`/etc/nginx/idunn-stream-routes/*.conf`, and created that directory `root:idunn
0775`. `nginx -t` passed before the reload; after it, `gamecult.org` and
`heimdall.gamecult.org/healthz` both still answered 200. Backup at
`/root/nginx.conf.bak-20260906-stream`.

Note the shape this locks in: **the vhost stays operator-owned and static.**
nginx keeps TLS, `server_name` and path routing and proxies to a stable local
endpoint; Idunn owns only the hop from that stable endpoint to whichever
candidate is admitted. Writing into `/etc/nginx/idunn-stream-routes` is the
whole of Idunn's nginx authority, and stream configuration cannot do the things
an http-context injection could.

**Idunn runs as root, and the unit says why.** This is the largest difference
between the generations and it should not be discovered at install time.

The previous generation ran `User=idunn` and reached root through a narrow
sudoers grant onto `/usr/local/libexec/idunn-yggdrasil`, a shell actuator. The
current generation has **no `sudo` anywhere in its source**: it calls
`systemctl`, `systemd-run`, `nginx` and `docker` directly, and a confined
`User=` cannot actuate anything at all. `docs/deployment-authority.md` states
the intended model — *"Freeze the exact source and recipe as `idunn`, then copy
and verify it into a root-owned actuation stage. The privileged driver never
opens Git."* — and `--source-uid`/`--source-gid` are validated as non-zero,
which only makes sense for a privileged daemon dropping down for source work.

So the separation is internal rather than at the process boundary, and the
protection is that Idunn accepts no imperative input: capability lives in the
recipe, affordances in the binding, and `cli_exposes_only_declarative_commands`
asserts there is no way to hand it a command string. The unit still runs under
`ProtectSystem=full` with each actuated path re-opened individually, so the
write set is enumerated even though the uid is root.

That is a real increase in blast radius over a confined process, and worth a
deliberate look before the cutover rather than a shrug. The counter-argument is
that the old shape was confined in name only: the process was sandboxed and the
thing it invoked was an unaudited root shell script with eleven hardcoded
targets.

**Still outstanding here:** the route driver stages preflight in
`/run/idunn/route-preflight`; the unit now declares `RuntimeDirectory=idunn`,
which creates and removes `/run/idunn` with the service.

## Two `.cc` shapes, and a retracted claim about them

**Corrected 2026-09-06.** An earlier version of this section claimed the CultLib
forks had diverged and that `cultcache-ts` could not read a `cultcache-rs`
store. **That was wrong**, and it was wrong in the usual way: generalised from a
single file.

The forks agree. `cultcache-ts` reads Idunn's live control store on yggdrasil
without complaint — 36 records — because ordinary CultCache stores use the same
framing in both runtimes: `[formatVersion, catalog, records]`. That covers the
control store, the runtime bundle's `expected.cc` and `activation.cc`, and the
process write lease.

The exception is narrow and deliberate. **Service identity private stores** are
written by `cultnet-rs`'s `atomic_create_private_store`, a minimal container
holding one bare positional envelope:

```
[ [key, type, payload, stored_at, schema_id] ]
```

That is a private-key container, not a record store, and `cultcache-ts` rejects
it — `invalid_type: expected object, received array`. Heimdall reads that one
shape in `src/idunn-store.ts`; everything else goes through the ordinary client.

The cost of getting this wrong is worth naming, because it was nearly shipped. A
consumer that hand-decodes the private-store framing and applies it to
`expected.cc` and the write lease will fail on first deployment, and its unit
tests will pass, because hand-built fixtures agree with the hand-built reader.
Build fixtures with the CultCache client so a test can disagree with you.

## Cutting over the authority

Idunn last, and not by deploying Idunn with Idunn.

1. Build the current Idunn on Linux and stage it beside the running one — do not
   overwrite `/usr/local/bin/idunn` while the old daemon is supervising.
2. Create `/etc/gamecult/idunn/bindings/` and the per-target brake stores.
3. Install `deploy/idunn-yggdrasil.service` from this repository. It invokes
   `idunn serve` and names every store explicitly.
4. Stop the old unit, move the binary into place, `daemon-reload`, start.
5. Verify continuity actually actuates — kill a managed daemon and watch it come
   back — before trusting the cutover. A control plane that starts is not a
   control plane that supervises.

**The window matters.** Between step 4's stop and a verified step 5, nothing is
supervising the swarm. Do it with the targets already migrated, so the new
daemon has bindings to load and the window is short.

## Retiring Codex

Decided by the operator 2026-09-05: the estate is not going back to Codex.
Generic token capacity comes from elsewhere, and anything *served* to third
parties needs an EU supplier with a DPA — `together.ai` is the current
direction. That makes the Codex integration a maintenance liability with no
consumer, and it is deep enough to be worth naming before anyone starts pulling.

**Live state.** `codex-connector.service` was `active (running)` and failing
`401 token_expired` on a three-minute loop against
`https://chatgpt.com/backend-api/codex/models`, with nothing connected to port
`4103`. **Stopped and disabled 2026-09-05**; the port is closed and the loop has
ended. It had consumed 1min 30s of CPU and peaked at 1.2 GB doing nothing but
retrying expired credentials.

Two loose ends that stop left behind:

- `ghostlight-dungeon.service` carries
  `Environment=GHOSTLIGHT_MODEL_CONNECTOR=127.0.0.1:4103`, which now points at
  nothing. It had logged nothing for 24 hours before the stop, so this broke
  no working path — but it is where the provider replacement lands first.
- `epiphany-model-connector.service` binds **the same port** with
  `--model gpt-5.4 --codex-home ...`. It is inactive and disabled. Two Codex
  connectors were competing for one endpoint, which is duplicate authority of
  exactly the kind the census was looking for; both are now off, and only one
  of them has a repository.

**Blast radius**, in the order it should come out:

| Surface | What it is |
|---|---|
| `codex-connector.service` on yggdrasil | running, 401-looping, no consumers |
| the sudoers `deploy`/`restart codex-connector` grants | privileged half — remove before the script |
| `GameCult/CodexConnector` | the whole repo; a transport for a provider we no longer buy |
| `Epiphany/epiphany-openai-codex-spine` | an OpenAI/Codex adapter crate |
| `Epiphany/epiphany-codex-bridge` + vendored `app-server` | Codex JSON-RPC protocol edge |
| `VoidBot` config | `turnCodexModel`, `mindCodexModel`, `imaginationCodexModel`, `codexModelReasoningEffort` and their model lists |
| `~/.codex/AGENTS.md` and per-repo `AGENTS.md` | doctrine maintained for a second agent runtime |

**Do not simply delete the Epiphany and Ghostlight side.** Those are minds that
need *an* inference transport; CodexConnector was one implementation of that
seam, and the seam is worth keeping. The correct shape is to replace the
provider behind it, not to remove the boundary and let each mind grow its own
HTTP client — that would be re-forking the wire law CodexConnector's README
exists to prevent. Retire the Codex-specific backend; decide deliberately
whether the transport daemon is rebuilt against the new supplier or whether
CultNet already covers it.

Sequencing note: `epiphany` and `epiphany-capstone-17` are on this migration
list. Do not write bindings for them until the provider question is settled —
their deployment shape depends on whether they still front a transport daemon.

## What is not carried forward

`Odin@ed48185` ("Supervise Bifrost Persona feedback deployment", 2026-07-18)
added a hardcoded `bifrost-persona-feedback` target with a `stale-deployment`
failure contract and `restart_on_missing_publication`. It never reached this
repository — it landed on Odin `main` after the extraction branch diverged, and
`330e4cd` deleted the machinery it was written against.

It is preserved at the `pre-idunn-extraction-main` tag in Odin. When
`bifrost-persona-feedback` is migrated, that commit is the specification for
what its health contract must express — re-expressed as a recipe
`[service.health]` contract, not restored as Rust.
