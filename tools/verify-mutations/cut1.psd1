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
        @{
            Id   = 'cut1-freeze-exact-recipe-check'
            Rule = 'freeze_exact refuses when the archived recipe file differs from the recipe blob it read from the same tree.'
            Test = 'drivers::tests::freeze_exact_rejects_a_recipe_the_archive_transforms'
            Old  = @'
            ensure!(
                materialized_recipe == recipe_bytes,
                "frozen recipe differs from the selected tree's recipe blob"
            );
'@
            New  = ''
        }
    )
}
