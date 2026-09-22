#!/bin/bash
# Soul's own mutants against /src/src/drivers.rs; whole lib suite per mutant.
cd /src
F=src/drivers.rs
cp $F /tmp/drivers.orig
run() {
  id=$1; shift
  cp /tmp/drivers.orig $F
  perl -0pi -e "$1" $F
  if cmp -s $F /tmp/drivers.orig; then echo "MUTANT $id: ANCHOR MISSED (no change)"; return; fi
  out=$(cargo test --lib 2>&1)
  if echo "$out" | grep -q "error\[E\|could not compile"; then echo "MUTANT $id: NO BUILD"; echo "$out" | grep -A5 "^error" | head -20; return; fi
  res=$(echo "$out" | grep "^test result")
  fails=$(echo "$out" | grep -E "^test .* FAILED$" | sed 's/^test //; s/ \.\.\. FAILED//' | tr '\n' ' ')
  if [ -n "$fails" ]; then echo "MUTANT $id: KILLED by: $fails | $res"; else echo "MUTANT $id: SURVIVED | $res"; fi
}
run S1-capdrop-only-on-bridge 's/(\n    match spec\.seccomp \{)/\n    if spec.network == ContainerNetwork::Bridge { args.retain(|a| a != "--cap-drop" && a != "ALL"); }$1/'
run S1b-readonly-dropped-on-named 's/(\n    match spec\.seccomp \{)/$1/; s/(        OsString::from\("--read-only"\),\n)//; s/(\n    if let Some\(cache_root\) = &spec\.cache_root \{\n        args\.push)/\n    if !matches!(spec.network, ContainerNetwork::Named(_)) { args.insert(args.iter().position(|a| a == "--pids-limit").unwrap(), OsString::from("--read-only")); }$1/'
run S2a-cpus-round-half 's/f64::from\(spec\.cpu_quota_percent\) \/ 100\.0/(f64::from(spec.cpu_quota_percent) \/ 100.0 * 2.0).round() \/ 2.0/'
run S2b-cpus-quota-floor50 's/f64::from\(spec\.cpu_quota_percent\) \/ 100\.0/f64::from(spec.cpu_quota_percent \/ 50 * 50) \/ 100.0/'
run S2c-cpus-round-int 's/f64::from\(spec\.cpu_quota_percent\) \/ 100\.0/(f64::from(spec.cpu_quota_percent) \/ 100.0).round()/'
run S3a-env-prefix-passthrough 's/(which collides with the Idunn source stamp"\n            \);\n)/$1            for (k, v) in &runner.environment { if k != name && k.starts_with(name.as_str()) { environment.push((k.clone(), v.clone())); } }\n/'
run S3b-env-idunn-prefix-ambient 's/(        let mut secret_mounts = Vec::new\(\);\n)/$1        for (k, v) in &runner.environment { if k.starts_with("GAMECULT_") || k.starts_with("IDUNN_") { environment.push((k.clone(), v.clone())); } }\n/'
run S4-secret-mount-writable 's/args\.push\(bind_mount\(&mount\.host_path, &mount\.container_path, true\)\?\);/args.push(bind_mount(\&mount.host_path, \&mount.container_path, false)?);/'
run S5-explicit-none-is-bridge 's/None \| Some\("none"\) => ContainerNetwork::None,/None => ContainerNetwork::None,\n            Some("none") => ContainerNetwork::Bridge,/'
run S6-cache-mount-rw-to-host-root 's/args\.push\(bind_mount\(cache_root, "\/cache", false\)\?\);/args.push(bind_mount(cache_root, "\/cache", false)?); args.push(OsString::from("--mount")); args.push(bind_mount(cache_root.parent().unwrap(), "\/cache-parent", false)?);/'
run S7-freeze-exact-skips-gitlinks 's/for \(path, fact\) in &gitlinks \{/for (path, fact) in gitlinks.iter().take(0) {/'
run S8-freeze-exact-skips-validate 's/            validate_frozen_source\(&partial\)\?;\n//'
run S8c-freeze-exact-skips-harden-and-validate 's/            harden_frozen_source\(&partial\)\?;\n            validate_frozen_source\(&partial\)\?;\n//'
run S9-secret-validation-skipped 's/                validate_runner_secret\(path, identity\)\?;\n                secret_mounts\.push/                secret_mounts.push/'
run S10-cache-root-validation-skipped 's/        if let Some\(cache_root\) = &runner\.cache_root \{\n            ensure_runner_cache_root\(cache_root, identity\)\?;\n        \}\n//'
run N1-named-plus-cache-drops-readonly 's/(        args\.push\(bind_mount\(cache_root, "\/cache", false\)\?\);\n)/$1        if matches!(spec.network, ContainerNetwork::Named(_)) { args.retain(|a| a != "--read-only"); }\n/'
run N2-secret-writable-when-plain-env-too 's/args\.push\(bind_mount\(&mount\.host_path, &mount\.container_path, true\)\?\);/args.push(bind_mount(\&mount.host_path, \&mount.container_path, spec.environment.len() == 1)?);/'
run N3-stamp-dropped-with-secret-and-env 's/(        let network = match runner\.network_profile\.as_deref\(\) \{)/        if !secret_mounts.is_empty() && environment.len() > 1 { environment.remove(0); }\n$1/'
run N4-stamp-refusal-removed 's/            ensure!\(\n                name != source_stamp\.0,\n[^\n]*\n            \);\n//'
run N5-lfs-flag-never-set 's/lfs_pointer_paths\.push\(entry\.path\.clone\(\)\);/let _ = \&entry;/'
run N6-exec-bit-dropped 's/let mode = if entry\.mode == "100755" \{ 0o755 \} else \{ 0o644 \};/let mode = 0o644;/'
run N7-symlink-as-file 's/std::os::unix::fs::symlink\(link_target, &target\)/fs::write(\&target, link_target)/'
cp /tmp/drivers.orig $F
