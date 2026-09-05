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

1. **`ghostlight`** — first. It is actively developed, already has a build in
   `/srv/ghostlight`, and its failure blast radius is one service.
2. **`codex-connector`** — second. Same shape, and the census's dependency chain
   puts it before Ghostlight's consumers.
3. **`gjallar`**, **`heimdall`**, **`repixelizer`**, **`streampixels`** — the
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

`Odin/deployment/idunn/recipe.toml` is the only worked example in the estate.
Read it before writing the second one.

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
