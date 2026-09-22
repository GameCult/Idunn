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
'@
            New  = @'
            for (path, fact) in gitlinks.iter().take(0) {
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
        ensure_frozen_directory(frozen_root, created_dirs, destination)?;
        let entries = self.git_tree_entries(repository, revision)?;
'@
            New  = @'
        ensure_frozen_directory(frozen_root, created_dirs, destination)?;
        {
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
            return Ok(Vec::new());
        }
        #[allow(unreachable_code)]
        let entries = self.git_tree_entries(repository, revision)?;
'@
        }
        @{
            Id   = 'cut1-fix-f1-no-bulk-fetch'
            Rule = 'materialize_tree_raw bulk-fetches every blob it needs once, before reading any of them, rather than letting cat-file --batch lazily fetch them one at a time.'
            Test = 'drivers::tests::freeze_exact_bulk_fetches_every_blob_in_one_round_trip'
            Old  = @'
        self.bulk_fetch_objects(repository, &object_order)?;
'@
            New  = ''
        }
        # cut1-fix-f2-no-fsck-on-fetch: not yet reached. `transfer.fsckObjects`
        # on the explicit fetch was meant to be an independent layer-(a)
        # defense beside `refuse_conflicting_tree_entries`, but every tree
        # constructible with `git hash-object --literally` that fsck's
        # `duplicateEntries` check rejects also has a literal duplicate path
        # or a case-folded ancestor collision that `refuse_conflicting_tree_entries`
        # rejects first and unconditionally, before the fetch's fsck result
        # can matter. No fixture built from this crate's own tools has yet
        # separated the two. The flag stays (fsck's other structural checks
        # are broader than this crate's own parser, and a future weakening of
        # `refuse_conflicting_tree_entries` should still be caught here), but
        # it is not pinned by a mutation entry until a fixture reaches it: for
        # example a tree that is well-formed by this crate's own rules (no
        # duplicate or aliased path) but that fsck refuses for an unrelated
        # structural reason.
        @{
            Id   = 'cut1-fix-f2-no-alias-refusal'
            Rule = 'git_tree_entries refuses a tree where one entry aliases another (a leaf name that is also another leaf''s ancestor, case-folded).'
            Test = 'drivers::tests::freeze_exact_refuses_a_case_folding_collision_between_two_regular_files'
            Old  = @'
        refuse_conflicting_tree_entries(&entries)?;
'@
            New  = ''
        }
        @{
            Id   = 'cut1-fix-f3-writer-follows-existing-entries'
            Rule = 'ensure_frozen_directory never treats a path that already exists as safe to write through; only a directory this freeze itself created may be reused.'
            Test = 'drivers::tests::ensure_frozen_directory_refuses_an_existing_entry_it_did_not_create'
            Old  = @'
    match fs::symlink_metadata(path) {
        Ok(metadata) => bail!(
'@
            New  = @'
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(metadata) => bail!(
'@
        }
        @{
            Id   = 'cut1-fix-f4-harden-pass-removed'
            Rule = 'freeze_exact hardens the frozen tree (root-owned 0444/0555, symlinks refused if they escape) before it is ever published.'
            Test = 'drivers::tests::freeze_exact_refuses_an_absolute_or_escaping_symlink'
            Old  = @'
            harden_frozen_source(&partial)?;
            let snapshot_sha256 = frozen_source_sha256(&partial)?;
'@
            New  = @'
            let snapshot_sha256 = frozen_source_sha256(&partial)?;
'@
        }
        @{
            Id   = 'cut1-fix-f5-lexical-symlink-check'
            Rule = 'a frozen-source symlink is validated by resolving its whole chain on the filesystem, not by lexically counting ".." components.'
            Test = 'drivers::tests::freeze_exact_refuses_a_symlink_chain_that_escapes_through_a_self_referential_directory'
            Old  = @'
    let canonical_root = root.canonicalize().context("resolving frozen source root")?;
    let canonical_target = path.canonicalize().with_context(|| {
        format!("resolving frozen source symlink chain at {}", path.display())
    })?;
    ensure!(
        canonical_target.starts_with(&canonical_root),
        "frozen source symlink escapes its root"
    );
'@
            New  = @'
    let _ = root;
    let _ = path;
'@
        }
        @{
            Id   = 'cut1-fix-n1-readonly-dropped-named-plus-cache'
            Rule = '--read-only is present on a Named network even when a cache root is also mounted.'
            Test = 'drivers::tests::docker_run_args_keeps_read_only_with_a_named_network_and_a_cache_root'
            Old  = @'
        args.push(bind_mount(cache_root, "/cache", false)?);
'@
            New  = @'
        args.push(bind_mount(cache_root, "/cache", false)?);
        if matches!(spec.network, ContainerNetwork::Named(_)) {
            args.retain(|a| a != "--read-only");
        }
'@
        }
        @{
            Id   = 'cut1-fix-n2-secret-writable-with-plain-env'
            Rule = 'a secret mount is read-only regardless of how many plain environment entries the runner also carries.'
            Test = 'drivers::tests::docker_run_args_keeps_secret_mounts_read_only_alongside_plain_environment'
            Old  = @'
        args.push(bind_mount(&mount.host_path, &mount.container_path, true)?);
'@
            New  = @'
        args.push(bind_mount(&mount.host_path, &mount.container_path, spec.environment.len() == 1)?);
'@
        }
        @{
            Id   = 'cut1-fix-n3-stamp-dropped-with-secret-and-env'
            Rule = 'the Idunn source stamp is never dropped from ContainerSpec::for_step''s environment, no matter what else the runner carries.'
            Test = 'drivers::tests::container_spec_for_step_keeps_the_source_stamp_with_a_secret_and_plain_environment'
            Old  = @'
        let network = match runner.network_profile.as_deref() {
'@
            New  = @'
        if !secret_mounts.is_empty() && environment.len() > 1 {
            environment.remove(0);
        }
        let network = match runner.network_profile.as_deref() {
'@
        }
        @{
            Id   = 'cut1-fix-n5-lfs-flag-never-set'
            Rule = 'a Git LFS pointer file materialized into a frozen tree is recorded in the returned LFS-pointer list.'
            Test = 'drivers::tests::freeze_exact_sets_the_lfs_pointer_flag_for_a_pointer_file'
            Old  = @'
                    if is_lfs {
                        lfs_pointer_paths.push(first.path.clone());
                    }
'@
            New  = @'
                    let _ = is_lfs;
'@
        }
    )
}
