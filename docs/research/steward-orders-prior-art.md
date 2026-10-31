# Steward orders prior art: privileged agents, break-glass, two-person rule

Serves Eureka campaign `eureka-body`, follow-up `eureka-root-idunn-orders`, under
ruling `eureka-root-idunn-orders-break-glass` (operator 2026-10-31: Eureka issues
typed orders to Idunn, run as root and posted to #operator, plus an announced,
brakeable break-glass order). Imagination pass, session eureka-self-2026-10-31.

Written from recall of the systems' documentation, not re-fetched on
2026-10-31; each claim names where to check it. Facts first, then what each one
says about this design.

## 1. Privileged agents that take requests from less privileged callers

### nix-daemon (closest shape)

- Unprivileged users never run as root: the Nix client talks to `nix-daemon`
  over a Unix socket (`/nix/var/nix/daemon-socket/socket`), and the daemon reads
  the caller's uid from the socket's peer credentials. `nix.conf`
  `allowed-users` says who may connect at all; `trusted-users` says who may
  pass privileged settings. (nix.conf manual, "allowed-users",
  "trusted-users".)
- The daemon accepts a closed set of typed operations (the worker protocol),
  not shell strings.
- Lesson: a local Unix socket plus `SO_PEERCRED` is the established way for a
  root agent to know which local principal is asking. The principal is derived
  from the kernel, never declared by the caller.

### Postfix maildrop and pickup

- Unprivileged `sendmail` writes into the `maildrop` queue through the setgid
  `postdrop` helper; the privileged `pickup` daemon reads each file, checks it,
  and copies it into its own queue before anything acts on it. (Postfix
  architecture overview, `pickup(8)`, `postdrop(1)`.)
- Lesson: the submitter's inbox is not the record. The privileged side copies
  the request into a store only it can write, and only that copy is acted on.

### SaltStack

- The master publishes jobs to minions that run as root; each job gets a job
  id (jid) from the master, not from the caller, and returns are kept in the
  job cache (`salt-run jobs.lookup_jid`). (Salt docs, "Job management".)
- `publisher_acl` and external auth (`external_auth`) grant a user named
  functions only, for example `pkg.install` and `service.restart`, optionally
  with argument patterns. (Salt docs, "Publisher ACL system", "External
  Authentication System".)
- CVE-2020-11651 and CVE-2020-11652: the master's request server exposed
  methods that skipped authentication, which gave unauthenticated root on every
  minion. (Salt advisory, April 2020.)
- Lesson: grant typed functions with argument grammar, not a shell; ids belong
  to the authority; and a root agent's intake is its attack surface, so keep it
  local and small.

### Ansible and AWX (Tower)

- Modules (`ansible.builtin.apt`, `ansible.builtin.systemd_service`) take typed
  parameters and report changed, ok or failed; `become` is per task.
- AWX job templates with RBAC let a user launch only pre-approved templates,
  with survey fields as typed, validated parameters; the activity stream and
  job output are the audit. (AWX docs, "Job Templates", "Surveys", "Activity
  Stream".)
- Lesson: "a named operation with validated parameters, launched by a principal
  who may launch only that" is the mature shape of a typed order.

### NixOS deploy tools (deploy-rs, colmena, nixos-rebuild)

- Activation switches a whole system closure; deploy-rs "magic rollback" rolls
  the activation back unless the deployer confirms it within a timeout over a
  fresh connection. (deploy-rs README, "Magic Rollback".)
- Lesson: a change that can cut off its own operator wants a confirm-or-revert
  window. For Idunn this already exists for releases (continuity, fencing); it
  matters for any later host-level order (reboot, network) and for break-glass.

### polkit and sudoers allowlists

- polkit rules can grant a user `org.freedesktop.systemd1.manage-units` for
  named units and verbs only (rules see `action.lookup("unit")` and `"verb"`).
  (polkit(8), systemd's polkit actions.) The mind units already use a polkit
  start grant.
- sudoers `Cmnd_Alias` allowlists and `log_output` I/O logs are what the interim
  root (`gamecult-ops deploy/eureka-root`) uses today.
- Lesson: a unit allowlist is ordinary practice. polkit alone would give
  restarts without a single order record, brake gate or #operator post, so it
  is the smaller tool but not the ruled one.

## 2. Break-glass access

- The common pattern (healthcare "break the glass", cloud emergency accounts,
  Teleport access requests, AWS's guidance on emergency access): the actor
  states a justification; use raises a loud alert at once; access is
  time-boxed; the session is recorded; every use is reviewed afterwards.
  (Teleport docs, "Access Requests" and "Session Recording"; AWS Well-Architected
  security pillar, emergency access.)
- What these systems assume and Yggdrasil lacks: a human on call who sees the
  alert in minutes. With the operator away, the review is done by Minds and
  members in #operator, and the alert reaches them only if chat is up.
- Lesson: break-glass is defended by visibility and review, not by its
  intake. It must announce before it runs and keep a full record of what ran.

## 3. Two-person rule and veto windows

- Two-person integrity (military nuclear surety; banking dual control): no
  single person can complete a critical action; a second must positively
  approve. Software forms: required reviewers on protected branches; Google's
  multi-party authorization for production changes; Teleport requests needing
  N approvers.
- A veto window is weaker and different: lazy consensus (Apache "lazy
  consensus": silence means assent after a stated period). It protects only if
  someone is watching during the window, and an attacker who controls the
  timing picks the quiet hour.
- The ruling in force (`brake-anyone-pulls-motion-releases`) is a veto shape:
  anyone pulls at once, only a motion releases. Positive approval would need a
  second principal who is awake; with the operator gone that is another Mind,
  and Minds read the same chat an injection arrives through.
- Lesson: for typed orders the type is the defence (closed operations,
  validated arguments, an allowlist); for break-glass the window plus the
  record is the defence, and its strength is the attention of whoever reads
  #operator. Say so where the window is configured.

## 4. What this gives the first cut

- Authenticate by kernel peer credentials on a root-owned local socket; derive
  the principal and the issuing unit, never accept a declared requester.
- Idunn assigns the order id and keeps the only record in its own store;
  the caller's request is not the record.
- Orders are a closed set of typed operations with argument grammar and an
  allowlist in root-owned configuration.
- Every order and its result are posted; the store is the record and the post
  is its projection.
- Break-glass, when it lands, waits on an announcement and a window, and is
  recorded in full.
