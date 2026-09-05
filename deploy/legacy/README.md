# Legacy actuator — the generation currently installed on yggdrasil

**These files are superseded. Do not use them for a new installation.** They are
kept because they are what is *running*, and a migration needs both sides.

`idunn-yggdrasil` is a root shell actuator reached through the narrow sudo grant
in `idunn-yggdrasil.sudoers`. It hardcodes eleven targets, their repositories
and their admitted refs, then delegates to executable manifests under
`/srv/odin/deploy-manifests/<target>`. It calls `idunn validate-release-authority`
and `idunn validate-minimum-source-revision` — subcommands the current binary
does not have.

## What replaced it

Everything this script does by convention, the current Idunn does by contract:

| Legacy mechanism | Current owner |
|---|---|
| `deployment_target_policy()` — a `case` over eleven target names | one operator binding per target, in `--bindings-dir` |
| `upstream_ref=refs/heads/...` in shell | `[repository] admitted_ref` |
| `minimum_source_revision_required` | `[repository] minimum_revision` |
| `requires_bifrost_authority` + `--release-authority-store` | `[repository] selection = "signed-release"`, per target |
| `/srv/odin/deploy-manifests/<target>` shell scripts | recipe `[[steps]]` in the target's own repository |
| `run_manifest_while_holding_authority_locks` | the deployment transaction and process write-lease |
| `flock` on a brake store | `[brakes] deployment_store` and `lifecycle_store` |
| root shell with `IDUNN_ACTUATOR=1` | `systemd-transient` workload driver under `DynamicUser` |

The structural difference is not tidiness. The legacy path admits a deployment
by *validating arguments to a root shell script*, and everything after that
validation is unverified shell running as root. The current path seals exact
source and artifacts, admits one incarnation against typed operator authority,
and hands execution to systemd — with no place to pass a command string,
enforced by `cli_exposes_only_declarative_commands`.

## The target inventory, which is still useful

This is the real migration scope, read out of `deployment_target_policy()`:

| Target | Repository | Admitted ref | Notes |
|---|---|---|---|
| `odin` | GameCult/Odin | `codex/ygg-idunn-independent-bootstrap` | ref is a branch, not main |
| `codex-connector` | GameCult/CodexConnector | `codex/ghostlight-release-binding` | minimum revision enforced |
| `ghostlight` | GameCult/Ghostlight | `codex/ghostlight-dungeon-mvp` | minimum revision enforced |
| `voidbot` | GameCult/VoidBot | `main` | |
| `heimdall` | GameCult/Heimdall | `main` | restart is `docker compose restart` |
| `epiphany` | GameCult/Epiphany | `codex/epiphany-shakedown-live` | |
| `epiphany-capstone-17` | GameCult/Epiphany | `codex/epiphany-shakedown-live` | second target, same repo |
| `bifrost-persona-feedback` | GameCult/Bifrost | `main` | **only** target requiring Bifrost release authority |
| `repixelizer` | GameCult/repixelizer | `main` | |
| `streampixels` | GameCult/StreamPixels | `main` | two units, one target |
| `gjallar` | GameCult/Gjallar | `codex/yggdrasil-aggregate-daemon` | |

Seven of eleven are pinned to a non-`main` branch. That is its own problem — the
swarm goal is everyone on `main` and current with each other — and migrating a
target is the moment to fix its ref, not to copy it forward.

## When these files die

When every target above has a binding and a recipe, and `idunn serve` owns
actuation on yggdrasil. At that point delete the sudoers grant first, then the
script, then `/srv/odin/deploy-manifests/`. The grant is the privileged half; it
should not outlive its consumer.
