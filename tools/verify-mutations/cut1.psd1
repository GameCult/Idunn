# Cut 1 mutation suite: the shared Docker argv lowering and the exact-revision
# freeze primitive. Run with:
#
#   pwsh tools/eureka-mutations.ps1 -Repo . -Entries tools/verify-mutations/cut1.psd1 `
#       -Target src/drivers.rs -Test 'cargo test --lib'
#
# (the harness itself lives in the Eureka skill repo; the stopgap copies it to
# /harness/eureka-mutations.ps1 inside the verify container.)

@{
    Mutations = @(
        @{
            Id   = 'cut1-cap-drop-revert'
            Rule = 'docker_run_args always drops every Linux capability (--cap-drop ALL).'
            Test = 'drivers::tests::container_spec_lowers_to_exact_docker_argv'
            Old  = @'
        OsString::from("--cap-drop"),
        OsString::from("ALL"),
        OsString::from("--security-opt"),
'@
            New  = @'
        OsString::from("--security-opt"),
'@
        }
        @{
            Id   = 'cut1-network-default-loosening'
            Rule = 'A runner with no declared network_profile (or "none") is admitted with no network, never bridge.'
            Test = 'drivers::tests::container_spec_lowers_to_exact_docker_argv'
            Old  = @'
            None | Some("none") => ContainerNetwork::None,
'@
            New  = @'
            None => ContainerNetwork::Bridge,
            Some("none") => ContainerNetwork::None,
'@
        }
        @{
            Id   = 'cut1-cpus-function-of-input'
            Rule = '--cpus is cpu_quota_percent / 100, to two decimal places.'
            Test = 'drivers::tests::container_spec_lowers_to_exact_docker_argv'
            Old  = @'
            f64::from(spec.cpu_quota_percent) / 100.0
'@
            New  = @'
            f64::from(spec.cpu_quota_percent) / 50.0
'@
        }
        @{
            Id   = 'cut1-required-environment-only'
            Rule = 'A step container receives only the source stamp plus the names in required_environment, never every binding environment entry.'
            Test = 'drivers::tests::required_environment_is_the_only_environment'
            Old  = @'
        for name in required_environment {
'@
            New  = @'
        for name in runner.environment.keys() {
'@
        }
        # cut1-freeze-exact-recipe-check retired by the F2 fix batch: after F2,
        # freeze_exact no longer archives anything, so `materialized_recipe`
        # and `recipe_bytes` are two raw reads of the identical blob object
        # and are always equal by construction. No fixture reachable through
        # freeze_exact can now make this ensure! fail (not yet reached), so a
        # mutant dropping it would survive; the check stays as defense in
        # depth against a materialization bug, not as a killable rule here.
        # ---- Cut 1 fix batch: Soul's 13 surviving mutants (F1), minus S8 ----
        # (S8, dropping the `validate_frozen_source` call inside `freeze_exact`,
        # is not yet reached: `harden_frozen_source` runs first and unconditionally
        # forces the exact permissions and symlink-escape check `validate_frozen_source`
        # re-derives, so no fixture reachable through `freeze_exact` today can make
        # `validate_frozen_source` fail where `harden_frozen_source` did not already
        # fail first. This is a design-redundancy finding, not a coverage gap; see
        # the fix-batch report.)
        @{
            Id   = 'cut1-fix-cap-drop-only-on-bridge'
            Rule = '--cap-drop ALL is present on every network branch, not only the default.'
            Test = 'drivers::tests::docker_run_args_pins_every_network_branch'
            Old  = @'
    match spec.seccomp {
        ContainerSeccomp::Default => {}
    }
    args.extend([
'@
            New  = @'
    match spec.seccomp {
        ContainerSeccomp::Default => {}
    }
    if spec.network == ContainerNetwork::Bridge {
        args.retain(|a| a != "--cap-drop" && a != "ALL");
    }
    args.extend([
'@
        }
        @{
            Id   = 'cut1-fix-read-only-dropped-on-named'
            Rule = '--read-only is present on every network branch, including a Named network.'
            Test = 'drivers::tests::docker_run_args_pins_every_network_branch'
            Old  = @'
    args.extend([
        OsString::from("--read-only"),
        OsString::from("--pids-limit"),
'@
            New  = @'
    if !matches!(spec.network, ContainerNetwork::Named(_)) {
        args.push(OsString::from("--read-only"));
    }
    args.extend([
        OsString::from("--pids-limit"),
'@
        }
        @{
            Id   = 'cut1-fix-cpus-round-half'
            Rule = '--cpus is cpu_quota_percent / 100 to two decimal places, not rounded to the nearest 0.5.'
            Test = 'drivers::tests::docker_run_args_pins_cpu_quota_across_the_range'
            Old  = @'
        OsString::from(format!(
            "{:.2}",
            f64::from(spec.cpu_quota_percent) / 100.0
        )),
'@
            New  = @'
        OsString::from(format!(
            "{:.2}",
            (f64::from(spec.cpu_quota_percent) / 100.0 * 2.0).round() / 2.0
        )),
'@
        }
        @{
            Id   = 'cut1-fix-cpus-floor-50'
            Rule = '--cpus is cpu_quota_percent / 100 exactly, not the quota floored to the nearest 50 first.'
            Test = 'drivers::tests::docker_run_args_pins_cpu_quota_across_the_range'
            Old  = @'
        OsString::from(format!(
            "{:.2}",
            f64::from(spec.cpu_quota_percent) / 100.0
        )),
'@
            New  = @'
        OsString::from(format!(
            "{:.2}",
            f64::from(spec.cpu_quota_percent / 50 * 50) / 100.0
        )),
'@
        }
        @{
            Id   = 'cut1-fix-env-prefix-passthrough'
            Rule = 'ContainerSpec::for_step copies only the exact required environment names, never a binding entry that merely starts with one.'
            Test = 'drivers::tests::required_environment_rejects_prefix_and_ambient_passthrough'
            Old  = @'
            if let Some(value) = runner.environment.get(name) {
                environment.push((name.clone(), value.clone()));
            } else if let Some(path) = runner.secret_files.get(name) {
'@
            New  = @'
            for (k, v) in &runner.environment {
                if k != name && k.starts_with(name.as_str()) {
                    environment.push((k.clone(), v.clone()));
                }
            }
            if let Some(value) = runner.environment.get(name) {
                environment.push((name.clone(), value.clone()));
            } else if let Some(path) = runner.secret_files.get(name) {
'@
        }
        @{
            Id   = 'cut1-fix-ambient-gamecult-idunn-env'
            Rule = 'ContainerSpec::for_step never admits a binding environment entry ambiently by its GAMECULT_/IDUNN_ prefix.'
            Test = 'drivers::tests::required_environment_rejects_prefix_and_ambient_passthrough'
            Old  = @'
        let mut environment = vec![(source_stamp.0.to_owned(), source_stamp.1.to_owned())];
        let mut secret_mounts = Vec::new();
'@
            New  = @'
        let mut environment = vec![(source_stamp.0.to_owned(), source_stamp.1.to_owned())];
        let mut secret_mounts = Vec::new();
        for (k, v) in &runner.environment {
            if k.starts_with("GAMECULT_") || k.starts_with("IDUNN_") {
                environment.push((k.clone(), v.clone()));
            }
        }
'@
        }
        @{
            Id   = 'cut1-fix-secret-mount-writable'
            Rule = 'A secret mount is bound read-only.'
            Test = 'drivers::tests::docker_run_args_mounts_secrets_read_only'
            Old  = @'
        args.push(bind_mount(&mount.host_path, &mount.container_path, true)?);
'@
            New  = @'
        args.push(bind_mount(&mount.host_path, &mount.container_path, false)?);
'@
        }
        @{
            Id   = 'cut1-fix-explicit-none-is-bridge'
            Rule = 'An explicit network_profile = "none" lowers to ContainerNetwork::None, never Bridge.'
            Test = 'drivers::tests::explicit_none_network_profile_is_none_not_bridge'
            Old  = @'
            None | Some("none") => ContainerNetwork::None,
'@
            New  = @'
            None => ContainerNetwork::None,
            Some("none") => ContainerNetwork::Bridge,
'@
        }
        @{
            Id   = 'cut1-fix-cache-mount-extra-parent'
            Rule = 'A cache root adds exactly one --mount, never a second mount of its parent directory.'
            Test = 'drivers::tests::docker_run_args_mounts_cache_root_exactly_once'
            Old  = @'
        args.push(bind_mount(cache_root, "/cache", false)?);
'@
            New  = @'
        args.push(bind_mount(cache_root, "/cache", false)?);
        args.push(OsString::from("--mount"));
        args.push(bind_mount(cache_root.parent().unwrap(), "/cache-parent", false)?);
'@
        }
        @{
            Id   = 'cut1-fix-freeze-skips-gitlinks'
            Rule = 'freeze_exact materializes every declared Gitlink.'
            Test = 'drivers::tests::freeze_exact_materializes_gitlinks'
            Old  = @'
            for (path, fact) in &gitlinks {
                lfs_pointer_paths
                    .extend(self.materialize_gitlink_raw(source, path, fact, &partial)?);
            }
'@
            New  = @'
            for (path, fact) in gitlinks.iter().take(0) {
                lfs_pointer_paths
                    .extend(self.materialize_gitlink_raw(source, path, fact, &partial)?);
            }
'@
        }
        @{
            Id   = 'cut1-fix-secret-validation-skipped'
            Rule = 'ContainerSpec::for_step validates a secret file before mounting it.'
            Test = 'drivers::tests::container_spec_for_step_validates_secret_files'
            Old  = @'
                validate_runner_secret(path, identity)?;
                secret_mounts.push(SecretMount {
'@
            New  = @'
                secret_mounts.push(SecretMount {
'@
        }
        @{
            Id   = 'cut1-fix-cache-root-validation-skipped'
            Rule = 'ContainerSpec::for_step validates the cache root before admitting it.'
            Test = 'drivers::tests::container_spec_for_step_validates_cache_root'
            Old  = @'
        if let Some(cache_root) = &runner.cache_root {
            ensure_runner_cache_root(cache_root, identity)?;
        }
'@
            New  = ''
        }
        @{
            Id   = 'cut1-fix-f2-revert-to-git-archive'
            Rule = 'freeze_exact writes blobs raw from the object store; it must never revert to git archive, which applies attribute-driven transforms.'
            Test = 'drivers::tests::freeze_exact_is_byte_exact_across_every_attribute_transform'
            Old  = @'
        fs::create_dir_all(destination)
            .with_context(|| format!("creating {}", destination.display()))?;
        let entries = self.git_tree_entries(repository, revision)?;
        let blob_objects: Vec<String> = entries
            .iter()
            .filter(|entry| entry.kind == "blob")
            .map(|entry| entry.object.clone())
            .collect();
        let blobs = self.read_blobs(repository, &blob_objects)?;
        let mut lfs_pointer_paths = Vec::new();
        for entry in &entries {
            let target = destination.join(&entry.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            match entry.mode.as_str() {
                "100644" | "100755" => {
                    let content = blobs
                        .get(&entry.object)
                        .context("Git cat-file --batch omitted a requested blob")?;
                    fs::write(&target, content)
                        .with_context(|| format!("writing {}", target.display()))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = if entry.mode == "100755" { 0o755 } else { 0o644 };
                        fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
                    }
                    if is_lfs_pointer(content) {
                        lfs_pointer_paths.push(entry.path.clone());
                    }
                }
                "120000" => {
                    let content = blobs
                        .get(&entry.object)
                        .context("Git cat-file --batch omitted a requested blob")?;
                    let link_target = std::str::from_utf8(content)
                        .context("frozen source symlink target is not UTF-8")?;
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(link_target, &target)
                        .with_context(|| format!("creating symlink {}", target.display()))?;
                }
                "160000" => {
                    // Gitlinks are materialized by the caller, which knows
                    // each one's admitted origin; this entry only reserves
                    // the directory.
                }
                other => bail!(
                    "frozen source tree entry {} has an unsupported mode {other}",
                    entry.path.display()
                ),
            }
        }
        Ok(lfs_pointer_paths)
'@
            New  = @'
        fs::create_dir_all(destination)
            .with_context(|| format!("creating {}", destination.display()))?;
        let mut archive = self.git_command([
            OsString::from("-C"),
            repository.as_os_str().to_owned(),
            OsString::from("archive"),
            OsString::from("--format=tar"),
            OsString::from(revision),
        ])?;
        archive.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut archive = archive.spawn().context("starting Git archive")?;
        let archive_stdout = archive.stdout.take().context("Git archive has no stdout")?;
        let mut extractor = Command::new("/bin/tar");
        extractor
            .args([
                OsString::from("--extract"),
                OsString::from("--file=-"),
                OsString::from("--directory"),
                destination.as_os_str().to_owned(),
                OsString::from("--no-same-owner"),
            ])
            .stdin(Stdio::from(archive_stdout))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let extractor = extractor.spawn().context("starting tar extractor")?;
        let archive_output = archive.wait_with_output().context("waiting for Git archive")?;
        let extractor_output = extractor.wait_with_output().context("waiting for tar extractor")?;
        ensure!(archive_output.status.success(), "Git archive failed");
        ensure!(extractor_output.status.success(), "tar extraction failed");
        Ok(Vec::new())
'@
        }
    )
}
