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

## Pre-B1 continuity aborts

The `transaction-*-abort*` files and `provider-anchor` were printed by a second
throwaway test over `9001b58`. It built a continuity transaction that issued an
activation (the recipe and binding of the `deployment_plan` tests, a provider
identity enrolled fresh, the trust-anchor path in the binding fixed at
`/tmp/idunn-b1-legacy/provider-anchor.cc`) and gave it the aborts the base code
writes: the pre-fence abort literal of `begin_pre_fencing_abort`, and the base
`post_fencing_abort_intent`. In each, the transaction issued an activation and
its abort records `topology_reconciliation = Skipped`, because the base rule
owed a continuity no projection cleanup. The workload and Odin evidence are
borrowed from `transaction-fencing`.

| File | What it is |
|---|---|
| `transaction-pre-fence-abort` | Warming continuity, pre-fencing abort in flight |
| `transaction-pre-fence-abort-terminal` | the same abort finished: Complete, `FailedBeforeFencing` |
| `transaction-post-fence-abort` | Fencing continuity, post-fencing abort in flight |
| `provider-anchor` | one line: the provider trust-anchor file bytes, hex. A test writes it to the fixed path so the Engine can read the plan's anchor |

## Generation v3 (the B2 lift)

`generation-v3-odin` and `generation-v3-route-proof` are the exact
`idunn.admitted_generation.v3` payloads the encoder at Idunn `583b2a7` wrote
(`admitted_envelope` over `rmp_serde::to_vec`), printed by a throwaway test that
is not kept. The first is the v2 `generation-with-receipts` record lifted by that
revision's own `LegacyAdmittedGeneration::into_current`, the second is the
generation `route_proof::committed_generation` admits through a real route-proof
deployment. Both are synthetic.

| File | Schema | What it is |
|---|---|---|
| `generation-v3-odin` | `idunn.admitted_generation.v3` | routed, Odin-correlated generation |
| `generation-v3-route-proof` | `idunn.admitted_generation.v3` | routed, route-proof generation, no Odin receipts |

`control_plane.rs` decodes them through `LegacyAdmittedGenerationV3`; the
fixtures die with that struct.
