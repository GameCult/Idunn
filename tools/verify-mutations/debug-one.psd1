@{
    Mutations = @(
        @{
            Id   = 'debug-f1-no-bulk-fetch'
            Rule = 'debug'
            Test = 'drivers::tests::freeze_exact_bulk_fetches_every_blob_in_one_round_trip'
            Old  = @'
        self.bulk_fetch_objects(repository, &object_order)?;
'@
            New  = ''
        }
    )
}
