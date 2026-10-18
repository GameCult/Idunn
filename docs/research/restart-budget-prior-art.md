# Restart-budget and handover prior art (Eyes facts pass)

Serves two operator questions in Eureka campaign `idunn-reincarnation` (session idunn-reincarnation-2026-10-18):

- `idunn-reincarnation:question:budget-charge` (what charges a target's continuity restart budget)
- `idunn-reincarnation:question:handover-pre-fencing` (who acts when the incumbent dies while a deployment is before Fencing)

Facts and evidence pointers only. No recommendation. Gathered 2026-10-18 from official docs and upstream source (main branches at fetch time). Quotes are under 15 words. "Not found" means the primary source read does not state it, not that it is false.

## 1. Kubernetes

### 1.1 CrashLoopBackOff schedule, cap, reset (kubelet)

- Docs, pod lifecycle, "Container restarts": backoff is "exponential backoff delay (10s, 20s, 40s, ...)", "capped at 300 seconds (5 minutes)". Reset: after the container has run "for 10 minutes without any problems". https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/
- Source pkg/kubelet/kubelet.go (https://github.com/kubernetes/kubernetes/blob/master/pkg/kubelet/kubelet.go): `initialCrashLoopBackOff = time.Second * 10`; max is `MaxCrashLoopBackOff = v1beta1.MaxContainerBackOff` (value lives in the kubelet config API package, not re-read; docs say 300 s). Under alpha gate `ReduceDefaultCrashLoopBackOffDecay`: initial 1 s, max 60 s. Gate `KubeletCrashLoopBackOffMax` lets the config set the max (`CrashLoopBackOff.MaxContainerRestartPeriod`).
- Reset rule in code: `klet.crashLoopBackOff.HasExpiredFunc` returns `eventTime.Sub(lastUpdate) > 600*time.Second` (kubelet.go ~line 1102). I.e. the entry resets when more than 600 s have passed since the entry was last updated (last restart event). The client-go default is 2*maxDuration; the kubelet overrides it.
- Growth: client-go `Backoff.Next` doubles (`delay := entry.backoff * 2`) and caps with `min(delay, p.maxDuration)` (https://github.com/kubernetes/client-go/blob/master/util/flowcontrol/backoff.go).
- Window anchor: the exited container's `FinishedAt`; keyed per pod+container; event text "Back-off restarting failed container" (pkg/kubelet/kuberuntime/kuberuntime_manager.go, `doBackOff`).
- The backoff table is an in-memory object created in `NewMainKubelet` (`flowcontrol.NewBackOff(base, boMax)`); no persistence found in the code read. Not found: any doc statement that a node reboot does or does not reset or count toward it. Docs only say restart policy "only applies to restarts that make replacement containers inside the same Pod and running on the same node." Docs on kubelet restarts: pods "continue to run even during the restart" (kubelet restart, not node reboot).

### 1.2 Probes

Docs (pod lifecycle "Container probes"; https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/):

- Liveness: past the failure threshold "the kubelet restarts that container" (per pod restartPolicy).
- Readiness: the EndpointSlice controller "removes the Pod's IP address from the EndpointSlices"; the container is not restarted.
- Startup: until it succeeds "Kubernetes does not execute liveness or readiness probes"; on failure "the kubelet kills the container", subject to restart policy.
- Backoff keys on a container having exited (`doBackOff` reads the last exited container), so crash, liveness kill and startup-probe kill all reach it; readiness failure leaves the container running and does not. The docs do not describe backoff per probe type.

### 1.3 Dependency down

Docs, same page, "Dependencies and init containers": init containers "run only once (if successful), during Pod startup"; failed init containers are restarted by the kubelet when pod restartPolicy is OnFailure or Always. Readiness gates: pod is ready only when all `readinessGates` conditions are True. No general documented rule for a main container whose external dependency is down: an app that exits is restarted under backoff; an app that stays up and fails readiness is only removed from endpoints.

### 1.4 RollingUpdate and the old ReplicaSet

- Deployment docs (https://kubernetes.io/docs/concepts/workloads/controllers/deployment/): "Once new Pods are ready, old ReplicaSet can be scaled down further"; at the end "all old ReplicaSets is scaled to 0". maxUnavailable and maxSurge default 25%. progressDeadlineSeconds default 600; when exceeded, "Kubernetes takes no action on a stalled Deployment" beyond the Progressing=False condition.
- ReplicaSet docs: it fulfills its purpose by "creating and deleting Pods as needed to reach the desired number." https://kubernetes.io/docs/concepts/workloads/controllers/replicaset/
- Deployment controller source, pkg/controller/deployment/rolling.go (https://github.com/kubernetes/kubernetes/blob/master/pkg/controller/deployment/rolling.go): `reconcileOldReplicaSets` computes `minAvailable = replicas - maxUnavailable`, runs `cleanupUnhealthyReplicas` first (comment: "Clean up unhealthy replicas first, otherwise unhealthy replicas will block deployment"), then `scaleDownOldReplicaSetsForRollingUpdate`. The Deployment controller decides old-RS replica counts during rollout, scaling unavailable ones down first.
- Layering: restarts of a dying container are done by the kubelet (restartPolicy with backoff), not the ReplicaSet. The ReplicaSet recreates a pod only when the pod object is gone or terminal. Not found: a statement in those words that the old ReplicaSet keeps recreating its dying pods until scaled down.

## 2. Nomad

### 2.1 restart block (https://developer.hashicorp.com/nomad/docs/job-specification/restart)

- `attempts` restarts allowed within `interval`; interval "starts when the first task starts". Defaults: service/system attempts 2, interval 30m, delay 15s, mode fail; batch attempts 3, interval 24h, delay 15s, mode fail. Delay has up to 25% jitter.
- `mode = "fail"` (default): after attempts are used the allocation is marked failed and goes to `reschedule`. `mode = "delay"`: "The task waits another full interval before restarting"; the allocation is never failed by restarts.
- Source client/allocrunner/taskrunner/restarts/restarts.go (https://github.com/hashicorp/nomad/blob/main/client/allocrunner/taskrunner/restarts/restarts.go): in-memory `RestartTracker` with `count` and `startTime` set at construction (`time.Now()`); a new interval resets count; `count > Attempts` returns TaskNotRestarting in fail mode, or TaskRestarting with delay to interval end in delay mode. Constructed in `NewTaskRunner` (task_runner.go ~line 468) from the alloc's job type and the task group's policy. `SetKilled` returns TaskKilled without counting. A restart triggered with failure=false (operator/API) returns TaskRestarting without consulting the policy. Unrecoverable start errors return TaskNotRestarting.
- Client restart: restarts.go keeps no persisted counter. `TaskRunner.Restore` (task_runner.go ~line 1244) restores local state and the driver handle (`RecoverTask`); if the handle cannot be restored for a non-system job the task "will not run until server is contacted" (`waitOnServers`). The tracker is rebuilt at task-runner construction. Persisted `TaskState` carries restart counts separately; `GetState` does not read them.

### 2.2 reschedule block (https://developer.hashicorp.com/nomad/docs/job-specification/reschedule)

- Applies when an allocation fails to set up or its tasks fail more than `restart` allows. Drains trigger migrations and lost nodes trigger replacements, so reschedule is ignored in both cases. Not applied to system/sysbatch.
- Service defaults: `delay_function = exponential`, delay starting 30s, `max_delay` 1h, `unlimited = true`. Batch defaults: 1 attempt per 24h, constant 5s, unlimited false. `delay_function`: constant, exponential (doubling), fibonacci. `max_delay` ignored for constant. Minimum delay 1 s; docs warn very low delay with `unlimited = true` can cause runaway rescheduling.
- With `unlimited = true`: "The `progress_deadline` in the `update` block still applies".

### 2.3 Deployments and restarts of old allocations

- Update block (https://developer.hashicorp.com/nomad/docs/job-specification/update): `max_parallel` 1, `min_healthy_time` 10s, `healthy_deadline` 5m, `progress_deadline` 10m ("the deployment is marked as failed"), `auto_revert` false, `canary` 0. The page says nothing on restarts of old allocations. Not found: docs on deployment/restart interaction.
- Restart is client-local ("Restarts happen on the client that is running the task"); deployment is server-side. Reconciler source (scheduler/reconciler/reconcile_cluster.go, `placeAllocs`): when the deployment is not place-ready (paused/failed/canarying), rescheduling of failed allocs still proceeds, with comment about avoiding "odd user experiences", except where the failed alloc is part of the failed deployment (`partOfFailedDeployment` skips it).

## 3. systemd

### 3.1 Start limit (https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html)

- A unit started more than `StartLimitBurst=` times within `StartLimitIntervalSec=` may not start again until the interval passes. Applies to all starts, including manual, not only `Restart=`; it is checked after condition checks, so failing conditions do not count. Units with `Restart=` that hit it are no longer restarted automatically; `systemctl reset-failed` flushes the counter. `StartLimitAction=` default none. Counters are cleared when the unit is unloaded.
- Source: `unit_test_start_limit` in src/core/unit.c (`ratelimit_below(&u->start_ratelimit)`; on hit logs "Start request repeated too quickly", returns -ECANCELED); `unit_reset_failed` calls `ratelimit_reset`.

### 3.2 Job result "dependency" and the start limit

- src/core/unit.c `unit_start` order: conditions (-ECOMM), asserts (-EPROTO), `unit_verify_deps` (-ENOLINK, "The necessary dependencies are not fulfilled"), then `UNIT_VTABLE(u)->test_startable`. For services, `service_test_startable` (src/core/service.c ~6381) calls `unit_test_start_limit`. A start failing on unfulfilled dependencies returns before the start-limit counter is touched.
- src/core/job.c: `-ENOLINK` maps to `JOB_DEPENDENCY` (`job_run_and_invalidate`); `job_fail_dependencies` also finishes dependent START/VERIFY_ACTIVE jobs with `JOB_DEPENDENCY` ("Dependency failed for %s."). `n_failed_jobs` increments only for JOB_FAILED, JOB_INVALID, JOB_FROZEN, JOB_CONCURRENCY. JOB_DEPENDENCY triggers `OnFailure=` handling.
- Requires=: if the required unit fails to activate and the dependent has After= on it, the dependent is not started; without After= it may still start. Wants=: failure "has no impact on the validity of the transaction as a whole." Dependent is stopped when the required unit is explicitly stopped or restarted; not stopped if the required unit stops on its own.

### 3.3 Restart= and dependency failure

- systemd.service(5) (https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html): the restart table is by exit cause of the service's own process (exit code, signal, timeout, watchdog, OOM); no row for dependency failure. Page: "service restart is subject to unit start rate limiting." Not found: a doc statement on whether Restart= acts on a dependency failure. From the code above, a dependency-failed start job finishes with JOB_DEPENDENCY and never passes through the service's own failure state (inference, not an explicit doc statement).
- Backoff: `RestartSec=` default 100ms; `RestartSteps=` with `RestartMaxDelaySec=` give exponential growth to a cap (doc example 10s, 20s, 40s, 80s, 160s, 160s...). `RestartMaxDelaySec` default infinity; both must be set. `RestartRandomizedDelaySec=` (v262) adds jitter.

### 3.4 Transient units across reboot

- systemd.unit(5) path table (man/systemd.unit.xml ~lines 403, 465): "Dynamic configuration for transient units" lives in `/run/systemd/transient` (system) and `$XDG_RUNTIME_DIR/systemd/transient` (user). That `/run` is volatile is not stated in this file.
- systemd-run docs: failed transient units are "kept in memory until the user explicitly resets their failure state"; `--collect` is `CollectMode=inactive-or-failed`. systemd-run and https://systemd.io/TRANSIENT-SETTINGS/ do not mention reboot. Not found: an explicit sentence on survival across reboot.

## 4. Other supervisors

### 4.1 Erlang/OTP supervisor (https://www.erlang.org/doc/apps/stdlib/supervisor.html, https://www.erlang.org/doc/system/sup_princ.html)

- `intensity` (MaxR) and `period` (MaxT), defaults 1 and 5. If more than MaxR restarts occur within the last MaxT seconds, the supervisor "terminates all the child processes and then itself" (exit reason `shutdown`). The parent supervisor then restarts it or itself terminates; failure escalates up the tree. Stated purpose: prevent a process from repeatedly crashing for the same reason and being restarted indefinitely.
- Restart types: `permanent` (always), `transient` (only abnormal exit), `temporary` (never). Every restart the strategy performs counts toward intensity; the docs describe no per-cause charging and no backoff delay. Multi-level: total restarts before the top gives up is roughly the product of intensities above the failing child. Strategy `rest_for_one` restarts the failed child and those started after it.

### 4.2 runit and s6

- runit `runsv` (https://smarden.org/runit/runsv.8.html): if ./run exits immediately runsv "waits a second" before restarting; no rate limit or retry cap described; keeps restarting unless told otherwise (`o`, `d`, `x`).
- s6-supervise (https://skarnet.org/software/s6/s6-supervise.html): "There is a minimum 1-second delay between two ./run spawns." A ./finish script exiting 125 makes supervise stop restarting; otherwise it restarts indefinitely. The give-up decision is delegated to a user script, not a built-in counter.

### 4.3 Borg (Verma et al., EuroSys 2015; https://research.google/pubs/large-scale-cluster-management-at-google-with-borg/, PDF text read)

- Borglet "restarts them if they fail"; Borg "monitors the health-check URL and restarts tasks that do not respond promptly" (sections 2.6, 3.3).
- Section 4 (Availability): Borg "automatically reschedules evicted tasks"; "limits the allowed rate of task disruptions and the number of tasks from a job that can be simultaneously down"; rate-limits finding new places for tasks from unreachable machines because it cannot tell machine failure from network partition; "avoids repeating task::machine pairings that cause task or machine crashes".
- Section 2.3 (updates): "Updates are generally done in a rolling fashion", with a limit on task disruptions an update causes; changes that would exceed it "are skipped".
- Not found in the Borg paper: a crash-loop backoff schedule, a restart budget, or any distinction between charging task faults and dependency faults. Omega paper not fetched.

## 5. Facts grouped by the brief's sub-topics

- (a) Charging only body faults: Kubernetes backoff keys on container exit regardless of cause (readiness failure does not exit and does not advance it). systemd runs dependency verification (-ENOLINK, JOB_DEPENDENCY) before the start-limit test, and JOB_DEPENDENCY is not in `n_failed_jobs`. Nomad's tracker counts any recoverable task failure or start error, with no dependency classification. OTP counts every restart. Borg avoids repeating task::machine pairings that cause crashes (placement, not budget).
- (b) Give up vs capped forever: OTP gives up and escalates. Kubernetes caps at 300 s with no give-up. Nomad restart fail-mode gives up locally, then reschedule continues (service default unlimited, exponential to 1 h); delay-mode never fails. systemd start limit stops auto-restart until the interval passes or reset-failed; `RestartMaxDelaySec` default infinity. runit restarts forever at 1 s; s6 restarts forever unless finish exits 125.
- (c) Rollout vs restart supervisor: Kubernetes splits by layer (kubelet restarts containers; ReplicaSet maintains pod count; Deployment controller scales old RS during rollout). Nomad splits by node vs server (client restart tracker vs server reconciler; reconciler skips rescheduling allocs of a failed deployment). Borg bounds update-induced disruptions with a rate limit.

## 6. Memory claims vs sources

budget-charge:

1. "Kubernetes separates readiness from liveness; only liveness feeds CrashLoopBackOff": CORRECTED. Readiness never restarts and never advances backoff (confirmed). Backoff keys on any container exit (crash, liveness kill, startup-probe kill), so it is not fed only by liveness.
2. "CrashLoopBackOff caps at 5 min": CONFIRMED (docs 300 s; initial 10 s, doubling). Reset: docs say 10 min healthy; code says 600 s since the entry's last update. 60 s cap only under alpha gate.
3. "systemd does not run a unit whose required dependency failed (job result 'dependency')": CONFIRMED with qualifier: Requires= plus After= blocks the start; Requires= without After= may still start; Wants= does not block.
4. "Nomad reschedules with capped exponential delay": CONFIRMED for service jobs (exponential from 30 s, max_delay 1 h, unlimited by default); batch default is constant 5 s, 1 attempt per 24 h.
5. Dependency failure vs start limit (not in the question's claims, material): systemd checks dependencies before counting a start (confirmed by code order). Whether Restart= acts on dependency failure: UNSUPPORTED by docs.
6. Whether restarts after a node reboot count toward CrashLoopBackOff: UNSUPPORTED by docs; code read shows the kubelet backoff map in memory. Nomad's restart tracker is likewise in memory and rebuilt on client restart.

handover-pre-fencing:

1. "In a Kubernetes rolling update the old ReplicaSet keeps recreating its dying pods until the rollout scales it down": PARTLY CONFIRMED, PARTLY CORRECTED. ReplicaSets do create pods to reach their desired count, and the Deployment controller scales old RS down, unhealthy first. Container restarts are the kubelet's, not the ReplicaSet's; the RS recreates only pods that are gone.
2. "Nomad restarts old allocations by their own restart policy, independent of the deployment": PARTLY SUPPORTED. Restart is client-local using the alloc's task-group policy (code), so independent of the server-side deployment; no doc statement on the interaction. Server-side, failed allocs are rescheduled even when the deployment is not place-ready, except allocs of a failed deployment.
3. Any source saying an aborted or stalled rollout automatically hands the slot back to the old generation: UNSUPPORTED (Kubernetes takes no action on a stalled Deployment; Nomad `auto_revert` defaults false).
