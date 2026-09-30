# Watchdog reporting: target

Status: target, Self, 2026-09-30. This is the first campaign whose state is
typed in Eureka's mind (campaign `idunn-watchdog`). Questions, rulings, cut
specs and their verification live there. This document keeps the rationale
and the design truths the campaign produces. The cut map's body facts and
model page will live in `watchdog-reporting-map.md`.

## Why

The operator ruled Odin's store-recovery Q-R4 on 2026-09-30 (Odin
`docs/store-recovery-cut.md`, `cdc57bd`). A third set-aside inside ten minutes
stops Odin writing while it keeps serving. The operator added:

> Yep, and Idunn needs better reporting capabilities so it can serve as
> watchdog. I ought to get a message about this on Discord if it ever happens.

and, on where delivery lives:

> Bifrost is indeed intended to be the bridge to external services like Discord

Today nobody would hear about it. Idunn already detects several conditions an
operator must act on, and for each one it writes a single `eprintln!` line to
journald (`report_once`, `control_plane.rs:4599`):

- A continuity restart budget is exhausted (6 per hour; `restarts_exhausted`,
  decided at `control_plane.rs:5709`). Idunn stops restarting the target, and
  the only trace is one log line.
- An admitted route is degraded. It fails its stable-route challenge
  (`supervise_admitted_route`, ~5973). The degraded mark is durable, but no
  one is told.
- A post-fencing phase deadline expires into `OperatorRequired`. This lives on
  B5, which is unmerged on `hands/b5`. The failure record is durable and the
  notice is a log line.

Odin's own store condition (`odin.store_condition.v1`, `contested`) is mapped
but not built. Idunn reads nothing about Odin's store health today.

The delivery path half exists in Bifrost:

- `tools/operator-notification.mjs publish-idunn-alarm` builds a
  `bifrost.bridge.discord_post_command.v1` `discord-dm` command.
- `tools/cultmesh-bridge-commands.mjs process` turns such commands into DMs,
  writing a `discord_post_receipt.v1` for each.

But no pump runs anywhere, and the bot token file on Yggdrasil is absent
(gamecult-ops `inventory.md` ~740). Idunn has never called the publisher. So
the machine has a detector with no record, and a deliverer that nothing feeds.

## Design truths this campaign produces

- **Idunn decides what is an incident.** Idunn owns daemon survival, so the
  judgement "an operator must look at this" is its judgement, made where the
  condition is decided. An incident is typed state in Idunn's store before any
  delivery is attempted. A stderr line is a trace of the record, not the
  record itself.
- **Bifrost delivers.** Discord transport, the bot token, the recipient
  binding and the delivery receipt belong to Bifrost. Idunn holds no Discord
  credential and speaks no Discord API.
- **Survival never waits on delivery.** Bifrost is a managed target like any
  other. Its absence, failure or backlog may delay a message. It never delays
  or gates an Idunn decision, tick, restart, or deployment. An incident nobody
  has delivered stays visible in Idunn's own state and in `idunn status`.
- **One message per incident, not per tick.** A fault that lasts hours
  produces one opening notice, not a stream of them. Repeated delivery
  attempts for one incident are idempotent at the recipient's end.
- **Nothing sensitive leaves.** A notice names the target, the condition and
  the time. It never carries a binding value, a path to a secret, a token, or
  a line of configuration.

## Not this campaign

- Odin's store-recovery cuts, O2 included. They produce `odin.store_condition`
  and are Odin's campaign, still in prose. This campaign consumes the record
  once it exists.
- Idunn's B5 phase deadlines. B5 is in prose on `hands/b5`. This campaign
  reports its `OperatorRequired` outcome once B5 merges.
- VoidBot's `notify_owner` MCP tool and its separate DM code.
- Bifrost's Persona delivery crossing.
- Eve/TUI lowering of incidents.
