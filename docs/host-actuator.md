# Host actuator: Idunn on a Windows workstation

Idunn on yggdrasil is a Linux control plane whose every driver is a local
actuator: git, docker, `systemd-run`, nginx, a file lock. It has no remote
execution. Muninn runs on Raven, a Windows workstation, inside the logged-on
user's desktop session because it captures that desktop and loops its audio.
Windows `sshd` spawns in session 0, so every remote path into the desktop
session is a hop through a scheduled task, and that hop is where deaths hide.

This document is the authority map for the cut that lets Idunn manage a
target on a host it cannot actuate locally. It is written before the code and
maintained with it.

## Objective

A Muninn deployment to Raven is one Idunn transaction: the same brake, the
same sealed release, the same Expected/activation/presence chain, the same
continuity, as odin on yggdrasil. Nothing on Raven is started by an operator
running a script, and a dead `muninn serve` is seen the tick it dies.

## Authority map

- **Owner:** the yggdrasil Idunn daemon stays the single deployment and
  continuity authority. It seals, brakes, publishes Expected, issues
  activations, admits Odin's correlation, fences, leases and retires. The
  host actuator decides nothing.
- **Host actuator (`idunn-host`):** one process per managed Windows host,
  running in the user session, started at logon by one scheduled task. It
  dials Idunn over CultNet RUDP on the WireGuard mesh and executes the
  consequences of two ports, runner and workload, for that host. It holds
  the host's service identity and signs every report with it. It keeps no
  transaction state; a restarted actuator re-adopts a running workload by
  pid, creation time and executable digest.
- **Inputs to the actuator:** signed requests from Idunn carrying the
  compiled plan, sealed release, Expected, activation and its credential.
  The exact source is fetched by the actuator from the origin at the exact
  revision Idunn froze; the plan's recipe bytes must match the tree.
- **Outputs from the actuator:** `SealedRelease`, installed root, and a
  `HostWorkloadObservation` (pid, creation time, executable path and
  sha256, command line sha256, environment names, runtime bundle, session
  id, user sid, exit code once exited), each signed by the host identity.
- **Transport:** the actuator dials; the host opens no inbound port and
  holds no credential for yggdrasil. Idunn binds its actuator hub on the
  mesh address only. Confidentiality is WireGuard's; authenticity is the
  two service identities (Idunn's, already enrolled; one new profile for
  host actuators). A request Idunn did not sign is dropped; a report the
  bound host anchor did not sign is dropped.
- **Derived state:** the actuator's own logs, the scheduled task's state,
  and Task Manager are evidence, never admission.
- **Forbidden writers:** `restart-muninn.ps1` and its VBS/`schtasks`
  launcher no longer start `serve` on a host with an actuator. The
  actuator install unregisters the Muninn serve task. Operator scripts on
  other Muninn hosts (starfire, nightwing) are untouched until those hosts
  get an actuator.
- **Shared paths:** deploy, continuity restart, and cancel all reach Raven
  through the same two ports the systemd and docker drivers implement.
  There is no second launch path.
- **Deletion line:** `WorkloadDriver` and `RunnerDriver` as single-variant
  enums are gone; the driver is the tag of the binding. The observation is
  a variant per workload kind, not a systemd struct that a Windows process
  would have to counterfeit. The Muninn serve scheduled task, its VBS and
  cmd launchers, and `Register-HiddenVbsTask` for serve leave
  `restart-muninn.ps1` once Raven is admitted.

## Current mechanism versus intended

| Concern | Today | After |
|---|---|---|
| Runner | `DockerRunnerDriver`, concrete field on `Engine` | `RunnerPort` chosen per plan from the binding: docker on yggdrasil, `host-native` through the actuator |
| Workload | `SystemdTransientWorkloadDriver`, concrete field | `WorkloadPort` chosen per plan: `systemd-transient` or `host-actuator` |
| Observation | 47-field systemd/`/proc` struct | `WorkloadObservation::{Systemd, Host}`, untagged on the wire so persisted records decode unchanged |
| Isolation proof | uid, pid ns, mount ns | per variant: Linux as before; host by pid and creation time, which a workstation cannot fake for two live processes |
| Liveness | `systemctl show` per tick | `Observe` request per tick over the session; a closed session is an error, and the error is "dead" |
| Route | nginx stream fragment | none for Muninn: consumers discover its endpoint through Odin's advertisement |
| Presence | Muninn publishes none | Muninn publishes a dual-signed `gamecult.runtime_presence_health.v2` to Odin from the runtime bundle and activation credential the actuator hands it |

## Invariants that must hold across the cut

1. A persisted transaction or admitted generation written by the previous
   Idunn decodes unchanged. Verified by decoding a systemd observation
   encoded alone as the enum.
2. No path on yggdrasil can start, stop or observe a host workload except
   through the actuator session bound to that host's anchor.
3. A host with no attached actuator is a failed observation, which
   continuity counts and gives up on after three attempts, exactly like a
   unit that will not start.
4. The actuator cannot be told to run a program the recipe did not declare;
   `allowed_programs` on the host-native runner is enforced on the host.

## What the cut taught, same day

Each of these was found by running the pipe against Raven, not by reading.

- **Fixed host resources cannot overlap.** Muninn owns the desktop, the
  capture device and two advertised ports; candidate-then-promote failed at
  bind time. `rollout.strategy = "stop-then-start"` exists for exactly a
  route-less host-actuator workload: the admitted incumbent is stopped at
  Starting, before the candidate starts, and supervision leaves the target
  alone from Starting on. A systemd-transient workload may not declare it.
- **An unobservable host is silence.** The hub returns a typed
  `HostUnobservable` for not-attached, detached, reconnected and unanswered.
  Supervision skips that tick and logs it. Before this, an Idunn restart
  burned all three continuity attempts inside the actuator's 15-second
  reattach window and demoted a healthy Muninn.
- **The hub is serviced on its own thread.** A tick that blocked on odin's
  docker build for a minute starved every attached host into a timeout.
- **A binding path is absolute by its own shape.** The actuator re-parses
  the Linux binding on Windows; `/var/...` must not fail there.
- **The actuator's git never rewrites line endings.** The frozen tree is
  byte-exact; Windows git's autocrlf made the recipe bytes differ from the
  plan's.
- **The actuator must be rebuilt when the binding vocabulary grows.** It
  parses the same TOML the daemon does, from the same crate; an older
  actuator refuses a plan whose binding it cannot decode, which is the
  right refusal and a deploy step to remember.
- **A provider's presence sequence must survive its restart.** Odin admits
  only above the sequence it last stored for a signer; Muninn now uses the
  observation clock.
- **Odin (not this repo) died on a stale provider presence.** A continuity
  restart of raven-muninn left the previous presence in Odin's store; Odin
  treated a stored presence that no longer authenticated as tampering and
  exited, on every restart, until Idunn gave up on it. Fixed in Odin
  `aaaa8ed`: stale, logged, not fatal. Recorded here because this cut is
  what made a provider restart under Idunn a routine event.

Measured on Raven: cold build 1 m 20 s, warm 50 s; seal to admitted about
90 s including the brake release; a killed `serve` is observed on the next
tick and restarted by continuity in 6 s; reattach after an Idunn restart
about 20 s, during which the target is logged unobservable and untouched.

## Subtraction budget

Removed: `WorkloadDriver`, `RunnerDriver` enums; the Muninn serve launcher
body in `restart-muninn.ps1` and its `repair-raven-muninn-task-actions.ps1`
once Raven is admitted. Added: one binary target (`idunn-host`, Windows),
one module of wire records shared by both binaries, one identity profile,
`windows-sys` behind `cfg(windows)`. Net additive, bought by the capability
the operator ordered: a Windows host inside Idunn's authority.

## Build budget

`idunn` and `idunn-provision` are built for yggdrasil in docker
(`rust:latest`, `CARGO_TARGET_DIR=target/linux`; the test suite needs an
`/etc/machine-id` in the container) and installed with
`gamecult-ops/scripts/install-idunn-yggdrasil-release.sh`. `idunn-host`
builds on a Windows host with the stable toolchain and runs on Raven; it is
not built on yggdrasil. The library compiles on both; unix-only code is
`cfg(unix)`, and the Linux-shaped test fixtures pass on a Windows checkout
except the one parent-only-descriptor test.
