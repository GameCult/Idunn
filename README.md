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

Run one daemon locally, with no swarm and no persistence:

```bash
./target/release/idunn --daemon demo --restart-command "echo restart demo"
```

Inspect a deployment brake without touching anything:

```bash
./target/release/idunn-provision deployment-brake-status \
  --store /var/lib/gamecult/idunn-authority/deployment-brake.cc \
  --operator-anchor /etc/gamecult/idunn/deployment-brake-operator-anchor.cc \
  --runtime-id <runtime> --release-id <sha> --deployment-id <id>
```

`--rudp-health-bind` is explicit and off by default. A bare single-daemon Idunn
opens no health ingress.

## Authority

**Owner.** Deployment admission, and the continuity of installed bodies.

**Inputs.** Its own typed state stores, its own credentials, and an
operator-provided trust anchor. Nothing else. In particular, not the state,
health, binaries or availability of anything it manages.

**The invariant.**

> Idunn must be able to start, recover, observe and report while any managed
> target is absent, broken, or deliberately braked.

A target's brake gates mutation *of that target only*. It can neither authorize
nor block Idunn's own startup, nor any other target's lifecycle.

**Two brakes, not one.** Deployment and continuity are separate authorities
with separate stores, and the daemon refuses to start if the two resolve to the
same store.

- *Continuity* is restarting an already-admitted installed release. Physiology.
- *Deployment* is changing an artifact, revision, configuration, schema, unit,
  or authority binding.

A deployment brake does not own the former. Suspending restart actuation
requires the separately named lifecycle brake.

**Forbidden.** Idunn does not depend on a managed target, and does not repair
one merely to satisfy a deploy. If recovery would require that, the target
dependency is the thing to cut.

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
`ProtectHome=yes`, `PrivateTmp=yes`, with `ReadWritePaths` enumerated per
managed target and its own authority store mounted **read-only** —
`ReadOnlyPaths=/var/lib/gamecult/idunn-authority`.

Note `Wants=docker.service`, not `Requires=`. Deployment runners use Docker;
continuity actuation does not. A hard requirement would let a Docker outage
stop Idunn itself and take daemon survival down with it. This was changed from
`Requires=` during extraction — if you are copying an older installed unit,
change it.

Adapt the store paths, `--swarm-profile`, and `--rudp-health-bind` for your
host. Every path in `ExecStart` is explicit on purpose; Idunn discovers no
authority implicitly.

## Documentation

| | |
|---|---|
| [`docs/guide.md`](docs/guide.md) | what Idunn is for, who it helps, running it, what daemons should publish, typed records |
| [`docs/authority-map.md`](docs/authority-map.md) | authority map, repository declaration, operator binding, release foundation, promotion and continuity |
| [`docs/deployment-authority.md`](docs/deployment-authority.md) | the deployment transaction in depth |
| [`docs/signed-daemon-health-authority.md`](docs/signed-daemon-health-authority.md) | signed health admission and trust bindings |

## Platform

Idunn targets Linux. `cargo test` on Windows reports **63 passed, 18 failed**;
those 18 assert against absolute deployment paths (`/srv/...`,
`/var/lib/gamecult/...`) and Rust's `Path::is_absolute()` requires a drive
prefix there. It is platform semantics, not breakage — the same 18 fail
identically in the Odin tree this was extracted from. Build and test on Linux
for a true result.

## Licence

MIT.
