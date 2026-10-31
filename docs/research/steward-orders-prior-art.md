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

## 5. Windows hosts: Starfire and Raven

Serves follow-up `steward-orders-windows-hosts` under ruling
`remote-admin-on-desktops` (operator 2026-10-31: "We'll need a way to do this
kind of thing remotely"). The trigger: on 2026-10-31 she approved through UAC,
sitting at Starfire, an elevated script that added an inbound firewall rule for
Ollama on the WireGuard interface and registered a SYSTEM startup task. She is
away from today, possibly for months; a hard reboot already needs a call.

### Body, as read on 2026-10-31 (read-only)

- Both hosts answer TCP 22 and 3389 from Yggdrasil over WireGuard: Starfire
  10.77.0.2, Raven 10.77.0.4. WinRM is stopped (start type Manual) on both.
  winget v1.29 is present; choco is not on Raven.
- Yggdrasil holds no SSH client config and no private key for either host:
  `gamecultadmin` has only `authorized_keys` and `known_hosts`, root has no
  client key, and the mind users have no home. Nothing on Yggdrasil can reach a
  Windows shell today.
- Starfire: sshd runs, the stock `Match Group administrators` stanza is in
  force, `C:\ProgramData\ssh\administrators_authorized_keys` does not exist and
  the operator's user has no `authorized_keys`. Its admins are the operator's
  user (`Meta`) and the built-in Administrator. No key reaches Starfire; RDP,
  Sunshine/Moonlight and her own UAC click are the only privileged paths in.
- Raven: `administrators_authorized_keys` holds one key, commented
  `192.168.1.84 access key`, whose private half is on Starfire (the `raven`
  host entry in the operator's `~/.ssh/config`). A session with it is
  `BUILTIN\Administrators` at High Mandatory Level: Starfire already holds raw,
  unaudited admin on Raven.
- Raven already runs Idunn's host actuator (`idunn-host serve`, task
  `GameCult-Idunn-Host`, at logon, interactive, Limited): it dials
  `10.77.0.1:17890/udp` over CultNet RUDP, accepts only requests Idunn's
  service identity signed, and signs its reports with its own host identity
  (`docs/host-actuator.md`; runbook `gamecult-ops/runbooks/idunn-host-raven.md`).
  It is unelevated by design: Muninn must live in the desktop session. Starfire
  has no actuator.
- Memory, not re-proven here: Starfire has frozen under load (PSU or GPU
  suspected), and nothing that must survive may depend on Starfire or Raven
  staying up (`workstation-load-budget.md`, 2026-10-29).

### PowerShell JEA (Just Enough Administration)

- A session configuration (`.pssc`, registered with
  `Register-PSSessionConfiguration`) maps caller groups to role capabilities
  (`.psrc`); a role lists `VisibleCmdlets` with per-parameter `ValidateSet` or
  `ValidatePattern`, the session runs in `NoLanguage` mode and executes as a
  temporary virtual account (`RunAsVirtualAccount`) or a gMSA, so the caller
  holds no admin right. `TranscriptDirectory` records every session.
  (Microsoft Learn: "JEA overview", "JEA role capabilities", "JEA session
  configurations", "Auditing and reporting on JEA".)
- JEA rides WinRM. SSH-based PowerShell remoting has not supported endpoint
  configurations or JEA (Microsoft Learn, "PowerShell remoting over SSH",
  limitations; the current page may have changed, check before relying on it).
- Microsoft's own warning: a role that exposes a command able to run arbitrary
  code (`Start-Process`, `Invoke-Expression`, `Register-ScheduledTask` with a
  free action, `New-Service` with a free path) is an escape to the run-as
  account ("JEA security considerations").
- On a workgroup desktop, WinRM needs a listener (HTTPS for anything but
  Kerberos), a local Windows account for the caller, and that account's
  password or certificate mapping held by the caller.
- Lesson: JEA is the platform's answer to "typed, parameter-validated admin
  with a transcript", and its role file is the same shape as a typed order. It
  brings WinRM (off today), a Windows credential that would live on Yggdrasil,
  and no order id, brake or post: those would still be Idunn's, so the grammar
  would live twice, once in PowerShell attributes on each host and once in the
  order record.

### Ansible `win_*` modules over WinRM or SSH

- `ansible.windows.win_service` (state started, stopped, restarted; start
  mode), `community.windows.win_firewall_rule` (name, direction, action,
  protocol, localport, remoteip, profiles, interfacetypes),
  `community.windows.win_scheduled_task` (actions, triggers, principal, run
  level), `ansible.windows.win_package` and the community `win_chocolatey` and
  winget modules. Each takes typed parameters, declares desired state, is
  idempotent and reports changed, ok or failed. Transport is WinRM or OpenSSH
  with `ansible_shell_type: powershell`; over OpenSSH an admin key already has
  the elevated token, so no `become`. (Ansible docs: "Windows Remote
  Management", "Windows SSH", module pages.)
- Lesson: these modules are the order vocabulary to copy, with desired-state
  semantics ("rule X exists with these fields") rather than imperative verbs.
  Agentless push means the controller holds admin credentials for every host,
  which is the shape the brief rules out.

### Intune Remediations and Endpoint Privilege Management

- The Intune Management Extension runs on the device as SYSTEM, polls the
  service outbound over HTTPS, runs a detection script and, when it reports
  non-compliant, the remediation script, and returns both outputs to the
  console. No inbound port; the device authenticates by its enrolment
  certificate. (Microsoft Learn: "Remediations", "Intune Management
  Extension".)
- Endpoint Privilege Management adds support-approved elevation: the user asks
  to elevate one file, the request reaches the console, an admin approves
  there, and the device elevates that file hash only, logged. (Microsoft
  Learn: "Endpoint Privilege Management", "support approved elevation".)
- Lesson: the mature Windows pattern for an absent administrator is a SYSTEM
  agent that dials out, executes only what its authority signed and reports
  back, and EPM is the nearest thing to "remote approval in place of a UAC
  click". It is the shape `idunn-host` already has on Raven, minus elevation.

### Windows OpenSSH and administrators

- For a member of Administrators, sshd reads only
  `%ProgramData%\ssh\administrators_authorized_keys` (ACL Administrators and
  SYSTEM), and a key-authenticated session gets the full token: UAC does not
  apply. Observed on Raven at High Mandatory Level. (Win32-OpenSSH wiki;
  issue #1942 on the file's location.)
- Forced commands: `ForceCommand` in a `Match` block is the documented route;
  whether `command=` in an authorized_keys line is honoured on Windows has an
  open report of it being ignored (Win32-OpenSSH issue #473). Unproven on
  these hosts.
- Lesson: an admin key is a remote UAC bypass with nothing but sshd's event log
  for a record. As an order transport it needs a forced command that parses
  the order on the host, and that parser is the whole boundary.

### How remote approval replaces a local UAC click

- Entra PIM: a user eligible for a role activates it for a bounded time with a
  justification, optional MFA, and optional approval by a named approver who
  is notified and approves in the portal or app; every activation is logged.
  (Microsoft Learn, "Privileged Identity Management".)
- CyberArk dual control: use of a privileged account needs an approver's
  confirmation in the vault's web or mobile client; sessions run through a
  recording proxy (PSM). (CyberArk docs: "Dual control", "Privileged Session
  Manager".)
- Teleport access requests: a role is requested with a reason; reviewers
  approve or deny, thresholds can require N approvals, and plugins put the
  request in Slack, Mattermost or PagerDuty with approve buttons; the grant is
  time-boxed and sessions are recorded. (Teleport docs: "Access Requests",
  "Access Request plugins".)
- Shared shape: the approval arrives over an out-of-band channel authenticated
  as the approver, is scoped to one request, expires, and is recorded beside
  the request. A request nobody answers is denied, never run. Shared
  assumption: an approver who answers in minutes.
- Lesson for an operator away for months: positive approval is only as live as
  her reachability. Where her approval is kept, the channel is her Matrix
  account replying in #operator to one order id (Synapse authenticates the
  sender; a Mind cannot forge her mxid without root on Yggdrasil), with an
  expiry after which the order is refused. Where it is not, the order's type
  is the defence, as for Linux (section 3).

### What this gives the Windows design

- A SYSTEM agent on each host dials out to an authority that signs every
  request; no inbound port, no Windows credential and no admin key on
  Yggdrasil (Intune, Salt minion, `idunn-host`).
- The four Ansible modules become typed, desired-state orders, and JEA's
  warning becomes the grammar rule: no order field names a free program,
  script, or path to run outside a root the host binding declares.
- A Windows host is often absent (a frozen Starfire, a reboot waiting on a
  call): an order to an unattached host is refused at once, never queued to
  fire days later.
- Approval, where kept, is a Matrix reply from her account to one order id,
  scoped and expiring; anything outside the grammar is break-glass.
