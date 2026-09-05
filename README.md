# Idunn

GameCult's deployment admission and daemon-survival authority.

Extracted from [Odin](https://github.com/GameCult/Odin) on 2026-09-05 with its
full history (136 commits). Idunn had lived as `crates/idunn-daemon` inside the
repository of a service it manages, which inverted the authority it exists to
hold.

## Authority

**Owner.** Idunn owns deployment admission and the continuity of installed
bodies. It decides whether an artifact may be installed and whether an
already-admitted body may be restarted.

**Inputs.** Its own typed state stores, its own credentials, and an
operator-provided trust anchor. Nothing else.

**Invariant.** *Idunn must be able to start, recover, observe and report while
any managed target is absent, broken, or deliberately braked.* A target's brake
gates mutation of that target only; it can neither authorize nor block Idunn's
own startup, nor another target's lifecycle.

**Two brakes, not one.** Deployment and continuity are separate authorities
with separate stores, and the daemon refuses to start if they resolve to the
same store. Restarting an already-admitted installed release is continuity
physiology. Changing an artifact, revision, configuration, schema, unit or
authority binding is deployment. A deployment brake does not own the former.

**Forbidden.** Idunn does not depend on any managed target. It does not repair
a target merely to satisfy a deploy; if recovery would require that, the target
dependency is the thing to cut.

## Contracts

Idunn's typed contracts are published by
[CultLib](https://github.com/GameCult/CultLib) in `cultnet-rs` — the deployment
and lifecycle brakes, service identity, runtime activation, expected
incarnation, and the provider-health family. Idunn consumes them; it does not
define its own copies.

Odin holds its own separate projection of Idunn state for aggregation, in
`odin-core`. That is Odin's view as the all-seer, not Idunn's contract, and the
two sets do not overlap.

## Binaries

- `idunn` — the daemon.
- `idunn-provision` — operator provisioning and brake inspection.

## Building

```
cargo build
cargo test
```

**Idunn is a Linux daemon.** On Windows, 18 of 81 tests fail because they
assert against absolute deployment paths (`/srv/...`, `/var/lib/gamecult/...`)
and Rust's `Path::is_absolute()` requires a drive prefix there. That is
platform semantics, not breakage; the same 18 fail identically in the Odin tree
this was extracted from. Build and test on Linux for a true result.
