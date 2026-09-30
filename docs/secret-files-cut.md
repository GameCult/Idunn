# Binding secrets out of every printable surface: cut map

Status: cut map, Imagination pass 1 (Opus), 2026-09-30. Nothing has landed.
There is no separate target document; the invariant in section 1 is the end state.
Anchors are against these HEADs:
- Idunn `46d9c2f`
- gamecult-ops `62e3f05`
- Heimdall `e8e2832`
- StreamPixels `a49f7d8` (reference pattern only)

Incident: during overnight work an agent printed Heimdall's deployment-binding
secrets into a transcript. The operator's words: "I'll rotate before prod, not too
concerned about Anthropic having my oauth; do map the cut that'll ensure it doesn't
happen again".

**Heimdall `hands/secret-files` status (Self, 2026-09-30).**
- **Batch 5** (`efd1137..b5d7b6b`) fixed Soul pass 4's four findings:
  - F1: command-plane diagnostics are fixed text plus the error's class and code, and every provider body goes
    through `readProviderJson`.
  - F2: patron support keeps only the Bifrost status.
  - F3: the recipe test records the secrets `loadConfig` actually reads.
  - F4: `checkSchema` runs on every start.
- **Soul pass 5: hold.** F1-F3 held. Batch 6, in Hands, fixes:
  - **S5-1.** `checkSchema` passes a DEFERRABLE key, a `UNIQUE ... INCLUDE`, a same-named non-unique replacement for
    the partial unique index, and retyped columns. Each of these breaks the store at runtime.
  - **S5-2.** The privilege check demands UPDATE on `audit_events`, which the store only INSERTs into.
  - **S5-3 (older than this batch).** `app.ts:1310` writes a live, 300 s redeemable completion code into the audit
    event `auth_completion_created`.
  - **S5-4 to S5-7.** Test gaps: canary position, the `errorIdentity` filters, pool close, spread-env reads.
  - A flaky live-Postgres test.
- Noted, not in scope: **S5-8**, forced RLS with no policy passes the check.
- **Batch 6** (`b5d7b6b..6c0c876`): S5-1 through S5-7 fixed, and the flaky live test fixed (template DB clone, `fixturePool`).
  Result: 241 passed, 24/24 live, 40 of 41 mutants killed. The survivor, X13, is equivalent: pg-pool destroys a
  failed client.
- **Soul pass 6: hold.** Batch 7 is in Hands.
  - **F1.** An invalid or not-ready unique index passes the check. That breaks sign-in (42P10) and R21.1, because a
    consume takes 2 rows.
  - **F2.** A key index under a nondeterministic collation passes, and the upsert then moves one account's identity
    row to another account.
  - **F3.** The predicate normaliser is unsound: it strips parentheses and lowercases literals.
  - **F4.** Some databases the store could serve are refused: indexes are required by name, and column-level grants
    are not accepted.
  - **F5.** The recipe exploration's all-at-once run stops early; mutants Z4, Z5 and Z6 survive.
  - **F6-F8.** An inspected `cause` is not covered, a narrowed predicate has no fixture, and an added NOT NULL on a
    nullable column is not caught. CHECK constraints and triggers are out of scope, as S5-8 is.
- **Batch 7** (`6c0c876..ca2366c`): F1-F8 fixed.
  - Keys must be `indisvalid` with deterministic collations.
  - Predicates are printed by Postgres, using a TEMP table in a rolled-back transaction, and compared as sets of
    AND conditions.
  - Index names are not required. Column privileges are checked with `has_column_privilege`. A nullable column must
    stay nullable.
  - Results: 241 passed, 39/39 live, 60/61 mutants killed.
- **Soul pass 7: merge.** Nothing blocks.
  - 1400 predicate pairs were checked, and no two different predicates compared equal.
  - The `indisvalid` argument held under interrupted concurrent DDL.
  - TEMP is never refused for the real deployment, because the service role owns the database.
- **MERGED: Heimdall main fast-forwarded to `ca2366c` (2026-09-30).**
- Follow-up branch in Hands:
  - S7-1: the recipe exploration never runs the production start shape.
  - S7-2: extra unique indexes and similar documented as out of scope.
  - S7-3: the column collation must be deterministic.
  - S7-4: one more fixture each for Y3, Y4, Y5 and Y8.
- **Recorded:** a least-privilege column-subset grant is refused (F4(b)). That is theoretically servable, but it is
  not how Heimdall is deployed.

**Rulings and defaults, 2026-09-30 (Self).**
- **Q3 A is the operator's ruling:** "I'll rotate before prod". Cuts 1-4 land with the current values; rotation
  overwrites the credential files later. The token-encryption and signing-key sub-question waits for rotation time.
- **Q2 A, Self's default:** the plaintext fallback is a named shim, deleted in Cut 5. It is not a product decision;
  the operator may overrule it.
- **Q1 A, Self's default:** no schema change now; B is recorded as a follow-up if a second target repeats
  Heimdall's mistake. The operator may overrule it.

Open:
- Cut 0 capture;
- whether a re-deploy of an unchanged revision reloads a rotated credential (not yet
  established; see Cut 3).

## 1. The invariant, and why a "don't print" rule is the wrong cut

**Secret material exists only in files whose sole content is one secret.** Those
files are root-owned, mode 0400, and delivered by systemd `LoadCredential`. Nothing
multi-purpose ever holds a secret value:
- no binding, template, recipe, unit or transient unit;
- no plan blob, log, status line or README step.

If that holds, every existing printer becomes harmless. `cat` of a binding, `idunn
validate`, a journald tail, `systemctl cat`, `idunn status` and a plan decode all show
paths. No agent has to remember anything.

What was missing, such that printing the secret made sense to the agent:
- The gamecult-ops Heimdall runbook ordered it. `runbooks/heimdall-discord-launch.md:142-150`
  says to reconstruct the missing template from the installed binding with
  `ssh ygg 'sudo cat /etc/gamecult/idunn/bindings/heimdall.toml'`.
- The installed binding carried secret values under `[workload.environment]`.
- Heimdall's recipe declares those values as plain environment. Its admit contract
  therefore makes the binding the only place they can live.

The agent followed a runbook against a file that should never have held a secret.
The cut removes the secret from the file, not the reading.

The substrate already exists; it is not an invention of this map:
- Idunn's `[workload.secret_files]` (`src/deployment.rs:607`) lowers to
  `--property=LoadCredential=NAME:path` (`src/drivers.rs:3707-3718`).
- It sets `NAME=/run/credentials/<unit>/NAME` in the environment
  (`src/drivers.rs:3311-3336`).
- It admits only root:root 0400 single-link sources in a root-owned directory
  (`src/drivers.rs:6966-7000`).
- Idunn never opens a service credential for reading; it only stats it.
- StreamPixels and Ghostlight already use this through `*_FILE` names
  (`StreamPixels apps/service/src/security/environment-secret.ts`,
  `deployment/idunn/service.toml:130-142`).

Heimdall is the target that never moved.

### Leak paths found in the Body

| # | Path | Evidence | After the cut |
|---|---|---|---|
| P1 | Runbook tells an agent to `sudo cat` the installed binding | gamecult-ops `runbooks/heimdall-discord-launch.md:147` | step deleted (Cut 4); the file holds only paths anyway |
| P2 | Binding install README handles the DB URL in an agent shell (`export DBURL=$(sudo grep … service.env)`, `perl` substitution) | gamecult-ops `deployment/idunn-bindings/README.md:15-30` | directory deleted (Cut 4) |
| P3 | Transient unit carries every workload env value via `--setenv`. That lands in `/run/systemd/transient/<unit>.service`, readable through `systemctl cat` and `systemctl show -p Environment` without privilege. | Idunn `src/drivers.rs:3704-3706` | only non-secret values and credential paths |
| P4 | `/proc/<pid>/environ` of the workload (root-readable; Idunn itself reads it and keeps only a hash, `drivers.rs:3116`, `:3278`) | Idunn `src/drivers.rs:3116` | paths only |
| P5 | Every deployment plan persists the full binding text in `binding_blob`. It lands in `control.cc`, history and the nightly state backup. | Idunn `src/deployment_plan.rs:562`, `:710`; gamecult-ops `runbooks/gamecult-state-backup.md:38-39` | new plans hold paths; **old plans keep old values forever** → rotation (Q3) |
| P6 | TOML errors quote the offending source line verbatim. **CONFIRMED by probe** on `toml 0.8.23`: an invalid escape, a wrong type, an unknown field and an unterminated string each echoed the full `KEY = "value"` line. They reach journald through `load_bindings` (`control_plane.rs:3883` → `eprintln` at `:4597`, `:5350`) and the terminal through `idunn validate` (`:2475`). `scripts/install-idunn-yggdrasil-release.sh:38` tails that journal into the operator's terminal. | Probe: `toml::from_str` with four malformed lines, `e.to_string()` contained the canary in all four | lines hold paths only |
| P7 | `idunn status --command` prints `last_error` verbatim | Idunn `src/control_plane.rs:4131-4133` | nothing secret can be in an error |
| P8 | Legacy pre-Idunn Heimdall. `systemd/heimdall.service:14` reads `EnvironmentFile=/srv/heimdall/env/service.env`. `scripts/deploy-heimdall.sh:182` leaves `.service.env.backup.*` copies. `scripts/check-heimdall.sh:17-20` and `scripts/provision-ghostlight-heimdall-session-yggdrasil.sh:16-19` `source` it. | gamecult-ops | retired in Cut 5 |
| P9 | Two contradictory binding directories. `deployment/idunn-bindings/` says install 0640 root:idunn. `idunn/yggdrasil/bindings/README.md:24-27` says Heimdall is "intentionally absent" (stale). `scripts/idunn/idunn-deployment-targets.ps1:194` repeats it. | gamecult-ops | one directory (Cut 4) |

Out of scope, recorded:
- Docker runner `[runners.*.environment]` values reach `docker run -e` and `docker
  inspect`.
- No current template puts a secret there, and runner `secret_files` already mounts
  files (`drivers.rs:2018-2025`).

## 2. Identity, lifecycle, authority (per persistent kind)

| Kind | What names it | Life over time | Who decides |
|---|---|---|---|
| Heimdall secret value | credential file `/etc/gamecult/heimdall/credentials/<name>`. Its binding key is the `*_FILE` env name. The directory convention follows `streampixels-service.toml.in [workload.secret_files]`. | Created once by Cut 3. Overwritten in place on rotation (Q3). Never copied elsewhere. | operator (value); ops template (path) |
| Heimdall binding | target `heimdall` | rendered from `idunn/yggdrasil/bindings/heimdall.toml.in`, installed by `install-idunn-yggdrasil-release.sh`; hand edits on the host are forbidden writers | gamecult-ops template |
| Recipe input class (secret vs plain) | env name in Heimdall `deployment/idunn/recipe.toml` | changes only by a reviewed Heimdall commit | Heimdall repo |
| Plan `binding_blob` | plan id (content hash) | immutable history; never rewritten (rewriting breaks `plan_id`, `deployment_plan.rs:662-668`) | Idunn |
| Legacy `service.env` + backups | path under `/srv/heimdall/env` | read-only after Cut 3; shredded in Cut 5 | operator |

**Constraint that shapes every Idunn-side option.** An admitted generation's
stored plan is re-admitted under the *current* rules on continuity
(`control_plane.rs:6871`, and the other `parsed_inputs()` sites). Any
tightening of `admit()` that rejects an already-stored (recipe, binding) pair
takes daemon survival away from that target. New admit rules must be vacuous
for recipes that predate them.

## 3. Authority map

- **Owner of secret material:** the credential file. Nothing else owns a copy.
- **Owner of "is this input secret":** the program's own recipe. It declares secret
  inputs only as `*_FILE` names; it declares no plaintext secret name. Idunn
  enforces the rest with an existing rule: a binding may bind only declared
  names (`src/deployment.rs:1614-1617`).
- **Owner of the binding's shape:** the gamecult-ops template. The installed file is
  derived from it.
- **Inputs:**
  - Heimdall reads `*_FILE` paths from its environment and the bytes from those
    files.
  - Idunn reads binding paths and stats the files.
- **Outputs:** systemd credentials under `/run/credentials/<unit>/`.
- **Derived state:**
  - The binding's `[workload.secret_files]` holds paths only.
  - The transient unit's `LoadCredential=` and `Environment=` hold paths only.
  - The plan blob holds paths only.
- **Forbidden holders of a secret value:**
  - `[workload.environment]`;
  - any `*.toml.in` or template;
  - recipe `required_environment` and `optional_environment`;
  - READMEs and runbooks;
  - `/srv/heimdall/env/service.env` after Cut 5;
  - shell variables in any committed ops script.
- **Forbidden printers:** none needed. A printer of any artifact above prints no
  secret. The only way to see a secret is `sudo cat` of a single-purpose
  credential file, and that is a deliberate act.
- **Shared paths:** the same `readSecretInput` reader serves legacy start, Idunn
  start and tests (Cut 1). The same `admit()` serves `idunn validate`, binding
  load and continuity re-admit.
- **Deletion line:**
  - Heimdall's plaintext secret names in the recipe (Cut 1).
  - `deployment/idunn-bindings/` and the runbook `sudo cat` step (Cut 4).
  - The plaintext fallback shim, the legacy unit, `service.env`, the legacy deploy
    and check scripts, and the transitional copy script (Cut 5).

## 4. Cuts

### Cut 0. Capture (read-only, Yggdrasil; no edits)

- **Rule for every command:** print key names, hashes or booleans, never values.
- **Never read** `/var/lib/gamecult/**/*.cc`.
- **Key names of the installed binding.** Pipe the binding through this extractor.
  It prints section headers and `key = ` names. It refuses base64-looking
  continuation lines and counts them instead. It was tested against the ops
  template and a multiline PEM:
  ```
  sudo awk '
  /^[[:space:]]*\[[^=]*\][[:space:]]*$/ { print; next }
  /^[[:space:]]*(#|$)/ { next }
  match($0, /^[[:space:]]*[A-Za-z_][A-Za-z0-9_-]*[[:space:]]+=[[:space:]]/) {
    k = substr($0, 1, RLENGTH); sub(/[[:space:]]+=[[:space:]]$/, "", k); gsub(/^[[:space:]]+/, "", k)
    if (length(k) <= 64) { print "  " k; next } }
  { other++ } END { print "unclassified-lines " other+0 }' /etc/gamecult/idunn/bindings/heimdall.toml
  ```
- **Key names of the legacy environment file:**
  `sudo grep -o '^[A-Za-z_][A-Za-z0-9_]*=' /srv/heimdall/env/service.env`.
  Count its siblings with
  `sudo ls /srv/heimdall/env | grep -c 'service.env.backup'`.
- **Where each value comes from.** For every secret-class name present in both
  sources, print only whether the two values are equal: `sha256` each value
  inside one root shell, and emit `NAME equal|differ`. This decides whether
  Cut 3 copies from `service.env` or from the binding.
- **What runs Heimdall now:**
  `systemctl is-active heimdall.service`, plus the names of the
  `idunn-heimdall-*` units. Inventory (`gamecult-ops inventory.md:632-634`) says
  the legacy unit runs Heimdall and that Idunn has never admitted a Heimdall
  generation. That claim is unverified.
- **Credential-source preconditions:** `stat -c '%U:%G %a %h'` on the intended
  credential directory, if it exists.

### Cut 1. Heimdall reads secrets from credential files

- **Repo/branch:** Heimdall, a branch from `e8e2832`. No dependencies.
- **Deletes first:**
  - Remove these plaintext secret names from `deployment/idunn/recipe.toml:134-174`:
    - `GC_ACCESS_DATABASE_URL`, from `required_environment` (`:173`);
    - `GC_ACCESS_APP_BIFROST_SHARED_SECRET`;
    - `GC_ACCESS_APP_GHOSTLIGHT_SHARED_SECRET`;
    - `GC_ACCESS_APP_SHARED_SECRET`. No reader exists for it: `src/config.ts:208-216`
      reads only per-slug names. Delete it; do not rename it.
    - `GC_ACCESS_BIFROST_PATRON_SUPPORT_SECRET`;
    - `GC_ACCESS_PROVIDER_{DISCORD,PATREON,TWITCH,YOUTUBE}_CLIENT_SECRET`;
    - `GC_ACCESS_TOKEN_ENCRYPTION_KEY_BASE64`.
  - Remove the inline-PEM input `GC_ACCESS_SIGNING_PRIVATE_KEY_PEM`
    (`src/config.ts:281-284`). It is not declared in the recipe, and
    `GC_ACCESS_SIGNING_PRIVATE_KEY_PATH` already covers the key: bind it through
    `secret_files` under its own name, as `Idunn docs/migration.md:463-472`
    describes.
  - Client IDs, URLs, TTLs and `GC_ACCESS_APP_*_RUNTIME_IDS` are not secrets and stay.
- **Adds:**
  - The recipe gains the `_FILE` twin of every deleted secret name.
    `GC_ACCESS_DATABASE_URL_FILE` is **required**; the rest are optional.
  - One reader in `src/config.ts`, `readSecretInput(env, name)`:
    - it reads `env[name + "_FILE"]` and strips one trailing newline;
    - when the `_FILE` variable is absent, it falls back to `env[name]`;
    - it throws if **both** are set, so one input has one owner;
    - its errors name the variable and the path, never the file's contents.
  - The plaintext fallback is a named shim. It protects only the legacy
    `EnvironmentFile` unit (P8), and Cut 5 deletes it (Q2).
- **Per-file changes:**
  - `src/config.ts:124-139` (`readProviderConfig`, the client secret);
  - `:208-216` (app shared secrets);
  - `:294-296` (token key);
  - `:302-304` (patron secret);
  - `:306-308` (database URL); route each through `readSecretInput`;
  - `:205`: storage-backend selection must key off the resolved database URL, not
    `env.GC_ACCESS_DATABASE_URL`;
  - `src/custody.ts:17,23,54`: error text names `GC_ACCESS_TOKEN_ENCRYPTION_KEY_BASE64_FILE`;
  - `src/providers.ts:24,33,42,51,65,83`: `clientSecretEnv` becomes the `_FILE`
    name, so the published witness (`src/verse-witness.ts:240-243`, names only)
    stays truthful.
- **Verification:**
  - tests (vitest, a new `tests/config-secrets.test.ts`):
    - `_FILE` value is read and has its newline trimmed. Mutant: read `env[name]`
      first. It must die.
    - Both set is refused. Mutant: file wins silently.
    - A missing or unreadable file errors with the variable and path, and the
      message contains no byte of a canary written to a sibling file. Mutant:
      append the contents to the message.
    - The required database-URL file is absent → startup refused.
  - StrykerJS scoped to `readSecretInput` and its call sites.
  - Negative grep on the recipe returns nothing. It was tested for collision:
    `_FILE` names end in `_FILE"` and do not match.
    `rg -n '"GC_ACCESS_(DATABASE_URL|[A-Z_]*_SECRET|TOKEN_ENCRYPTION_KEY_BASE64|SIGNING_PRIVATE_KEY_PEM)"' deployment/idunn/recipe.toml`
  - Negative grep on source: `rg -n 'env\.GC_ACCESS_(DATABASE_URL|TOKEN_ENCRYPTION_KEY_BASE64|BIFROST_PATRON_SUPPORT_SECRET)\b' src`
    returns nothing outside `readSecretInput`.
  - Heavy runs go on Yggdrasil via `tools/stopgap/ygg-verify.sh`.

### Cut 2. Pin the Idunn rule the campaign rests on

- **Repo/branch:** Idunn, a branch from `46d9c2f`. It has no dependencies and can run
  in parallel with Cut 1.
- **Why:** the guarantee "a binding cannot carry an undeclared name" is one line
  (`src/deployment.rs:1614-1617`). Its only test, `extra_operator_environment_is_rejected`
  (`:2178-2186`), asserts `is_err()`. Any other error satisfies it, which is the
  cousin-not-the-rule failure.
- **Changes:** tests only. No behaviour change.
  1. The admit error for an undeclared `[workload.environment]` name is *this*
     rule. Assert on the message, or better, on a typed refusal if Hands finds
     one is cheap.
  2. The same assertion for an undeclared `[workload.secret_files]` name.
  3. `idunn validate` exits non-zero on an undeclared name, and its output does
     not contain the bound value. Use a canary; this pins P6 at the admit layer,
     not the TOML layer.
- **Verification:** run `cargo-mutants --in-diff` scoped to `deployment.rs`
  `admit`, on Yggdrasil. The mutant `is_subset(..)` → `true` at `:1615` must die
  in tests 1 and 2.
- **Deletion line:** none.

### Cut 3. Yggdrasil: credential files and a secret-free installed binding

- **Depends on:** Cut 1 merged, and Cut 0 capture read.
- **First:** a copy of the current installed binding goes to
  `/root/heimdall.toml.pre-secret-cut`, mode 0400. Take it with `install -m 0400`
  and never display it. It is the rollback source until rotation; after rotation
  it is shredded (Q3).
- **Transitional copy (Q3-A):** the script is
  `gamecult-ops scripts/provision-heimdall-credentials-yggdrasil.sh`, about 40
  lines, modelled on `provision-ghostlight-heimdall-session-yggdrasil.sh`. It
  must:
  - run as root and refuse otherwise;
  - read values from the source Cut 0 chose, without `set -x` and without printing;
  - write each value through `mktemp` in a `root:root 0700` directory;
  - `chmod 0400` each file and `mv` it into place;
  - print only `NAME written (N bytes)`.

  The script is deleted in Cut 5.
- **Binding edit:**
  - `[workload.environment]` keeps only non-secret declared names.
  - `[workload.secret_files]` gains the `*_FILE` entries.
  - `GC_ACCESS_SIGNING_PRIVATE_KEY_PATH` is bound there if it is not already.
- **Verification, in this order, printing only exit codes and counts:**
  1. `idunn validate --recipe <new recipe> --binding <new binding>` succeeds.
  2. `idunn validate --recipe <new recipe> --binding /root/heimdall.toml.pre-secret-cut > /dev/null 2>&1; echo $?`
     is non-zero. This is the negative check: the old shape is now inadmissible.
     Discard the output; it could quote a line.
  3. Run the Cut 0 extractor on the installed binding. Every
     `[workload.environment]` name must be in the recipe's non-`_FILE`
     declared set.
  4. After `idunn up heimdall`, run
     `systemctl show -p Environment --value <idunn-heimdall unit> | tr ' ' '\n' | cut -d= -f1`
     to list names only. Every `*_FILE` value starts with
     `/run/credentials/`; print only a boolean per name.
  5. The P6 regression probe: make a scratch copy of the new binding with one
     line corrupted, and run `idunn validate` on it. Its output contains only paths.
- **Not yet established:** whether `idunn up heimdall` on an unchanged revision
  starts a fresh unit, and so reloads a rotated credential. Hands probes it and
  reports; the map's rotation step depends on the answer.

### Cut 4. gamecult-ops: one binding directory, template derived from a secret-free binding

- **Repo/branch:** gamecult-ops `main` from `62e3f05`. Depends on Cut 3's
  verification step 3 passing. From then on, copying the installed binding is
  safe by construction.
- **Deletes first:**
  - `deployment/idunn-bindings/README.md` (76 lines) and
    `deployment/idunn-bindings/heimdall.toml.template` (119 lines): the second
    authority and P2.
  - `runbooks/heimdall-discord-launch.md:142-150`, the reconstruct-by-`sudo cat`
    paragraph (P1).
  - `idunn/yggdrasil/bindings/README.md:24-27`, the stale "Heimdall is
    intentionally absent" paragraph.
- **Adds:** `idunn/yggdrasil/bindings/heimdall.toml.in`. It is the post-Cut-3
  installed binding, with only the signer id replaced by
  `PROVISIONED_HEIMDALL_SIGNER_ID`, following the README's convention.
- **Keeps and moves:**
  - Carry one sentence into `idunn/yggdrasil/bindings/README.md`: secret inputs
    are `[workload.secret_files]` paths to `root:root 0400` credential files, and
    a template never holds a value.
  - The README's field and preprovisioning tables duplicate
    `Idunn docs/migration.md:440-472`. Point to that instead of moving them.
  - Reconcile the install mode. The README says 0644 and
    `scripts/install-idunn-yggdrasil-release.sh:32` installs 0640. Make the
    README say what the script does.
  - Update `scripts/idunn/idunn-deployment-targets.ps1:194` and
    `runbooks/heimdall-discord-launch.md` §4 to name the template.
- **Verification:**
  - Render the template and diff it against the installed binding on Yggdrasil.
    The only difference is the signer-id line. This diff is safe to print only
    because Cut 3's step 3 passed.
  - Negative greps, each returning nothing:
    - `rg -n 'sudo cat /etc/gamecult/idunn/bindings' runbooks`
    - `rg -n 'REPLACED_ON_INSTALL' .`
    - `rg -n 'deployment/idunn-bindings' . -g '!docs/repo-census-2026-09/**'`
  - Run the key-name extractor over `idunn/yggdrasil/bindings/*.toml.in`. No
    `[workload.environment]` name ends in `SECRET`, `_KEY`, `PASSWORD`, `TOKEN`
    or `DATABASE_URL`. This is a review aid, not an enforcement: enforcement is
    admit plus the recipe.

### Cut 5. Retire the legacy Heimdall path

- **Depends on:** Heimdall running as an Idunn-admitted generation, and the
  legacy unit stopped. If Cut 0 shows Heimdall is not yet admitted, this cut
  waits on that admission. It is not a fork.
- **Deletes first:**
  - In gamecult-ops:
    - `systemd/heimdall.service` (28 lines);
    - `scripts/deploy-heimdall.sh` (238);
    - `scripts/check-heimdall.sh` (58; it sources `service.env`);
    - `scripts/test-heimdall-yggdrasil-wiring.sh` (45);
    - `scripts/provision-ghostlight-heimdall-session-yggdrasil.sh`. It sources
      `service.env`, and after rotation the operator writes Ghostlight's
      copy of the shared secret directly;
    - the transitional `provision-heimdall-credentials-yggdrasil.sh`;
    - runbook §2 "Upload The Service Env" and the §5 `check-heimdall.sh` steps;
    - the `scripts/README.md:33-34,46` entries.
  - In Heimdall: the plaintext fallback in `readSecretInput`.
  - On Yggdrasil: disable the legacy unit, then `shred -u`
    `/srv/heimdall/env/service.env`, every `.service.env.backup.*` and
    `.service.env.candidate.*`, and `/root/heimdall.toml.pre-secret-cut`.
- **Verification:**
  - `rg -n 'service\.env' scripts runbooks systemd` returns only history notes.
  - Heimdall test: setting a plaintext name without its `_FILE` twin no longer
    configures the secret. Mutant: restore the fallback.

## 5. Operator questions

**Q1. Should Idunn type the secret/plain distinction in the recipe schema?**
- A: No schema change. Heimdall's recipe declares secrets only as `*_FILE` names,
  and the existing rule "a binding binds only declared names" does the
  enforcing (Cuts 1–2).
- B: Add `required_credentials` and `optional_credentials` to `ServiceDeclaration`
  (`src/deployment.rs:112-126`). `admit()` then refuses a declared credential in
  `[workload.environment]`, which closes the loophole of binding a `*_FILE` name
  to a readable non-credential path. Because of the continuity re-admit (§2),
  the rules must be vacuous for old recipes, and every recipe would migrate.

**Recommended: A.** Neither option stops a recipe author from declaring a secret
as plain environment. Only the author knows what a string means, and B changes
nothing about that. B's real gain is the loophole, and that loophole leaks to
the filesystem, not to transcripts. Record B as a follow-up if a second target
repeats Heimdall's mistake.

**Q2. Should Heimdall keep the plaintext fallback during the legacy period?**
- A: Keep it as a named shim. Setting both the value and its `_FILE` twin is
  refused, and the shim is deleted in Cut 5.
- B: Go file-only now. That requires Heimdall to be Idunn-admitted and the legacy
  unit retired in the same move.

**Recommended: A.** Inventory says the legacy unit is what runs Heimdall today.
Under Idunn the fallback is inert, because admit refuses the plaintext names.

**Q3. How should rotation be sequenced? ("I'll rotate before prod")**
- A: Land Cuts 1–4 with the current values, copied by the transitional script
  without being displayed. Rotate later, before prod, by overwriting the
  credential files and redeploying.
- B: Rotate at Cut 3. The operator writes the new values straight into the
  credential files and no copy script exists.

**Recommended: A.** Rotating before the cut would pour the new values into the
same leaky binding. Rotating at the cut blocks it on the operator, and it
cascades: the app shared secrets must change in Ghostlight, StreamPixels and
Bifrost's credential files in the same window.

Either way:
- Old values stay in every stored plan's `binding_blob`, in history and in
  backups (P5). Rotation, not history rewriting, is what makes them dead.
- Rewriting plans breaks `plan_id` (`deployment_plan.rs:662-668`).

Sub-question for rotation time, if Cut 0 shows `GC_ACCESS_TOKEN_ENCRYPTION_KEY_BASE64`
was in the binding: rotating that key makes the stored encrypted provider tokens
undecryptable (`src/custody.ts`). Should Heimdall re-encrypt them, or should users
re-link? The answer depends on how many linked accounts exist before prod. The
signing key has the same shape: rotating it invalidates issued tokens.

## 6. Subtraction ledger (estimate)

| Cut | Removed | Added | Notes |
|---|---|---|---|
| 1 | ~12 recipe names, inline-PEM path (~5), per-site env reads (~15) | ~12 `_FILE` names, reader (~20), tests (~80) | net-positive only in tests |
| 2 | 0 | ~40 test lines | pins an existing rule |
| 3 | secret values from the installed binding | 1 transitional script (~40) | script dies in Cut 5 |
| 4 | 195 (second binding directory), ~9 runbook lines, ~5 README lines | ~120 template | template replaces the second directory |
| 5 | ~420 ops lines, the shim, the legacy unit, `service.env` + backups | 0 | |

Follow-ups outside this migration:
- Q1-B, if a second target repeats Heimdall's mistake.
- Docker runner environment is visible through `docker inspect` (Idunn
  `src/drivers.rs:2016-2017`). No current binding puts a secret there.
- The P6 echo also applies to recipes. They are repo files and hold no secrets
  by construction, so no action is needed.
