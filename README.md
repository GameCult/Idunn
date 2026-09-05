# Idunn

**Deployment admission and daemon-survival authority for the GameCult swarm.**

Idunn decides two things and nothing else: whether an artifact may be
installed, and whether an already-installed body may be restarted. It keeps
daemons alive across a fleet, admits releases against operator-signed
authority, and reports typed health — while depending on none of the services
it manages.

Extracted from [Odin](https://github.com/GameCult/Odin) on 2026-09-05 with its
full 136-commit history. Idunn had lived as a crate inside the repository of a
service it manages, which inverted the authority it exists to hold.

---

## Quick start

Requires Rust 1.95+. Idunn is a **Linux daemon**; see [Platform](#platform).

```bash
git clone https://github.com/GameCult/Idunn.git
cd Idunn
cargo build --release
```

Two binaries land in `target/release/`:

| Binary | Purpose |
|---|---|
| `idunn` | the daemon — keepalive supervision, deployment actuation, health |
| `idunn-provision` | operator tooling — brake status, identity enrolment, provisioning |

Idunn's CLI is **declarative**. There are three commands and no imperative
escape hatch — you cannot hand it a shell string to run:

```bash
idunn serve  [runtime options]                      # the control plane
idunn up     <service|profile:name> [--no-wait]     # request a deployment
idunn status [--command ID]                         # read what happened
```

A test, `cli_exposes_only_declarative_commands`, asserts that
`--deploy-command`, `--restart-command` and `--swarm-profile` are **rejected**.
What a target is, how it is built, and what it is allowed to touch live in two
documents, never in a command line — see [Recipes and bindings](#recipes-and-bindings).

Request a deployment and watch it:

```bash
idunn up ghostlight --requested-by "$USER"
idunn status --command <id>
```

`idunn up` accepts a service name or `profile:<name>`; profiles are declared by
the bindings, so `idunn up profile:full-gamecult` brings up everything bound
into that profile.

## Authority

**Owner.** Deployment admission, and the continuity of installed bodies.

**Inputs.** Its own typed state stores, its own credentials, and
operator-authored trust anchors and bindings. It does not read a managed
target's binaries, health, or availability to decide anything about itself.

The one deliberate exception, stated plainly rather than hidden: Idunn reads
Odin's published topology correlation to gate **deployment** promotion, so that
a promotion waits for semantic discovery. It is read-only, an absent store
yields `None` rather than an error, and it is structurally unreachable from the
continuity path — `validate_live_providers_for_deploy` never invokes the
provider check for `CommandKind::Continuity`. Odin being down blocks
promotions. It cannot stop daemon survival.

**The invariant.**

> Idunn must be able to start, recover, observe and report while any managed
> target is absent, broken, or deliberately braked.

A target's brake gates mutation *of that target only*. It can neither authorize
nor block Idunn's own startup, nor any other target's lifecycle.

**Two brakes, not one.** Deployment and continuity are separate authorities
with separate stores, declared per target in that target's binding. A binding
naming one store for both is rejected outright — *"deployment and lifecycle
brakes are identical"*.

- *Continuity* is restarting an already-admitted installed release. Physiology.
- *Deployment* is changing an artifact, revision, configuration, schema, unit,
  or authority binding.

A deployment brake does not own the former. Suspending restart actuation
requires the separately named lifecycle brake.

**Forbidden.** Idunn does not depend on a managed target, and does not repair
one merely to satisfy a deploy. If recovery would require that, the target
dependency is the thing to cut.

## Recipes and bindings

Two documents describe a target, and the split is the security model.

The **recipe** lives in the target's own repository at
`deployment/idunn/recipe.toml` (`gamecult.idunn.target_declaration.v1`). It
declares *capability*: build and test steps, the artifacts they produce, the
service's argument shape, its health contract, its state slots, and what it
provides. It names no host paths, no images, no privileges.

The **operator binding** lives on the host under `--bindings-dir`
(`gamecult.idunn.operator_binding.v2`). It supplies everything privileged:
which origin and ref are admitted, the pinned runner image, the workload's
roots and hardening, the runtime trust anchor, the route, the two brake
stores, rollout strategy and placement. It also declares which profiles the
target belongs to.

```toml
# recipe — in the target's repo, capability only
[[steps]]
id = "build"
phase = "build"
runner = "rust-build"
argv = ["cargo", "build", "--locked", "--release", "-p", "thing"]

[service]
executable_artifact = "thing"
arguments = [
  { kind = "literal", value = "--store" },
  { kind = "binding", name = "state_root" },
]
```

```toml
# binding — on the host, privilege only
[repository]
origin = "https://github.com/GameCult/Thing.git"
admitted_ref = "refs/heads/main"
selection = "ref-head"
recipe_path = "deployment/idunn/recipe.toml"

[brakes]
deployment_store = "/var/lib/gamecult/idunn-authority/thing-deployment-brake.cc"
lifecycle_store  = "/var/lib/gamecult/idunn-authority/thing-lifecycle-brake.cc"
```

A recipe cannot grant itself an affordance: the binding must name every
affordance the recipe's runners derive, and `admit()` rejects a binding that
omits one. Unknown fields are rejected on both sides. A repository that gets
compromised can therefore change what is built, but not what it is allowed to
touch on the host.

## Contracts

Idunn's typed contracts are published by
[CultLib](https://github.com/GameCult/CultLib) in `cultnet-rs` — deployment and
lifecycle brakes, service identity, runtime activation, expected incarnation,
and the provider-health family. Idunn consumes them and defines no private
copies.

State is CultCache (`.cc` typed documents) throughout. Health is published over
CultNet RUDP.

Odin keeps its own separate projection of Idunn state for aggregation. That is
the all-seer's view, not Idunn's contract, and the two sets do not overlap.

## Running as a service

[`deploy/`](deploy/) carries a worked systemd installation:

| File | |
|---|---|
| `idunn-yggdrasil.service` | the unit |
| `idunn-yggdrasil` | operator wrapper |
| `idunn-yggdrasil.sudoers` | the narrow sudo grant the wrapper needs |

The unit runs as a dedicated `idunn` user under `ProtectSystem=full`,
`ProtectHome=yes`, `PrivateTmp=yes`. `ReadWritePaths` is Idunn's own state plus
the build root; the operator anchor, the bindings, and Odin's correlation store
are all `ReadOnlyPaths`. Idunn admits against operator authority — it does not
author it.

Note `Wants=docker.service`, not `Requires=`. Only the build step is
containerized; warm, fence, promote, drain and restart run on systemd, and
worktree-tree recipes skip Docker entirely. A hard requirement would let a
Docker outage stop Idunn itself and take daemon survival down with it.

Adapt the store paths for your host. Every path in `ExecStart` is explicit on
purpose; Idunn discovers no authority implicitly.

## Documentation

| | |
|---|---|
| [`docs/migration.md`](docs/migration.md) | **start here if you run a swarm today** — moving a target from the previous generation, in order, and cutting over the authority itself |
| [`docs/authority-map.md`](docs/authority-map.md) | authority map, repository declaration, operator binding, release foundation, promotion and continuity |
| [`docs/deployment-authority.md`](docs/deployment-authority.md) | the deployment transaction in depth |
| [`docs/signed-daemon-health-authority.md`](docs/signed-daemon-health-authority.md) | signed health admission and trust bindings |
| [`docs/guide.md`](docs/guide.md) | the **previous** generation's keepalive daemon — still what runs on yggdrasil, kept for the migration; its commands no longer work |
| [`deploy/legacy/`](deploy/legacy/) | the shell actuator that generation used, and what replaced each part of it |

## Platform

Idunn targets Linux. `cargo test` on Windows reports **63 passed, 18 failed**;
those 18 assert against absolute deployment paths (`/srv/...`,
`/var/lib/gamecult/...`) and Rust's `Path::is_absolute()` requires a drive
prefix there. It is platform semantics, not breakage — the same 18 fail
identically in the Odin tree this was extracted from. Build and test on Linux
for a true result.

## Licence

MIT.
