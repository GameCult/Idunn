@{
    Mutations = @(
        @{
            Id   = 'cut1-fix-f1-no-bulk-fetch'
            Rule = 'materialize_tree_raw bulk-fetches every blob it needs once, before reading any of them, rather than letting cat-file --batch lazily fetch them one at a time.'
            Test = 'drivers::tests::freeze_exact_is_byte_exact_and_recipe_checked'
            Old  = @'
        self.bulk_fetch_objects(repository, &object_order)?;
'@
            New  = ''
        }
        @{
            Id   = 'cut1-fix-f2-no-fsck-on-fetch'
            Rule = 'the exact-revision fetch enables transfer.fsckObjects, so Git itself refuses a fetched tree with duplicate names.'
            Test = 'drivers::tests::freeze_exact_refuses_a_duplicate_named_tree_that_would_write_outside_its_root'
            Old  = @'
        self.git([
            OsString::from("-c"),
            OsString::from("transfer.fsckObjects=true"),
            OsString::from("-C"),
            source.checkout.as_os_str().to_owned(),
            OsString::from("fetch"),
'@
            New  = @'
        self.git([
            OsString::from("-C"),
            source.checkout.as_os_str().to_owned(),
            OsString::from("fetch"),
'@
        }
        @{
            Id   = 'cut1-fix-f2-no-alias-refusal'
            Rule = 'git_tree_entries refuses a tree where one entry aliases another (a leaf name that is also another leaf''s ancestor, case-folded).'
            Test = 'drivers::tests::freeze_exact_refuses_a_duplicate_named_tree_that_would_write_outside_its_root'
            Old  = @'
        refuse_conflicting_tree_entries(&entries)?;
'@
            New  = ''
        }
        @{
            Id   = 'cut1-fix-f3-writer-follows-existing-entries'
            Rule = 'ensure_frozen_directory never treats a path that already exists as safe to write through; only a directory this freeze itself created may be reused.'
            Test = 'drivers::tests::freeze_exact_refuses_a_duplicate_named_tree_that_would_write_outside_its_root'
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
    )
}
