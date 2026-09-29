# Legacy control-record wire fixtures

Each `*.hex` file is two lines: the CultCache envelope `schema_id`, then the
record's MessagePack payload in hex. They are the exact bytes the encoder at
Idunn `9001b58` (the base of the F0 cut) wrote for records built through that
revision's own constructors: `DeploymentTransaction::new`,
`AdmittedGeneration::from_transaction`, `SealedRelease::new`,
`compile_deployment_plan`, and `rmp_serde::to_vec`. A throwaway test on a
scratch commit over `9001b58` printed them; it is not kept.

Everything in them is synthetic: identities were enrolled fresh in a temporary
directory, the Odin receipts are placeholder bytes with correct digests, and
the plan is the `deployment_plan` test recipe. No byte comes from a live store.

| File | Schema | What it is |
|---|---|---|
| `generation-with-receipts` | `idunn.admitted_generation.v2` | routed, stateless generation with Ready and latest Odin receipts |
| `generation-route-repair-started` | `idunn.admitted_generation.v2` | the same with `route_repair_started_at_unix_millis` set |
| `transaction-fencing` .. `transaction-committing` | `idunn.deployment_transaction.v3` | one continuity transaction in each post-fencing phase |
| `transaction-committing-v2` | `idunn.deployment_transaction.v2` | the Committing record in the v2 layout: the v3 tuple without key 34 (base code stops writing v2, so this one is the v3 encoding with its trailing nil slot dropped and the header count lowered) |
| `transaction-failed-after-fencing` | `idunn.deployment_transaction.v3` | terminal post-fencing failure with complete abort evidence |

`control_plane.rs` decodes these through `LegacyDeploymentTransaction` and
`LegacyAdmittedGeneration`. The fixtures die with those structs.
