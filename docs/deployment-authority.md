# Idunn Deployment Authority

Idunn is GameCult's deployment, runtime-admission, continuity, and future swarm
control plane. It decides which exact workload incarnation may act. It does not
replace systemd, container runtimes, nginx, or a future general-purpose
orchestrator.

The architectural test is simple:

- Where and how a workload runs is generic infrastructure work. Use the
  configured systemd, container, proxy, or later Nomad/Kubernetes driver.
- Which GameCult incarnation has authority to serve, write state, or represent
  an admitted workload is Idunn work.

Idunn and Odin are closely integrated but do not share authority. Idunn owns
the admitted physical swarm. Odin owns the discoverable semantic topology of
the Verse. A service owns its signed runtime presence, capabilities, capacity,
and health. CultMesh carries typed projections between them.

## Authority map

- **Owner:** Idunn alone selects an exact source, admits its repository recipe
  under an operator binding, records the resulting artifact and process
  provenance, grants a process-bound state-write lease, separately admits
  route membership, and keeps the admitted incarnation alive.
- **Inputs:** root-admitted operator bindings; source-owned strict deployment
  declarations; exact Git objects read as the `idunn` identity; signed
  deployment or lifecycle-brake state; signed incarnation-bound service
  presence and health; Odin's observed capability topology when graph changes
  require semantic discovery; and observations from the configured workload
  and route drivers.
- **Outputs:** typed CultCache deployment plans and receipts; an Idunn-owned
  expected-fleet CultMesh projection; process-bound write grants; admitted
  route membership; deployment and continuity decisions; and explicit
  expected/present/ready disagreement records.
- **Derived state:** release dashboards, operator summaries, route displays,
  capacity summaries, and Odin's correlation views are projections. A target
  count, proxy backend, service-manager unit, container label, or configured
  endpoint cannot establish admission by itself.
- **Forbidden writers:** target repositories cannot grant themselves users,
  secrets, mounts, network, capabilities, resources, host paths, routes,
  placement, source refs, or rollout policy. Odin cannot promote a process or
  grant state writes. nginx cannot choose membership. systemd and container
  runtimes cannot choose the admitted incarnation. Service configuration cannot
  impersonate observed presence or readiness. Persisted shell strings and
  repository-supplied root programs are not executable authority.
- **Shared paths:** `idunn up`, scheduled deployment, manual deployment,
  scaling, restart, host reboot recovery, and crash recovery use the same
  source-to-recipe-to-artifact provenance and the same distinct write-lease and
  route-membership primitives.
  Deployment may change that chain only while the deployment stop admits it
  (absent or clear; an engaged stop halts). Continuity may restart only the already-admitted chain and
  is governed by a separate lifecycle brake.
- **Deletion line:** hard-coded target copies, raw `deploy_command` and
  `restart_command` state, `sh -c` actuation, root Git inspection, staged
  `/srv/odin/deploy-manifests`, target-specific gamecult-ops deploy programs,
  duplicate target units, and stop-incumbent-before-probe rollouts must cease to
  decide Ghostlight or CodexConnector deployment before either target is
  promoted through this design.

## The three truths Odin exposes

For each workload incarnation, Odin keeps three distinct observations:

1. **Expected** — Idunn has sealed this exact incarnation and has published its
   plan and release identities, admitted runtime and signer identities, health
   and optional canonical typed-MessagePack state-contract identities,
   endpoints, declared capabilities, and dependency role.
2. **Present** — Idunn has published the launch-scoped activation only after its
   actuator observed the exact runtime instance, and that instance has
   established its own matching signed CultNet/CultMesh runtime session.
3. **Ready** — the observed incarnation satisfies its signed capability and
   health contract and its required dependency graph is ready.

Correlation never repairs disagreement. Incarnation, runtime identity, endpoint,
schema, capability, and dependency mismatches remain visible and block
promotion. Configuration contributes only expected state; it cannot supply
present or ready state.

Every Idunn-managed deployment has an Odin-visible expected projection before
dependent services are promoted, and an Odin-observed runtime projection before
they are considered ready.

### Projection keying

The three truths are per incarnation, and so is the projection. Every
Expected, activation, and write-lease record Idunn publishes is keyed by
`{target}@{expected projection sha256}`; only the runtime presence trust
anchor is keyed by target, because it is provider lookup material shared by
every incarnation. Odin keys its presence and correlation records the same way
and reads Idunn's projection by that key.

A target being replaced therefore has two incarnations projected side by side:
the admitted incumbent under its key, the sealed candidate under its own.
Publishing the candidate cannot change what the incumbent reads about itself,
withdrawing a failed candidate removes only the candidate's records, and a
correlation Odin holds about one incarnation is never mistaken for evidence
about another. A record of those three types keyed by the bare target is the
shape of an earlier single-slot projection; Idunn retires such records the
first time it publishes for that target, and Odin never reads them.

## Source-owned deployment declaration

Each deployable repository publishes one small, reviewable declaration at a
fixed path such as `deployment/idunn/recipe.toml`. Unknown fields are rejected.
The declaration contains no host commands or privileges. It describes:

- schema and target identity;
- requested runner profiles;
- build and test argv executed inside those runners;
- pinned generic HTTPS inputs and immutable artifact outputs;
- a constrained launch contract naming the packaged executable, literal
  arguments, operator-bound argument slots, and required environment names;
- one health contract and, only for stateful services, a state/schema contract;
- capabilities provided, including schema and compatibility information;
- bootstrap, required, optional, shared-infrastructure, private, and external
  operator-bound capability dependencies;
- startup ordering, conflicts, migration conditions, and minimum capacity when
  they matter.

Dependencies resolve by typed capability/provider compatibility. A declaration
names a daemon only when identity itself is the contract; ordinary dependencies
must not hard-code a provider instance.

Runtime health uses the shared `warming`, `active`, `degraded`, and `failed`
ontology. A recipe names only its health contract. It cannot redefine those
states, readiness, or operator-facing detail. Bootstrap dependencies must be
available before process start; the remaining dependency kinds retain their
typed intent without making this foundation a scheduler.

The declaration may run repository code only inside a runner sandbox admitted
by the operator binding. It cannot publish a host path, Docker argument,
systemd property, nginx fragment, UID, secret path, or capability grant. Idunn
deterministically lowers the constrained launch contract through the configured
workload driver; repository-owned raw unit or container-runtime text is not an
input.

For systemd `DynamicUser` workloads, systemd's reported primary user and group
names are allocated runtime identities. When a binding declares no fixed state
group, Idunn validates that the process primary UID and GID match instead of
requiring an empty `Group` property. A declared state group remains an exact
name and numeric GID contract because it grants the process its bounded write
lease.

## Operator binding

An operator binding is static root-admitted configuration, not observed state.
For one target on one fleet it owns:

- canonical repository origin, admitted ref, and minimum revision;
- exact runner kind, image digest, and affordances for each requested profile;
- source, cache, output, release, and runtime roots, plus state roots and
  transition policy only when the recipe declares state;
- preconfigured runner network profiles, mounts, identities, secrets,
  capabilities, resource bounds, and device access;
- workload driver, transient-unit namespace, private endpoint range, and stable
  route;
- runtime identity, expected signer identity, and trust-anchor store;
- a process-write-lease record only when the recipe declares process-bound
  writable state; its sibling lock path is derived rather than configured;
- route driver and its root-owned configuration destination;
- rollout, drain, irreversible-cut, and retention policy;
- one admitted replica on one named node in v1; later placement and scaling are
  delegated to an established orchestrator driver;
- externally supplied capability bindings.

A recipe request that has no compatible admitted binding fails closed. A
binding can grant less than requested; it cannot silently substitute a
different semantic capability.

`gamecult-ops` publishes these fleet bindings and provisions host substrate. It
does not own application build or deployment programs.

## Static release

A target whose product is a sealed tree, with no process to run (a built web
front end), declares `[static_release] artifact = "<id>"` in place of
`[service]`, and its binding declares `driver = "static-release"` with a
`release_root`, a `current_link` and one `[workload.carry]`. It is the same
transaction as any deployment, in three phases: Sealing (source, build, seal,
install: unchanged), Starting (the one external change, behind the deployment
brake) and Complete. It never enters Warming through Committing, has no
Expected projection, no activation and no workload, publishes nothing to
Odin, and its Complete creates no admitted generation, so continuity never
supervises it. It declares no state, capabilities, dependencies, conflicts,
route or write lease, and its rollout is `candidate-then-promote` with at least
two retained releases.

What Starting does, in three durable steps recorded as `flip` on the
transaction: the release to name and the release the link names now are
written first; then Idunn copies the operator's carry source into
`<release_root>/<sealed release id>/<carry.destination>` (mode 0444) and
renames a fresh symlink over `current_link`; then it observes the link naming
the release and completes. A failure before the rename ends the transaction
through the ordinary pre-fencing abort with `current_link` untouched. A crash
resumes at the recorded intent and repeats the flip, which is harmless.

The carry is a copy of one per-site file (for example `config.json`) taken at
flip time. It sits beside the artifacts, never inside one, so it is outside
every sealed digest: changing the site file changes no release id, and the
sha256 of the bytes carried is provenance on the transaction
(`flip.carried_sha256`), beside the brake bytes and the requester. The
destination is a single leaf name that no artifact uses. The web server reads
`current_link/<artifact destination>` for the tree and `current_link/<carry
destination>` for the carried file (a one-time root edit of the vhost, an
`alias` for the carried file beside the root for the tree). A missing or
non-regular carry source fails the deployment before anything moves.

What `current_link` names is observed from the symlink and confirmed by the
latest Admitted static transaction's `flip.release_id`; it is stored nowhere
else. A rollback is a flip back to `flip.previous_release_id`: release
directories stay, and Idunn prunes none. The deployment stop is read exactly as for a
service, by the binding's runtime id. Release ids are per transaction, so
redeploying an unchanged tarball with a changed site file is a new release that
carries the new file, never an in-place re-carry of an existing one: the carry
is part of a release. If `current_link` is replaced by a directory or by a link
that names nothing under the release root after the flip was recorded, the
transaction keeps retrying with backoff: `idunn status --command <id>` shows the
error as "waiting on ...", operator cancel is refused (Starting, not held), and
the fix is by hand (restore the link or remove the directory); it then
completes. A bounded retry is follow-up work. A new
upstream tarball enters as a recipe `external_input` with a pinned url and
sha256, so each release is a recipe commit whose pin is its provenance.

### Rollback

`idunn rollback <target> [--state-store PATH] [--no-wait]` writes a
`rollback-` command and nothing else. The daemon freezes it into one new
transaction (`CommandKind::Rollback`) that reuses the previous release exactly:
the plan, sealed release and installed release are copied from the transaction
that flipped it, so there is no source, no build and no source cleanup, and it
creates no admitted generation. It starts at Sealing with that evidence and
reaches the same deployment stop as a deploy, which admits it unless engaged;
then `advance_static` flips, with the same three durable steps, the same
nothing-aborts-after-the-rename rule and the same stuck-flip behavior.

The previous release is derived, never stored: the newest Admitted static
transaction for the target (control.cc and history.cc) names the release
`current` must point at (`flip.release_id`) and the one it displaced
(`flip.previous_release_id`); the newest transaction that flipped that release
supplies the evidence. A rollback of a rollback therefore flips forward again.
The command is refused, recorded as a rejected transaction, when the target has
no completed static release, when the newest release is the first one, when
`current` does not name the newest flip, when the previous release directory is
gone, or when an artifact in it no longer digests to its sealed receipt. A
failed check at flip time ends the transaction before fencing with `current`
untouched.

The rollback reuses the old plan verbatim, so its binding snapshot, brake path
and carry source are the old ones: if the operator moved the site file the
rollback fails before the link moves. The carry is re-applied from the site file
as it is at rollback time, and its sha256 is recorded in the rollback's own
flip evidence (it may differ from the original release's; the copy inside the
rolled-back-to release is replaced). Nothing prunes release directories.

`CommandKind::Rollback` is stored in the existing v5 command and transaction
records with no schema change; an Idunn at 9f15cee or earlier cannot decode
them, so this is forward-only, like v5 itself. The newest flip is ordered by
`flipped_at` wall clock, and the observed-link check makes a clock step refuse
the rollback rather than mis-flip. A rollback cannot be cancelled, like a static
deploy. A rollback is also refused when the previous release was deployed under
a different release root or current link than the newest one.

## Typed deployment state

Mutable control-plane state will be CultCache. Runtime integration must use
typed records for:

- requested service or profile;
- resolved capability graph and exact providers;
- exact source and recipe digest;
- runner binding and artifact digests;
- full materialization receipts for every pinned external input and artifact;
- candidate runtime identity, private endpoint, and signed runtime health;
- launch-scoped runtime activation, published as current only after workload-
  driver observation;
- process-bound write lease;
- independently admitted route membership;
- expected, present, and ready correlation;
- promotion, drain, failure, and continuity results.

Old records containing raw commands are readable history only. No executor may
interpret their command text.

State transition is also typed authority. The repository may package a
migration artifact and declare its accepted source schemas, destination schema,
and literal/operator-bound argv. The operator binding admits the state roots,
backup root, and whether this rollout permits migration or a fresh-root cut.
Idunn alone issues a fenced one-shot migration grant bound to the exact source,
artifact, state schema generation, and deployment incarnation. A target
migration never receives general deployment or lifecycle authority.

The full operator binding, compiled plan, source facts, and sealed release are
private Idunn control-plane state. They contain host paths and operational
bindings and are never published as Odin topology. Only the sanitized
`ExpectedIncarnation` projection is shaped for CultMesh publication. It is
derived only from a sealed release validated against its compiled plan and
names both content addresses and the executable artifact digest. Present and
Ready remain Odin-owned correlations over independently verified runtime
evidence; the deployment-plan module deliberately does not model them.

## The deployment stop

`src/brake.rs` is the only reader and writer of the stop. A stop halts work
that has not crossed an irreversible step, and it is read again at each such
step: Sealing (`advance_sealing`), the static flip intent (`advance_static`) and
the entry into Fencing for a deploy (`advance_warming`). An engaged stop there
makes the transaction wait with a note naming who engaged it and why; nothing
aborts and nothing moves. After the flip intent is recorded, or once a deploy
has fenced, the transaction runs to its end: a stop halts before the
irreversible step, never mid-fence or mid-flip. Continuity is never gated.
The sequence of a stop record is positive and advances by one per verb; nothing
reads it for authority, so it is not validated further.

Absent means the store file is missing in an existing directory. Anything else
Idunn cannot judge (a symlink, a zero-length or undecodable file, another
runtime's record, another owner, an unsearchable or non-directory parent) reads
as unreadable and fails closed; the wait note carries the
`idunn-provision deployment-brake-clear` command that replaces it.

A binding may name a second stop, the target's steward:

```toml
[brakes.steward]
user = "mind-streampixels"            # a login, never root
store = "/var/lib/gamecult/idunn-steward/streampixels/deployment-brake.cc"
```

Deploys admit only when every stop admits. The steward's store is read as owned
by that user's uid: the owner uid proves who wrote the file, and the wait note
names the stop by the binding's steward user, never by the record's `by` (which
is the verb's own text). Idunn never writes the steward directory and no key,
group or ACL is involved. The operator's store is a separate stop the steward
cannot clear, and the wait note names each holding stop.

The steward owns its directory, so it could unlink any file in it. Binding
validation therefore refuses a steward store whose directory is shared with,
inside or around the directory of any other Idunn authority store (deployment
and lifecycle brakes, trust anchor, write-lease record, release authority).
Validation cannot see the file system: the root who provisions the directory
makes it contain the steward's store and nothing else.

A steward store owned by another uid, or a steward user that does not resolve
to a non-root uid, is unreadable and fails closed. Only the steward (the
owner uid) can repair it: a clear run as root leaves a root-owned file that is
still unreadable.

### Provisioning a steward (root, once per target)

1. Create the store directory, owned by the steward and holding nothing else:
   `install -d -o mind-streampixels -g mind-streampixels -m 0755
   /var/lib/gamecult/idunn-steward/streampixels`. It must not be, contain or sit
   inside any other authority directory, and no workload `ReadWritePaths` may
   cover it. Idunn (root) reads it without further grants; no group or ACL.
2. Add the stanza above to the target's operator binding, after `[brakes]`,
   with the steward's real login. Deploys planned afterwards see it.
3. The steward engages and clears as itself, never through root:
   `idunn-provision deployment-brake-engage --store <store> --runtime-id <id>
   --reason <text>` and `deployment-brake-clear` with the same flags.
4. The operator reads it with `idunn-provision deployment-brake-status --store
   <store> --runtime-id <id> --owner-user mind-streampixels`.

The binding is sealed into each plan. A deploy already planned keeps the
binding it was sealed with: editing the stanza does not apply to it. If the
steward's login is deleted or its directory is lost, that deploy stays
unreadable-held at Sealing (or the static flip intent). `idunn cancel` withdraws
only a deploy held for a readiness disagreement before Fencing, and `idunn
expire` only one in Warming through Committing, so neither frees a deploy held
by the stop. Recovery is to restore the steward's login or recreate the
directory owned by it (and have the steward clear its store).

Record v6 (`deployment_admitted_at_unix_millis`, key 38) is forward-only: an
Idunn that predates v6 cannot read a migrated `control.cc`, so, as for v5 (see
`route-continuity-cut.md`, "An older Idunn refuses the v5 store"), take a
byte-compared pre-install backup of `control.cc` and `history.cc` with Idunn
stopped, and roll back by restoring the backup, never by swapping the binary.

## Initial drivers and replaceable boundary

The first Yggdrasil implementation uses:

- transient systemd units for process lifetime and isolation, regenerated from
  Idunn's admitted state after Idunn starts;
- Docker only for admitted build/test/package runners;
- nginx HTTP proxying for Ghostlight;
- nginx stream proxying for CodexConnector's stable TCP endpoint;
- atomic root-owned files for process-bound write leases and separately
  generated proxy membership;
- one host firewall allow per routed stable endpoint, owned by the route
  driver beside its fragment: admitted with the membership, withdrawn with the
  last membership, never opened by hand;
- graceful nginx validation and reload for route changes.

Idunn calls narrow workload, route, and write-lease/fencing driver ports. Those
ports express stage, start, observe, stop, validate route, fence an incumbent,
publish a process-bound write lease, and separately promote route membership.
They do not express GameCult admission decisions. A later Nomad or Kubernetes
driver may implement the same consequences. Idunn must not grow general
bin-packing, distributed scheduling, service-mesh, container-format,
cryptography, or consensus logic; when those become requirements, place
Idunn's admission semantics over an established orchestrator.

## Candidate promotion

For a singleton incarnation:

1. Resolve the required capability graph. Reuse a compatible admitted shared
   provider, especially Odin, rather than spawning a duplicate.
2. Publish desired fleet intent. Intent is not an Expected incarnation.
3. Freeze the exact source and recipe as `idunn`, then copy and verify it into a
   root-owned actuation stage. The privileged driver never opens Git.
4. Build, test, and package inside the bound runners. Seal the artifact,
   declaration, binding, launch contract, and input digests.
5. Derive the release-bound Expected incarnation from the validated sealed
   release and its exact compiled plan. Publish it only once the deployment
   stop has admitted this transaction (stamping `deployment_admitted_at_unix_millis`), under the candidate's own
   incarnation key, beside whatever the target's admitted incarnation projects.
6. Issue one opaque runtime-instance activation bound to the Expected digest,
   pass the exact Expected and activation documents through the actuator's
   isolated credential mechanism, and start the candidate privately. It has no
   stable route or canonical write lease.
7. After the workload driver observes the exact native runtime and executable
   artifact, publish that activation as current. Require the candidate's own
   signed matching `warming` presence. Render and validate the proposed route
   before changing either authority.
8. Open a named promotion transaction and separately fence the incumbent
   writer. The incumbent may remain routed but its process-bound write authority
   is revoked and it must return a retryable unavailable result.
9. Grant the private candidate its own process-bound write lease. Keep it off
   the stable route and wait for signed `active` health and the independently
   correlated Ready observation.
10. Publish the candidate backend, gracefully reload the route driver, and
   observe that the proxy has adopted it. Proxy lag is explicit transaction
   state, not readiness.
11. Clear the promotion fence only after route adoption is observed, then drain
    and stop the incumbent.

Sealing resolves the source, freezes it, runs every recipe step, and installs
the release before the deployment stop is consulted at all (`advance_sealing`).
The stop gates step 5, the first Verse-visible change: publishing the
candidate's Expected incarnation on the move into `Starting`. Build and test
already run unbraked, inside the sealing phase.

The state-write lease and route membership are distinct authorities. Fencing
the incumbent does not grant the candidate or change the route. Granting the
candidate permits that exact private process to write but does not route stable
traffic. Changing the route grants no write authority. The promotion
transaction clears its fence only after the independently observed write and
route authorities name the candidate. A PID, endpoint, symlink, or unit alone
grants neither authority.

Promotion persists crash-resumable phases: planned, materialized, sealed,
activation-issued, candidate-started, activation-observed, warming,
incumbent-fenced, write-lease-granted, ready-observed, route-observed,
fence-cleared, draining, and complete. A dead Idunn cannot leave a second
writer. Its successor resumes the exact sealed transaction or restores the
prior admitted incarnation through the same fence; it never infers completion
from a symlink or eventual health convergence.

An irreversible state cut fails closed after the write lease moves. A reversible
cut may restore the incumbent only through the same process-write-lease
primitive and only while its artifact and state contract remain valid.

## Continuity and outage behavior

Continuity restarts the exact sealed release, binding, endpoint, runtime
identity, and route membership. Every restart receives a new runtime-instance
activation and, for a state writer, a new process-bound lease only after the new
warming presence is observed. A lease from the dead process is never reused. A
continuity restart does not fetch a branch, rebuild, rerun a migration, alter
the launch contract, or pass the deployment stop. An explicit lifecycle
brake may suspend this actuation without changing deployment authority.

Continuity never stops. Six charged attempts in an hour only open the
ContinuityExhausted incident (a page; the wire name stays, nothing is
exhausted) and gate no mint, wait or yield. When the next attempt is due is a
function of continuity's own record: a stored attempt streak, a backoff anchor
written when the pass first finds the admitted generation dead and the target
free, and a capped doubling wait from that anchor (5 s base, 300 s cap). A run
of 600 s or more by the admitted body resets the streak. A generation lost to a
host reboot (the workload driver's evidence, `lost_to_host_reboot`) is
re-materialized once per admission without charge or wait.

Each target has two slots, the incumbent and a transaction's candidate. The
incumbent is continuity's until a transaction takes the target, at Fencing (at
Starting for a stop-then-start rollout), through one `take_target` CAS; of two
transactions entering from one view exactly one succeeds. Before that, a
deployment owns only its private candidate and the target is free for it
between continuity attempts.

Phases Warming through Committing, post-fencing aborts and drains all carry
deadlines (the plan's frozen `PhaseDeadlines`; `WARMING_DEADLINE_SECONDS`, and
`PRE_FENCING_ABORT_DEADLINE_SECONDS` for a pre-fencing abort). What an expired
post-fencing deadline does depends on the owner. A deployment whose candidate
was never issued a write lease aborts to the incumbent. One whose candidate was
issued a lease may have written under a new state contract and is left to the
operator. A continuity retries its phase forward on a capped backoff and never
stops its live candidate, because that candidate is the only running copy of
the admitted body. Both operator cases open an `operator-required` incident
(subject: the transaction id) that closes when the record ends or its step goes
on.

The one yield: a continuity whose route proof has not succeeded gives way to a
deployment the deployment stop admits that awaits the handover of the
target. It begins its own post-fencing abort and the deployment fences what is
admitted. A continuity that has proven its route is serving never yields.

At boot, a lift returns to forward retry any continuity abort an older Idunn
left over a running candidate. If the lift errs or cannot observe the
candidate, the abort is held, not run. The lift is a migration with a deletion
line (follow-up `c2c-delete-boot-lift`).

An admitted generation keeps its Ready and latest Odin receipts as durable
proof of the exact activation at their recorded admission times. Dependency
selection reauthenticates those receipts at those times; continuity and route
observation own current workload health. Historical receipts do not expire
merely because their provider remains admitted longer than the observation
window.

A failed continuity restart leaves its own candidate activation projected under
the shared Expected key. Its abort demotes that key to Expected-only with the
exact activation the transaction issued. Rollback after fencing demotes the
admitted generation's exact activation. No path adopts an activation the
transaction did not issue.

A projection can still hold an activation the abort never resolved: a failure
between preparing a candidate and publishing its observation, an abort written
by an earlier Idunn, or an operator edit. Continuity therefore restarts a
generation only from an Expected-only projection: when its own demotion is
refused it mints no restart, records the deferral in the generation's
continuity backoff, and tries again after it. At boot, Idunn demotes an
activation still projected for a failed continuity in history, by that
transaction's exact activation, and only reports one no failed transaction
issued.

Idunn starts and recovers from its own durable admitted state. Odin is the first
managed semantic daemon, never an Idunn bootstrap dependency. Odin's initial
admission is the sole graph-bootstrap exception: Idunn uses its root-admitted
local binding, publishes Odin's expected projection into durable CultMesh state,
and waits for Odin's observed activation and signed presence before admitting
dependents.

When Odin is unavailable, Idunn continues health observation and continuity for
already admitted incarnations and leaves current routes in place. It does not
begin a graph-changing transaction, and a merely sealed candidate may not start
promotion. Only a transaction whose incumbent has already been fenced may
promote when its frozen evidence already contains the exact Odin Ready receipt
for this runtime instance and signed-presence digest. Otherwise it waits, rolls
back through the same fence when reversible, or fails closed; Idunn never
manufactures Ready locally. New deployment, dependency
substitution, promotion, or scaling waits for semantic discovery. When Odin
returns, disagreement is correlated and exposed before graph-changing work
resumes.

Source selection is explicit per binding: a pinned object, the captured head of
an admitted ref, or a signed release authorization. The current deterministic
foundation retains the exact selected commit, tree, recipe blob, and Gitlink
facts, but shape validation is not proof of Git ancestry, signature authority,
or object custody. Those proofs belong to the narrow Idunn-owned source driver
that will produce these private facts. “Whatever the branch points to when root
runs” is not a selection policy.

## Garden path

The ordinary operator surface is:

```text
install/start Idunn
idunn up ghostlight
idunn up profile:aetheria
idunn up profile:full-gamecult
```

`idunn up` ensures a compatible Odin is admitted first, publishes desired fleet
state, resolves the capability graph, stages candidates, waits for signed
presence and readiness, then promotes routes. An “Idunn script” means the small
repository declaration plus this standard command, not another imperative host
deployment program.

## Verification

The first live cut is complete only when tests and host observations prove:

- exact source → recipe → artifact → process provenance;
- an issued but driver-unobserved activation cannot establish Present, and a
  prior runtime instance cannot replay presence after restart;
- target requests cannot grant an omitted runner or host affordance;
- old command-bearing records and old target scripts are unexecutable;
- a candidate cannot write before its process-bound lease or receive the stable
  route before separate route admission;
- route membership and write leases remain separate records and name the same
  incarnation and process before the promotion fence clears;
- expected, present, and ready disagreement blocks promotion and remains
  observable;
- replayed or stale signed health cannot satisfy present or ready;
- external operator-bound dependencies still require their declared observed
  health/readiness contract; configuration supplies only expected state;
- a compatible shared Odin is reused and a second provider is not spawned;
- Idunn death during every promotion phase resumes or fails closed without a
  second writer;
- proxy-driver lag remains visible and cannot be mistaken for route admission;
- the old process rejects writes immediately after fence or lease revocation;
- same-release crash recovery works while the deployment brake is engaged;
- the lifecycle brake can stop continuity without admitting a deployment;
- Odin loss preserves current continuity/routes and blocks graph changes;
- the incumbent remains routed until the candidate has signed `active` health
  and is correlated Ready;
- the old unit, release, state root, grant, and route cannot regain authority;
- Ghostlight restart preserves the exact world journal and digest identity.
