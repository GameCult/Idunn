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
1. **`gjallar`** — first. Quiet repository, one systemd unit, no compose
   indirection. It is the cheapest proof that the systemd-transient workload
   driver works end to end on this host.
2. **`ghostlight`** — **blocked, not merely later.** A world-elaboration and
   ontology rebuild is running in it right now: commits landed 2026-09-05 across
   178 branches, with work parked on `codex/ghostlight-dungeon-mvp`. Adding
   `deployment/idunn/recipe.toml` to that tree collides with live work. Migrate
   it when the rebuild lands, and coordinate rather than assuming.
3. **`heimdall`**, **`repixelizer`**, **`streampixels`** — the
   simple ones. `heimdall` and `repixelizer` currently restart via
   `docker compose`, so they are the first real test of a compose-shaped
   workload under the systemd-transient driver.
4. **`bifrost-persona-feedback`** — the only target that carries signed-release
   authority. Migrate it *after* at least three ref-head targets work, because
   it is the one that exercises `selection = "signed-release"`.
5. **`epiphany`** and **`epiphany-capstone-17`** — two targets sharing a
   repository. Prove the binding-per-target model here.
6. **`voidbot`** — deployed, live retrieval path, 5.7 GB of state. Not early.
7. **`odin`** — last of the targets. Idunn reads Odin's topology to gate
   promotion, so migrating Odin changes the thing that gates the migrations.
8. **Idunn itself** — the installed unit and binary. See *Cutting over the
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

Read `CodexConnector/deployment/idunn/recipe.toml` before writing a new one. It
is the richest worked example — it carries `[[external_inputs]]` pinned by
sha256, a non-Rust runner, and a typed credential-store schema — and since that
target is being retired rather than migrated, it is a reference with no live
claim on it. `Odin`'s is the minimal example.

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
