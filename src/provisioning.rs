use crate::brake::{Stop, StopState, process_uid, read_stop, replace_stop};
use anyhow::{Context, Result, anyhow, bail, ensure};
use cultcache_rs::{
    CultCacheEnvelope, CultCacheExpectedEnvelope, DatabaseEntry, SingleFileMessagePackBackingStore,
};
use cultnet_rs::{
    GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA, GameCultProviderHealthIdentity,
    GameCultServiceTrustAnchorRecord, IDUNN_AUTHENTICATED_PROVIDER_HEALTH_PROJECTION_SCHEMA,
    IDUNN_AUTHENTICATED_PROVIDER_HEALTH_PROJECTION_SIGNING_PURPOSE,
    IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA, 
    IDUNN_LIFECYCLE_BRAKE_AUTHORITY,
    IDUNN_LIFECYCLE_BRAKE_SCHEMA, IDUNN_LIFECYCLE_BRAKE_SCOPE, IdunnDaemonHealthTrustBindingRecord,
    IdunnLifecycleBrakeObservation,
    IdunnLifecycleBrakeRecord, IdunnServiceIdentity, OdinTopologyIdentity, ServiceIdentityProfile,
    ServiceIdentityTrustAnchor, derive_service_identity_id, enroll_service_identity_at,
    evaluate_idunn_continuity_restart,
    export_service_identity_trust_anchor, open_service_identity_at,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

trait RootDistributionDocument: DatabaseEntry {
    fn root_key(&self) -> &str;
}

impl RootDistributionDocument for IdunnDaemonHealthTrustBindingRecord {
    fn root_key(&self) -> &str {
        &self.binding_id
    }
}

impl RootDistributionDocument for GameCultServiceTrustAnchorRecord {
    fn root_key(&self) -> &str {
        &self.trust_anchor_id
    }
}

pub fn run(args: impl IntoIterator<Item = String>) -> Result<()> {
    let mut args = args.into_iter();
    let command = args.next().ok_or_else(|| anyhow!(usage()))?;
    let options = parse_options(args)?;
    match command.as_str() {
        "enroll-provider-health-identity" => {
            let public_key = enroll_provider_health_identity(&options)?;
            println!("{public_key}");
        }
        "provider-health-public-key" => {
            let public_key = provider_health_public_key(&options)?;
            println!("{public_key}");
        }
        "provider-health-identity-id" => {
            println!(
                "{}",
                identity_id::<GameCultProviderHealthIdentity>(&options)?
            );
        }
        "host-actuator-identity-id" => {
            println!(
                "{}",
                identity_id::<crate::host_actuator::IdunnHostActuatorIdentity>(&options)?
            );
        }
        "export-provider-health-public-anchor" => export_identity_anchor::<
            GameCultProviderHealthIdentity,
        >(
            &options, "provider health public anchor"
        )?,
        "enroll-idunn-identity" => enroll_identity::<IdunnServiceIdentity>(&options)?,
        "enroll-host-actuator-identity" => {
            enroll_identity::<crate::host_actuator::IdunnHostActuatorIdentity>(&options)?
        }
        "export-host-actuator-public-anchor" => export_identity_anchor::<
            crate::host_actuator::IdunnHostActuatorIdentity,
        >(&options, "host actuator public anchor")?,
        "export-idunn-public-anchor" => {
            export_identity_anchor::<IdunnServiceIdentity>(&options, "Idunn public anchor")?
        }
        "enroll-odin-topology-identity" => enroll_identity::<OdinTopologyIdentity>(&options)?,
        "export-odin-topology-public-anchor" => {
            export_identity_anchor::<OdinTopologyIdentity>(&options, "Odin topology public anchor")?
        }
        "deployment-brake-engage" => deployment_brake_write(&options, StopState::Engaged)?,
        "deployment-brake-clear" => deployment_brake_write(&options, StopState::Clear)?,
        "deployment-brake-status" => deployment_brake_status(&options)?,
        "lifecycle-brake-engage" => lifecycle_brake_engage(&options)?,
        "lifecycle-brake-release" => lifecycle_brake_release(&options)?,
        "lifecycle-brake-status" => lifecycle_brake_status(&options)?,
        "create-daemon-health-trust-binding" => create_health_binding(&options)?,
        "add-daemon-health-trust-binding" => add_health_binding(&options)?,
        "require-daemon-health-release-binding" => require_health_binding_release(&options)?,
        "rotate-daemon-health-trust-signer" => rotate_health_binding_signer(&options)?,
        "validate-daemon-health-trust-binding" => {
            require_only(&options, &["input"])?;
            validate_health_binding_store(&path(&options, "input")?)?;
        }
        "create-provider-projection-trust-anchor" => create_projection_anchor(&options)?,
        "validate-provider-projection-trust-anchor" => validate_projection_anchor(&options)?,
        _ => bail!("unknown command {command:?}\n{}", usage()),
    }
    Ok(())
}

fn enroll_identity<P: ServiceIdentityProfile>(options: &BTreeMap<String, String>) -> Result<()> {
    require_only(options, &["private-store"])?;
    enroll_service_identity_at::<P>(&path(options, "private-store")?)?;
    Ok(())
}

fn export_identity_anchor<P: ServiceIdentityProfile>(
    options: &BTreeMap<String, String>,
    label: &str,
) -> Result<()> {
    require_only(options, &["private-store", "public-anchor"])?;
    let private = path(options, "private-store")?;
    let public = path(options, "public-anchor")?;
    reject_alias(&private, &public)?;
    refuse_existing(&public, label)?;
    let signer = open_service_identity_at::<P>(&private)?;
    export_service_identity_trust_anchor(&signer, &public)?;
    Ok(())
}

fn enroll_provider_health_identity(options: &BTreeMap<String, String>) -> Result<String> {
    require_only(options, &["private-store"])?;
    let signer = enroll_service_identity_at::<GameCultProviderHealthIdentity>(&path(
        options,
        "private-store",
    )?)?;
    encode_public_key(&signer.entry().public_key)
}

/// The profile-derived identity id of an enrolled private store, read-only.
/// This is the value an operator binding names as `expected_signer_identity_id`
/// or that a host binding is keyed by; it is derived, never typed.
fn identity_id<P: ServiceIdentityProfile>(options: &BTreeMap<String, String>) -> Result<String> {
    require_only(options, &["private-store"])?;
    let signer = open_service_identity_at::<P>(&path(options, "private-store")?)?;
    Ok(signer.entry().identity_id.clone())
}

fn provider_health_public_key(options: &BTreeMap<String, String>) -> Result<String> {
    require_only(options, &["private-store"])?;
    let signer = open_service_identity_at::<GameCultProviderHealthIdentity>(&path(
        options,
        "private-store",
    )?)?;
    encode_public_key(&signer.entry().public_key)
}

/// Engage or clear the deployment stop. The author is the process uid's login
/// name and the time is this verb's clock: neither is an argument.
fn deployment_brake_write(options: &BTreeMap<String, String>, state: StopState) -> Result<()> {
    require_only(options, &["store", "runtime-id", "reason"])?;
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )?;
    let record = replace_stop(
        &path(options, "store")?,
        required(options, "runtime-id")?,
        state,
        required(options, "reason")?,
        now,
    )?;
    println!(
        "{} runtime={} by={} sequence={}",
        record.state.name(),
        record.runtime_id,
        record.by,
        record.sequence
    );
    Ok(())
}

/// Print the stop and exit nonzero unless deployments admit, so a script can
/// gate on it as it could on the retired exact-actuation validation.
fn deployment_brake_status(options: &BTreeMap<String, String>) -> Result<()> {
    allow_only(options, &["store", "runtime-id", "owner-user"])?;
    // A steward's store is owned by the steward: the operator names that user
    // to read it, and the owner check is the same one Idunn makes.
    let owner = match options.get("owner-user") {
        Some(user) => crate::steward_orders::user_uid(user)?,
        None => process_uid(),
    };
    let stop = read_stop(&path(options, "store")?, required(options, "runtime-id")?, owner);
    match &stop {
        Stop::Absent => println!("absent: deployments admit"),
        Stop::Unreadable(why) => println!("unreadable: deployments wait ({why})"),
        Stop::Clear(record) | Stop::Engaged(record) => println!(
            "{} runtime={} by={} at={} sequence={} reason={}",
            record.state.name(),
            record.runtime_id,
            record.by,
            record.at_unix_millis,
            record.sequence,
            record.reason
        ),
    }
    ensure!(stop.admits(), "the deployment brake does not admit deployments");
    Ok(())
}

fn lifecycle_brake_engage(options: &BTreeMap<String, String>) -> Result<()> {
    require_only(
        options,
        &[
            "store",
            "runtime-id",
            "target",
            "reason",
            "updated-at-unix-millis",
        ],
    )?;
    let updated = parse_u64(options, "updated-at-unix-millis")?;
    let record = IdunnLifecycleBrakeRecord {
        schema_version: IDUNN_LIFECYCLE_BRAKE_SCHEMA.into(),
        authority: IDUNN_LIFECYCLE_BRAKE_AUTHORITY.into(),
        runtime_id: required(options, "runtime-id")?.into(),
        target: required(options, "target")?.into(),
        scope: IDUNN_LIFECYCLE_BRAKE_SCOPE.into(),
        status: "engaged".into(),
        reason: required(options, "reason")?.into(),
        updated_at_unix_millis: updated,
        released_until_unix_millis: None,
    };
    replace_lifecycle_brake(&path(options, "store")?, record)
}

fn lifecycle_brake_release(options: &BTreeMap<String, String>) -> Result<()> {
    allow_only(
        options,
        &[
            "store",
            "runtime-id",
            "target",
            "reason",
            "updated-at-unix-millis",
            "released-until-unix-millis",
        ],
    )?;
    let updated = parse_u64(options, "updated-at-unix-millis")?;
    let record = IdunnLifecycleBrakeRecord {
        schema_version: IDUNN_LIFECYCLE_BRAKE_SCHEMA.into(),
        authority: IDUNN_LIFECYCLE_BRAKE_AUTHORITY.into(),
        runtime_id: required(options, "runtime-id")?.into(),
        target: required(options, "target")?.into(),
        scope: IDUNN_LIFECYCLE_BRAKE_SCOPE.into(),
        status: "released".into(),
        reason: required(options, "reason")?.into(),
        updated_at_unix_millis: updated,
        released_until_unix_millis: options
            .get("released-until-unix-millis")
            .map(|value| {
                value
                    .parse()
                    .context("--released-until-unix-millis must be u64")
            })
            .transpose()?,
    };
    replace_lifecycle_brake(&path(options, "store")?, record)
}

fn lifecycle_brake_status(options: &BTreeMap<String, String>) -> Result<()> {
    require_only(
        options,
        &["store", "runtime-id", "target", "now-unix-millis"],
    )?;
    let runtime_id = lifecycle_identifier(options, "runtime-id")?;
    let target = lifecycle_identifier(options, "target")?;
    let now = parse_u64(options, "now-unix-millis")?;
    match read_lifecycle_brake(&path(options, "store")?)? {
        None => {
            evaluate_idunn_continuity_restart(
                IdunnLifecycleBrakeObservation::Missing,
                runtime_id,
                target,
                now,
            )
            .map_err(|denial| anyhow!("lifecycle brake denied continuity restart: {denial:?}"))?;
            println!("allowed runtime={runtime_id} target={target} brake=absent");
        }
        Some(record) => {
            evaluate_idunn_continuity_restart(
                IdunnLifecycleBrakeObservation::Present(&record),
                runtime_id,
                target,
                now,
            )
            .map_err(|denial| anyhow!("lifecycle brake denied continuity restart: {denial:?}"))?;
            if let Some(expires) = record.released_until_unix_millis {
                println!(
                    "allowed runtime={} target={} brake=released expires={expires}",
                    record.runtime_id, record.target
                );
            } else {
                println!(
                    "allowed runtime={} target={} brake=released expires=never",
                    record.runtime_id, record.target
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn replace_lifecycle_brake(path: &Path, record: IdunnLifecycleBrakeRecord) -> Result<()> {
    record.validate()?;
    let store = SingleFileMessagePackBackingStore::new(path);
    let entries = store.pull_all_read_only_snapshot()?;
    let current = match entries.as_slice() {
        [] => None,
        [entry]
            if entry.r#type == IdunnLifecycleBrakeRecord::TYPE
                && entry.schema_id.as_deref() == Some(IDUNN_LIFECYCLE_BRAKE_SCHEMA) =>
        {
            let current_record = IdunnLifecycleBrakeRecord::decode_canonical(&entry.payload)?;
            if entry.key != current_record.target {
                bail!("lifecycle brake envelope key does not match its target");
            }
            if current_record.runtime_id != record.runtime_id
                || current_record.target != record.target
            {
                bail!("lifecycle brake store is bound to another runtime or target");
            }
            if record.updated_at_unix_millis <= current_record.updated_at_unix_millis {
                bail!("lifecycle brake update time must advance");
            }
            Some(entry.clone())
        }
        _ => bail!("lifecycle brake store is corrupt or ambiguous"),
    };
    let next = typed_envelope(
        &record.target,
        &record,
        IDUNN_LIFECYCLE_BRAKE_SCHEMA,
        record.updated_at_unix_millis,
    )?;
    if !store.compare_exchange(
        &[CultCacheExpectedEnvelope {
            r#type: IdunnLifecycleBrakeRecord::TYPE.into(),
            key: record.target.clone(),
            current,
        }],
        &[next],
    )? {
        bail!("lifecycle brake changed during transition");
    }
    Ok(())
}

fn read_lifecycle_brake(path: &Path) -> Result<Option<IdunnLifecycleBrakeRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let entries = SingleFileMessagePackBackingStore::new(path).pull_all_read_only_snapshot()?;
    let entry = match entries.as_slice() {
        [] => return Ok(None),
        [entry] => entry,
        _ => bail!("lifecycle brake store is ambiguous"),
    };
    if entry.r#type != IdunnLifecycleBrakeRecord::TYPE
        || entry.schema_id.as_deref() != Some(IDUNN_LIFECYCLE_BRAKE_SCHEMA)
    {
        bail!("lifecycle brake store contains a foreign record");
    }
    let record = IdunnLifecycleBrakeRecord::decode_canonical(&entry.payload)?;
    if entry.key != record.target {
        bail!("lifecycle brake envelope key does not match its target");
    }
    Ok(Some(record))
}

fn lifecycle_identifier<'a>(options: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    let value = required(options, name)?;
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("--{name} is empty, oversized, or contains control characters");
    }
    Ok(value)
}

fn validate_health_binding_store(path: &Path) -> Result<()> {
    let entries = SingleFileMessagePackBackingStore::new(path).pull_all_read_only_snapshot()?;
    validated_health_bindings(&entries)?;
    Ok(())
}

fn validated_health_bindings(
    entries: &[CultCacheEnvelope],
) -> Result<Vec<IdunnDaemonHealthTrustBindingRecord>> {
    if entries.is_empty() {
        bail!("daemon health trust store is empty");
    }
    let mut keys = BTreeSet::new();
    let mut tuples = BTreeSet::new();
    let mut bindings = Vec::with_capacity(entries.len());
    for envelope in entries {
        if envelope.r#type != IdunnDaemonHealthTrustBindingRecord::TYPE
            || envelope.schema_id.as_deref() != Some(IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA)
        {
            bail!("trust store contains an alien type or schema");
        }
        let binding: IdunnDaemonHealthTrustBindingRecord =
            rmp_serde::from_slice(&envelope.payload)?;
        if rmp_serde::to_vec(&binding)? != envelope.payload || envelope.key != binding.binding_id {
            bail!("trust store contains a noncanonical or mismatched binding");
        }
        binding.validate()?;
        if !keys.insert(binding.binding_id.clone())
            || !tuples.insert((
                binding.daemon_id.clone(),
                binding.health_contract.clone(),
                binding.source_runtime_id.clone(),
            ))
        {
            bail!("trust store contains a duplicate binding id or tuple");
        }
        bindings.push(binding);
    }
    Ok(bindings)
}

fn create_health_binding(options: &BTreeMap<String, String>) -> Result<()> {
    let record = health_binding(options)?;
    write_new_typed(
        &path(options, "output")?,
        &record.binding_id,
        &record,
        IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA,
        record.bound_at_unix_millis,
    )
}

fn health_binding(
    options: &BTreeMap<String, String>,
) -> Result<IdunnDaemonHealthTrustBindingRecord> {
    require_only(
        options,
        &[
            "output",
            "binding-id",
            "daemon",
            "health-contract",
            "source-runtime",
            "signer-public-key-hex",
            "bound-at-unix-millis",
            "release-binding-required",
        ],
    )?;
    let public_key = decode_public_key(required(options, "signer-public-key-hex")?)?;
    let record = IdunnDaemonHealthTrustBindingRecord {
        schema_version: IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA.into(),
        binding_id: required(options, "binding-id")?.into(),
        daemon_id: required(options, "daemon")?.into(),
        health_contract: required(options, "health-contract")?.into(),
        source_runtime_id: required(options, "source-runtime")?.into(),
        signer_identity_id: derive_service_identity_id::<GameCultProviderHealthIdentity>(
            &public_key,
        )?,
        signer_public_key: public_key,
        binding_authority: "root".into(),
        bound_at_unix_millis: parse_u64(options, "bound-at-unix-millis")?,
        release_binding_required: parse_bool(options, "release-binding-required")?,
        private_state_exposed: false,
    };
    record.validate()?;
    Ok(record)
}

fn add_health_binding(options: &BTreeMap<String, String>) -> Result<()> {
    let record = health_binding(options)?;
    let output = path(options, "output")?;
    let store = SingleFileMessagePackBackingStore::new(&output);
    let existing = store.pull_all_read_only_snapshot()?;
    if existing.is_empty() {
        bail!("trust store must already exist; create the first binding explicitly");
    }
    let mut keys = BTreeSet::new();
    let mut tuples = BTreeSet::new();
    let mut expected = Vec::with_capacity(existing.len() + 1);
    for envelope in existing {
        if envelope.r#type != IdunnDaemonHealthTrustBindingRecord::TYPE
            || envelope.schema_id.as_deref() != Some(IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA)
        {
            bail!("trust store contains an alien type or schema");
        }
        let binding: IdunnDaemonHealthTrustBindingRecord =
            rmp_serde::from_slice(&envelope.payload)?;
        if rmp_serde::to_vec(&binding)? != envelope.payload || envelope.key != binding.binding_id {
            bail!("trust store contains a noncanonical or mismatched binding");
        }
        binding.validate()?;
        if !keys.insert(binding.binding_id.clone()) {
            bail!("trust store contains duplicate binding ids");
        }
        if !tuples.insert((
            binding.daemon_id,
            binding.health_contract,
            binding.source_runtime_id,
        )) {
            bail!("trust store contains duplicate daemon/contract/runtime tuples");
        }
        expected.push(CultCacheExpectedEnvelope {
            r#type: envelope.r#type.clone(),
            key: envelope.key.clone(),
            current: Some(envelope),
        });
    }
    if keys.contains(&record.binding_id) {
        bail!("binding id already exists");
    }
    if tuples.contains(&(
        record.daemon_id.clone(),
        record.health_contract.clone(),
        record.source_runtime_id.clone(),
    )) {
        bail!("daemon/contract/runtime tuple already exists");
    }
    expected.push(CultCacheExpectedEnvelope {
        r#type: IdunnDaemonHealthTrustBindingRecord::TYPE.into(),
        key: record.binding_id.clone(),
        current: None,
    });
    let envelope = typed_envelope(
        &record.binding_id,
        &record,
        IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA,
        record.bound_at_unix_millis,
    )?;
    if !store.compare_exchange(&expected, &[envelope])? {
        bail!("trust store changed during validated append");
    }
    Ok(())
}

fn require_health_binding_release(options: &BTreeMap<String, String>) -> Result<()> {
    require_only(
        options,
        &[
            "store",
            "binding-id",
            "daemon",
            "health-contract",
            "source-runtime",
            "signer-public-key-hex",
        ],
    )?;
    let store_path = path(options, "store")?;
    let store = SingleFileMessagePackBackingStore::new(&store_path);
    let entries = store.pull_all_read_only_snapshot()?;
    let signer_public_key = decode_public_key(required(options, "signer-public-key-hex")?)?;
    require_health_binding_release_from_snapshot(
        &store,
        entries,
        required(options, "binding-id")?,
        required(options, "daemon")?,
        required(options, "health-contract")?,
        required(options, "source-runtime")?,
        &signer_public_key,
    )
}

fn require_health_binding_release_from_snapshot(
    store: &SingleFileMessagePackBackingStore,
    entries: Vec<CultCacheEnvelope>,
    binding_id: &str,
    daemon_id: &str,
    health_contract: &str,
    source_runtime_id: &str,
    signer_public_key: &[u8],
) -> Result<()> {
    let bindings = validated_health_bindings(&entries)?;
    let target_index = bindings
        .iter()
        .position(|binding| binding.binding_id == binding_id)
        .ok_or_else(|| anyhow!("named daemon health trust binding does not exist"))?;
    let current = &bindings[target_index];
    if current.daemon_id != daemon_id
        || current.health_contract != health_contract
        || current.source_runtime_id != source_runtime_id
        || current.signer_public_key != signer_public_key
    {
        bail!("named daemon health trust binding does not match the expected immutable fields");
    }
    if current.release_binding_required {
        return Ok(());
    }

    let mut next = current.clone();
    next.release_binding_required = true;
    next.validate()?;
    let mut replacement = entries[target_index].clone();
    replacement.payload = rmp_serde::to_vec(&next)?;
    let expected = entries
        .into_iter()
        .map(|envelope| CultCacheExpectedEnvelope {
            r#type: envelope.r#type.clone(),
            key: envelope.key.clone(),
            current: Some(envelope),
        })
        .collect::<Vec<_>>();
    if !store.compare_exchange(&expected, &[replacement])? {
        let latest_entries = store.pull_all_read_only_snapshot()?;
        let latest_bindings = validated_health_bindings(&latest_entries)?;
        if let Some(latest) = latest_bindings
            .iter()
            .find(|binding| binding.binding_id == binding_id)
            && latest.daemon_id == daemon_id
            && latest.health_contract == health_contract
            && latest.source_runtime_id == source_runtime_id
            && latest.signer_public_key == signer_public_key
            && latest.release_binding_required
        {
            return Ok(());
        }
        bail!("daemon health trust store changed during release-binding transition");
    }
    Ok(())
}

fn rotate_health_binding_signer(options: &BTreeMap<String, String>) -> Result<()> {
    require_only(
        options,
        &[
            "output",
            "binding-id",
            "signer-public-key-hex",
            "bound-at-unix-millis",
        ],
    )?;
    let output = path(options, "output")?;
    validate_health_binding_store(&output)?;
    let store = SingleFileMessagePackBackingStore::new(&output);
    let existing = store.pull_all_read_only_snapshot()?;
    let binding_id = required(options, "binding-id")?;
    let public_key = decode_public_key(required(options, "signer-public-key-hex")?)?;
    let bound_at = parse_u64(options, "bound-at-unix-millis")?;
    let mut expected = Vec::with_capacity(existing.len());
    let mut replacement = None;
    for envelope in existing {
        let binding: IdunnDaemonHealthTrustBindingRecord =
            rmp_serde::from_slice(&envelope.payload)?;
        if binding.binding_id == binding_id {
            if bound_at <= binding.bound_at_unix_millis {
                bail!("rotated signer binding time must advance");
            }
            if binding.signer_public_key == public_key {
                bail!("rotated signer must differ from the current signer");
            }
            let mut rotated = binding.clone();
            rotated.signer_identity_id =
                derive_service_identity_id::<GameCultProviderHealthIdentity>(&public_key)?;
            rotated.signer_public_key = public_key.clone();
            rotated.bound_at_unix_millis = bound_at;
            rotated.validate()?;
            replacement = Some(rotated);
        }
        expected.push(CultCacheExpectedEnvelope {
            r#type: envelope.r#type.clone(),
            key: envelope.key.clone(),
            current: Some(envelope),
        });
    }
    let replacement = replacement.ok_or_else(|| anyhow!("binding id does not exist"))?;
    let envelope = typed_envelope(
        &replacement.binding_id,
        &replacement,
        IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA,
        bound_at,
    )?;
    if !store.compare_exchange(&expected, &[envelope])? {
        bail!("trust store changed during validated signer rotation");
    }
    validate_health_binding_store(&output)
}

fn typed_envelope<T: DatabaseEntry>(
    key: &str,
    value: &T,
    schema: &str,
    millis: u64,
) -> Result<CultCacheEnvelope> {
    let stored_at = chrono::DateTime::from_timestamp_millis(i64::try_from(millis)?)
        .ok_or_else(|| anyhow!("document timestamp is out of range"))?
        .to_rfc3339();
    Ok(CultCacheEnvelope {
        key: key.into(),
        r#type: T::TYPE.into(),
        payload: rmp_serde::to_vec(value)?,
        stored_at,
        schema_id: Some(schema.into()),
    })
}

fn create_projection_anchor(options: &BTreeMap<String, String>) -> Result<()> {
    require_only(
        options,
        &[
            "output",
            "trust-anchor-id",
            "runtime-id",
            "idunn-public-anchor",
            "bound-at-unix-millis",
            "expires-at-unix-millis",
        ],
    )?;
    let output = path(options, "output")?;
    let low_path = path(options, "idunn-public-anchor")?;
    reject_alias(&output, &low_path)?;
    let low = read_low_level_idunn_anchor(&low_path)?;
    let bound = parse_u64(options, "bound-at-unix-millis")?;
    let record = GameCultServiceTrustAnchorRecord {
        schema_version: GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA.into(),
        trust_anchor_id: required(options, "trust-anchor-id")?.into(),
        service_id: "idunn".into(),
        runtime_id: required(options, "runtime-id")?.into(),
        signer_identity_id: derive_service_identity_id::<IdunnServiceIdentity>(&low.public_key)?,
        signer_public_key: low.public_key,
        signature_algorithm: "ed25519".into(),
        signing_purpose: IDUNN_AUTHENTICATED_PROVIDER_HEALTH_PROJECTION_SIGNING_PURPOSE.into(),
        signed_schema: IDUNN_AUTHENTICATED_PROVIDER_HEALTH_PROJECTION_SCHEMA.into(),
        binding_authority: "root".into(),
        bound_at_unix_millis: bound,
        expires_at_unix_millis: options
            .get("expires-at-unix-millis")
            .map(|value| value.parse().context("expires-at-unix-millis must be u64"))
            .transpose()?,
        private_state_exposed: false,
    };
    record.validate()?;
    write_new_typed(
        &output,
        &record.trust_anchor_id,
        &record,
        GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA,
        bound,
    )
}

fn validate_projection_anchor(options: &BTreeMap<String, String>) -> Result<()> {
    require_only(options, &["input", "idunn-public-anchor"])?;
    let input = path(options, "input")?;
    let low_path = path(options, "idunn-public-anchor")?;
    reject_alias(&input, &low_path)?;
    let record = read_typed::<GameCultServiceTrustAnchorRecord>(
        &input,
        GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA,
    )?;
    record.validate()?;
    let low = read_low_level_idunn_anchor(&low_path)?;
    if record.signer_public_key != low.public_key || record.signer_identity_id != low.identity_id {
        bail!("projection trust anchor does not match the supplied Idunn public anchor");
    }
    Ok(())
}

fn write_new_typed<T: RootDistributionDocument>(
    path: &Path,
    key: &str,
    value: &T,
    schema: &str,
    millis: u64,
) -> Result<()> {
    refuse_existing(path, "root distribution document")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stored_at = chrono::DateTime::from_timestamp_millis(
        i64::try_from(millis).context("document timestamp is out of range")?,
    )
    .ok_or_else(|| anyhow!("document timestamp is out of range"))?
    .to_rfc3339();
    let envelope = CultCacheEnvelope {
        key: key.into(),
        r#type: T::TYPE.into(),
        payload: rmp_serde::to_vec(value)?,
        stored_at,
        schema_id: Some(schema.into()),
    };
    let created = SingleFileMessagePackBackingStore::new(path).compare_exchange(
        &[CultCacheExpectedEnvelope {
            r#type: T::TYPE.into(),
            key: key.into(),
            current: None,
        }],
        &[envelope],
    )?;
    if !created {
        bail!("root distribution document lost atomic create race; refusing replacement");
    }
    Ok(())
}

fn read_typed<T: RootDistributionDocument>(path: &Path, schema: &str) -> Result<T> {
    let entries = SingleFileMessagePackBackingStore::new(path).pull_all_read_only_snapshot()?;
    let [entry] = entries.as_slice() else {
        bail!("root distribution file must contain exactly one document");
    };
    if entry.r#type != T::TYPE || entry.schema_id.as_deref() != Some(schema) {
        bail!("root distribution document has the wrong type or schema");
    }
    let value: T = rmp_serde::from_slice(&entry.payload).context("decoding typed root document")?;
    if rmp_serde::to_vec(&value)? != entry.payload {
        bail!("root distribution document is not canonical positional MessagePack");
    }
    if entry.key != value.root_key() {
        bail!("root distribution envelope key does not match its typed document");
    }
    Ok(value)
}

fn read_low_level_idunn_anchor(path: &Path) -> Result<ServiceIdentityTrustAnchor> {
    let entries = SingleFileMessagePackBackingStore::new(path).pull_all_read_only_snapshot()?;
    let [entry] = entries.as_slice() else {
        bail!("Idunn public anchor store must contain exactly one document");
    };
    if entry.r#type != IdunnServiceIdentity::TRUST_ANCHOR_TYPE
        || entry.key != IdunnServiceIdentity::TRUST_ANCHOR_KEY
        || entry.schema_id.as_deref() != Some(IdunnServiceIdentity::TRUST_ANCHOR_SCHEMA)
    {
        bail!("public anchor does not belong to the Idunn service identity profile");
    }
    let anchor: ServiceIdentityTrustAnchor = rmp_serde::from_slice(&entry.payload)?;
    if rmp_serde::to_vec(&anchor)? != entry.payload
        || anchor.schema_version != IdunnServiceIdentity::TRUST_ANCHOR_SCHEMA
        || anchor.identity_id
            != derive_service_identity_id::<IdunnServiceIdentity>(&anchor.public_key)?
    {
        bail!("Idunn public anchor is malformed or names the wrong key");
    }
    Ok(anchor)
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<BTreeMap<String, String>> {
    let mut args = args.peekable();
    let mut out = BTreeMap::new();
    while let Some(name) = args.next() {
        let name = name
            .strip_prefix("--")
            .ok_or_else(|| anyhow!("expected --option, got {name:?}"))?;
        if name.contains("seed") || name.contains("private-key") {
            bail!("private seed/key input is forbidden; enroll a protected identity instead");
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("missing value for --{name}"))?;
        if out.insert(name.into(), value).is_some() {
            bail!("duplicate option --{name}");
        }
    }
    Ok(out)
}

fn require_only(options: &BTreeMap<String, String>, names: &[&str]) -> Result<()> {
    for name in names {
        required(options, name)?;
    }
    if let Some(name) = options.keys().find(|name| !names.contains(&name.as_str())) {
        bail!("unsupported option --{name}");
    }
    Ok(())
}

fn allow_only(options: &BTreeMap<String, String>, names: &[&str]) -> Result<()> {
    if let Some(name) = options.keys().find(|name| !names.contains(&name.as_str())) {
        bail!("unsupported option --{name}");
    }
    Ok(())
}

fn required<'a>(options: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    options
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing --{name}"))
}

fn path(options: &BTreeMap<String, String>, name: &str) -> Result<PathBuf> {
    let value = required(options, name)?;
    if value.trim().is_empty() {
        bail!("--{name} must not be empty");
    }
    Ok(PathBuf::from(value))
}

fn parse_u64(options: &BTreeMap<String, String>, name: &str) -> Result<u64> {
    required(options, name)?
        .parse()
        .with_context(|| format!("--{name} must be u64"))
}

fn parse_bool(options: &BTreeMap<String, String>, name: &str) -> Result<bool> {
    match required(options, name)? {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("--{name} must be true or false"),
    }
}

fn decode_public_key(value: &str) -> Result<Vec<u8>> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("signer public key must be exactly 64 lowercase hexadecimal characters");
    }
    (0..32)
        .map(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(Into::into))
        .collect()
}

fn encode_public_key(value: &[u8]) -> Result<String> {
    if value.len() != 32 {
        bail!("provider health public key must be exactly 32 bytes");
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in value {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(encoded)
}

fn refuse_existing(path: &Path, label: &str) -> Result<()> {
    if path.exists() {
        bail!(
            "{label} {} already exists; replacement is forbidden",
            path.display()
        );
    }
    Ok(())
}

fn reject_alias(first: &Path, second: &Path) -> Result<()> {
    if normalized(first)? == normalized(second)? {
        bail!("private/public or input/output paths must not alias");
    }
    Ok(())
}

fn normalized(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| anyhow!("path has no parent"))?;
    let parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    Ok(parent.join(
        absolute
            .file_name()
            .ok_or_else(|| anyhow!("path has no filename"))?,
    ))
}

fn usage() -> &'static str {
    "Usage: idunn-provision enroll-provider-health-identity --private-store <path>\n       idunn-provision provider-health-public-key --private-store <path>\n       idunn-provision export-provider-health-public-anchor --private-store <path> --public-anchor <path>\n       idunn-provision enroll-idunn-identity --private-store <path>\n       idunn-provision export-idunn-public-anchor --private-store <path> --public-anchor <path>\n       idunn-provision enroll-odin-topology-identity --private-store <path>\n       idunn-provision export-odin-topology-public-anchor --private-store <path> --public-anchor <path>\n       idunn-provision deployment-brake-engage --store <path> --runtime-id <id> --reason <text>\n       idunn-provision deployment-brake-clear --store <path> --runtime-id <id> --reason <text>\n       idunn-provision deployment-brake-status --store <path> --runtime-id <id> [--owner-user <login>]\n       idunn-provision lifecycle-brake-engage --store <path> --runtime-id <id> --target <id> --reason <text> --updated-at-unix-millis <u64>\n       idunn-provision lifecycle-brake-release --store <path> --runtime-id <id> --target <id> --reason <text> --updated-at-unix-millis <u64> [--released-until-unix-millis <u64>]\n       idunn-provision lifecycle-brake-status --store <path> --runtime-id <id> --target <id> --now-unix-millis <u64>\n       idunn-provision create-daemon-health-trust-binding --output <path> --binding-id <id> --daemon <id> --health-contract <id> --source-runtime <id> --signer-public-key-hex <hex> --bound-at-unix-millis <u64> --release-binding-required <true|false>\n       idunn-provision add-daemon-health-trust-binding --output <path> --binding-id <id> --daemon <id> --health-contract <id> --source-runtime <id> --signer-public-key-hex <hex> --bound-at-unix-millis <u64> --release-binding-required <true|false>\n       idunn-provision require-daemon-health-release-binding --store <path> --binding-id <id> --daemon <id> --health-contract <id> --source-runtime <id> --signer-public-key-hex <hex>\n       idunn-provision rotate-daemon-health-trust-signer --output <path> --binding-id <id> --signer-public-key-hex <hex> --bound-at-unix-millis <u64>\n       idunn-provision validate-daemon-health-trust-binding --input <path>\n       idunn-provision create-provider-projection-trust-anchor --output <path> --trust-anchor-id <id> --runtime-id <id> --idunn-public-anchor <path> --bound-at-unix-millis <u64> --expires-at-unix-millis <u64>\n       idunn-provision validate-provider-projection-trust-anchor --input <path> --idunn-public-anchor <path>"
}

#[cfg(test)]
mod tests {
    use super::*;
    use cultcache_rs::CacheBackingStore;
    use tempfile::TempDir;

    fn invoke(args: &[&str]) -> Result<()> {
        run(args.iter().map(|value| value.to_string()))
    }

    fn replace_typed<T: DatabaseEntry>(
        path: &Path,
        key: &str,
        schema: &str,
        value: &T,
    ) -> Result<()> {
        std::fs::remove_file(path)?;
        SingleFileMessagePackBackingStore::new(path).push(&CultCacheEnvelope {
            key: key.into(),
            r#type: T::TYPE.into(),
            payload: rmp_serde::to_vec(value)?,
            stored_at: "2026-07-19T20:00:00Z".into(),
            schema_id: Some(schema.into()),
        })
    }

    fn binding_args<'a>(
        command: &'a str,
        output: &'a str,
        id: &'a str,
        daemon: &'a str,
        runtime: &'a str,
    ) -> Vec<&'a str> {
        vec![
            command,
            "--output",
            output,
            "--binding-id",
            id,
            "--daemon",
            daemon,
            "--health-contract",
            "provider.health",
            "--source-runtime",
            runtime,
            "--signer-public-key-hex",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "--bound-at-unix-millis",
            "1784483100000",
            "--release-binding-required",
            "false",
        ]
    }

    const TEST_SIGNER_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";
    const WRONG_SIGNER_HEX: &str =
        "2222222222222222222222222222222222222222222222222222222222222222";

    fn write_test_binding(
        command: &str,
        store: &str,
        id: &str,
        daemon: &str,
        runtime: &str,
    ) -> Result<()> {
        invoke(&binding_args(command, store, id, daemon, runtime))
    }

    fn require_test_binding(
        store: &str,
        id: &str,
        daemon: &str,
        contract: &str,
        runtime: &str,
        signer: &str,
    ) -> Result<()> {
        invoke(&[
            "require-daemon-health-release-binding",
            "--store",
            store,
            "--binding-id",
            id,
            "--daemon",
            daemon,
            "--health-contract",
            contract,
            "--source-runtime",
            runtime,
            "--signer-public-key-hex",
            signer,
        ])
    }

    fn store_snapshot(path: &Path) -> Result<Vec<CultCacheEnvelope>> {
        SingleFileMessagePackBackingStore::new(path).pull_all_read_only_snapshot()
    }

    #[test]
    fn append_health_binding_preserves_existing_and_rejects_collisions_and_aliens() -> Result<()> {
        let temp = TempDir::new()?;
        let store = temp.path().join("trust.cc");
        let path = store.to_str().unwrap();
        invoke(&binding_args(
            "create-daemon-health-trust-binding",
            path,
            "one",
            "daemon-one",
            "runtime-one",
        ))?;
        let before = SingleFileMessagePackBackingStore::new(&store)
            .pull_all_read_only_snapshot()?
            .remove(0)
            .payload;
        invoke(&binding_args(
            "add-daemon-health-trust-binding",
            path,
            "two",
            "daemon-two",
            "runtime-two",
        ))?;
        let entries =
            SingleFileMessagePackBackingStore::new(&store).pull_all_read_only_snapshot()?;
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.key == "one")
                .unwrap()
                .payload,
            before
        );
        assert!(
            invoke(&binding_args(
                "add-daemon-health-trust-binding",
                path,
                "two",
                "daemon-three",
                "runtime-three"
            ))
            .is_err()
        );
        assert!(
            invoke(&binding_args(
                "add-daemon-health-trust-binding",
                path,
                "three",
                "daemon-one",
                "runtime-one"
            ))
            .is_err()
        );
        SingleFileMessagePackBackingStore::new(&store).push(&CultCacheEnvelope {
            key: "alien".into(),
            r#type: "alien".into(),
            payload: vec![],
            stored_at: "2026-07-19T20:00:00Z".into(),
            schema_id: Some("alien.v0".into()),
        })?;
        assert!(
            invoke(&binding_args(
                "add-daemon-health-trust-binding",
                path,
                "four",
                "daemon-four",
                "runtime-four"
            ))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn require_release_binding_changes_only_the_named_flag_and_is_idempotent() -> Result<()> {
        let temp = TempDir::new()?;
        let store = temp.path().join("trust.cc");
        let path = store.to_str().unwrap();
        write_test_binding(
            "create-daemon-health-trust-binding",
            path,
            "one",
            "daemon-one",
            "runtime-one",
        )?;
        write_test_binding(
            "add-daemon-health-trust-binding",
            path,
            "two",
            "daemon-two",
            "runtime-two",
        )?;
        let before = store_snapshot(&store)?;

        require_test_binding(
            path,
            "one",
            "daemon-one",
            "provider.health",
            "runtime-one",
            TEST_SIGNER_HEX,
        )?;
        let after = store_snapshot(&store)?;
        let before_target = before.iter().find(|entry| entry.key == "one").unwrap();
        let after_target = after.iter().find(|entry| entry.key == "one").unwrap();
        let mut expected: IdunnDaemonHealthTrustBindingRecord =
            rmp_serde::from_slice(&before_target.payload)?;
        expected.release_binding_required = true;
        assert_eq!(
            rmp_serde::from_slice::<IdunnDaemonHealthTrustBindingRecord>(&after_target.payload)?,
            expected
        );
        let mut expected_envelope = before_target.clone();
        expected_envelope.payload = after_target.payload.clone();
        assert_eq!(after_target, &expected_envelope);
        assert_eq!(
            after.iter().find(|entry| entry.key == "two"),
            before.iter().find(|entry| entry.key == "two")
        );

        require_test_binding(
            path,
            "one",
            "daemon-one",
            "provider.health",
            "runtime-one",
            TEST_SIGNER_HEX,
        )?;
        assert_eq!(store_snapshot(&store)?, after);
        require_health_binding_release_from_snapshot(
            &SingleFileMessagePackBackingStore::new(&store),
            before,
            "one",
            "daemon-one",
            "provider.health",
            "runtime-one",
            &decode_public_key(TEST_SIGNER_HEX)?,
        )?;
        Ok(())
    }

    #[test]
    fn require_release_binding_refuses_wrong_identity_and_invalid_store_without_writing()
    -> Result<()> {
        let temp = TempDir::new()?;
        let store = temp.path().join("trust.cc");
        let path = store.to_str().unwrap();
        write_test_binding(
            "create-daemon-health-trust-binding",
            path,
            "one",
            "daemon-one",
            "runtime-one",
        )?;
        let before = store_snapshot(&store)?;
        for (id, daemon, contract, runtime, signer) in [
            (
                "one",
                "wrong",
                "provider.health",
                "runtime-one",
                TEST_SIGNER_HEX,
            ),
            ("one", "daemon-one", "wrong", "runtime-one", TEST_SIGNER_HEX),
            (
                "one",
                "daemon-one",
                "provider.health",
                "wrong",
                TEST_SIGNER_HEX,
            ),
            (
                "one",
                "daemon-one",
                "provider.health",
                "runtime-one",
                WRONG_SIGNER_HEX,
            ),
            (
                "missing",
                "daemon-one",
                "provider.health",
                "runtime-one",
                TEST_SIGNER_HEX,
            ),
        ] {
            assert!(require_test_binding(path, id, daemon, contract, runtime, signer).is_err());
        }
        assert!(
            invoke(&[
                "require-daemon-health-release-binding",
                "--store",
                path,
                "--binding-id",
                "one",
                "--daemon",
                "daemon-one",
                "--health-contract",
                "provider.health",
                "--signer-public-key-hex",
                TEST_SIGNER_HEX,
            ])
            .is_err()
        );
        assert_eq!(store_snapshot(&store)?, before);

        SingleFileMessagePackBackingStore::new(&store).push(&CultCacheEnvelope {
            key: "alien".into(),
            r#type: "alien".into(),
            payload: vec![],
            stored_at: "2026-07-19T20:00:00Z".into(),
            schema_id: Some("alien.v0".into()),
        })?;
        let foreign = store_snapshot(&store)?;
        assert!(
            require_test_binding(
                path,
                "one",
                "daemon-one",
                "provider.health",
                "runtime-one",
                TEST_SIGNER_HEX,
            )
            .is_err()
        );
        assert_eq!(store_snapshot(&store)?, foreign);

        let ambiguous = temp.path().join("ambiguous.cc");
        let ambiguous_path = ambiguous.to_str().unwrap();
        write_test_binding(
            "create-daemon-health-trust-binding",
            ambiguous_path,
            "one",
            "daemon-one",
            "runtime-one",
        )?;
        let mut duplicate: IdunnDaemonHealthTrustBindingRecord =
            rmp_serde::from_slice(&store_snapshot(&ambiguous)?.remove(0).payload)?;
        duplicate.binding_id = "duplicate".into();
        SingleFileMessagePackBackingStore::new(&ambiguous).push(&typed_envelope(
            &duplicate.binding_id,
            &duplicate,
            IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA,
            duplicate.bound_at_unix_millis,
        )?)?;
        let ambiguous_before = store_snapshot(&ambiguous)?;
        assert!(
            require_test_binding(
                ambiguous_path,
                "one",
                "daemon-one",
                "provider.health",
                "runtime-one",
                TEST_SIGNER_HEX,
            )
            .is_err()
        );
        assert_eq!(store_snapshot(&ambiguous)?, ambiguous_before);
        Ok(())
    }

    #[test]
    fn require_release_binding_cas_covers_the_whole_validated_store() -> Result<()> {
        let temp = TempDir::new()?;
        let store_path = temp.path().join("trust.cc");
        let path = store_path.to_str().unwrap();
        write_test_binding(
            "create-daemon-health-trust-binding",
            path,
            "one",
            "daemon-one",
            "runtime-one",
        )?;
        write_test_binding(
            "add-daemon-health-trust-binding",
            path,
            "two",
            "daemon-two",
            "runtime-two",
        )?;
        let stale = store_snapshot(&store_path)?;
        require_test_binding(
            path,
            "two",
            "daemon-two",
            "provider.health",
            "runtime-two",
            TEST_SIGNER_HEX,
        )?;

        let store = SingleFileMessagePackBackingStore::new(&store_path);
        let error = require_health_binding_release_from_snapshot(
            &store,
            stale,
            "one",
            "daemon-one",
            "provider.health",
            "runtime-one",
            &decode_public_key(TEST_SIGNER_HEX)?,
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed during release-binding"));
        let records = validated_health_bindings(&store_snapshot(&store_path)?)?;
        assert!(
            !records
                .iter()
                .find(|binding| binding.binding_id == "one")
                .unwrap()
                .release_binding_required
        );
        assert!(
            records
                .iter()
                .find(|binding| binding.binding_id == "two")
                .unwrap()
                .release_binding_required
        );
        Ok(())
    }

    #[test]
    fn concurrent_health_binding_appends_have_one_cas_winner() -> Result<()> {
        let temp = TempDir::new()?;
        let store = temp.path().join("trust.cc");
        let path = store.to_str().unwrap().to_string();
        invoke(&binding_args(
            "create-daemon-health-trust-binding",
            &path,
            "one",
            "daemon-one",
            "runtime-one",
        ))?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers = [
            ("two", "daemon-two", "runtime-two"),
            ("three", "daemon-three", "runtime-three"),
        ]
        .map(|(id, daemon, runtime)| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                invoke(&binding_args(
                    "add-daemon-health-trust-binding",
                    &path,
                    id,
                    daemon,
                    runtime,
                ))
            })
        });
        barrier.wait();
        let wins = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert!((1..=2).contains(&wins));
        assert_eq!(
            SingleFileMessagePackBackingStore::new(&store)
                .pull_all_read_only_snapshot()?
                .len(),
            1 + wins
        );
        Ok(())
    }

    #[test]
    fn provider_health_identity_enrollment_is_profile_exact_and_immutable() -> Result<()> {
        let temp = TempDir::new()?;
        let private = temp.path().join("provider-health-identity.cc");
        let options = BTreeMap::from([(
            "private-store".to_string(),
            private.to_string_lossy().into_owned(),
        )]);

        let public_key_hex = enroll_provider_health_identity(&options)?;
        let signer = open_service_identity_at::<GameCultProviderHealthIdentity>(&private)?;
        assert_eq!(
            public_key_hex,
            encode_public_key(&signer.entry().public_key)?
        );
        assert_eq!(
            decode_public_key(&public_key_hex)?,
            signer.entry().public_key
        );

        let entries =
            SingleFileMessagePackBackingStore::new(&private).pull_all_read_only_snapshot()?;
        let [entry] = entries.as_slice() else {
            panic!("provider health identity store must contain exactly one entry");
        };
        assert_eq!(entry.r#type, GameCultProviderHealthIdentity::PRIVATE_TYPE);
        assert_eq!(entry.key, GameCultProviderHealthIdentity::PRIVATE_KEY);
        assert_eq!(
            entry.schema_id.as_deref(),
            Some(GameCultProviderHealthIdentity::PRIVATE_SCHEMA)
        );
        assert!(open_service_identity_at::<IdunnServiceIdentity>(&private).is_err());

        let before = std::fs::read(&private)?;
        assert!(
            invoke(&[
                "enroll-provider-health-identity",
                "--private-store",
                private.to_str().unwrap(),
            ])
            .is_err()
        );
        assert_eq!(std::fs::read(&private)?, before);

        let unauthorized = temp.path().join("unauthorized.cc");
        assert!(
            invoke(&[
                "enroll-provider-health-identity",
                "--private-store",
                unauthorized.to_str().unwrap(),
                "--release-binding-required",
                "true",
            ])
            .is_err()
        );
        assert!(!unauthorized.exists());
        Ok(())
    }

    #[test]
    fn provider_health_public_key_is_read_only_and_rejects_wrong_profile_or_options() -> Result<()>
    {
        let temp = TempDir::new()?;
        let provider = temp.path().join("provider-health.cc");
        let provider_options = BTreeMap::from([(
            "private-store".to_string(),
            provider.to_string_lossy().into_owned(),
        )]);
        let expected = enroll_provider_health_identity(&provider_options)?;
        let provider_before = std::fs::read(&provider)?;
        assert_eq!(provider_health_public_key(&provider_options)?, expected);
        assert_eq!(std::fs::read(&provider)?, provider_before);

        let wrong_profile = temp.path().join("idunn.cc");
        enroll_service_identity_at::<IdunnServiceIdentity>(&wrong_profile)?;
        let wrong_before = std::fs::read(&wrong_profile)?;
        assert!(
            invoke(&[
                "provider-health-public-key",
                "--private-store",
                wrong_profile.to_str().unwrap(),
            ])
            .is_err()
        );
        assert_eq!(std::fs::read(&wrong_profile)?, wrong_before);

        assert!(
            invoke(&[
                "provider-health-public-key",
                "--private-store",
                provider.to_str().unwrap(),
                "--release-binding-required",
                "true",
            ])
            .is_err()
        );
        assert_eq!(std::fs::read(&provider)?, provider_before);
        Ok(())
    }

    #[test]
    fn provider_and_odin_anchor_exports_are_profile_exact_immutable_and_non_aliasing() -> Result<()>
    {
        let temp = TempDir::new()?;
        let provider_private = temp.path().join("provider-private.cc");
        let provider_anchor = temp.path().join("provider-anchor.cc");
        invoke(&[
            "enroll-provider-health-identity",
            "--private-store",
            provider_private.to_str().unwrap(),
        ])?;
        assert!(
            invoke(&[
                "export-provider-health-public-anchor",
                "--private-store",
                provider_private.to_str().unwrap(),
                "--public-anchor",
                provider_private.to_str().unwrap(),
            ])
            .is_err()
        );
        invoke(&[
            "export-provider-health-public-anchor",
            "--private-store",
            provider_private.to_str().unwrap(),
            "--public-anchor",
            provider_anchor.to_str().unwrap(),
        ])?;
        let provider_entries = store_snapshot(&provider_anchor)?;
        let [provider_envelope] = provider_entries.as_slice() else {
            panic!("provider anchor store must contain exactly one envelope");
        };
        assert_eq!(
            provider_envelope.r#type,
            GameCultProviderHealthIdentity::TRUST_ANCHOR_TYPE
        );
        assert_eq!(
            provider_envelope.key,
            GameCultProviderHealthIdentity::TRUST_ANCHOR_KEY
        );
        assert_eq!(
            provider_envelope.schema_id.as_deref(),
            Some(GameCultProviderHealthIdentity::TRUST_ANCHOR_SCHEMA)
        );
        let provider: ServiceIdentityTrustAnchor =
            rmp_serde::from_slice(&provider_envelope.payload)?;
        assert_eq!(rmp_serde::to_vec(&provider)?, provider_envelope.payload);
        assert_eq!(
            provider.identity_id,
            derive_service_identity_id::<GameCultProviderHealthIdentity>(&provider.public_key)?
        );
        assert!(open_service_identity_at::<OdinTopologyIdentity>(&provider_private).is_err());
        let provider_before = std::fs::read(&provider_anchor)?;
        assert!(
            invoke(&[
                "export-provider-health-public-anchor",
                "--private-store",
                provider_private.to_str().unwrap(),
                "--public-anchor",
                provider_anchor.to_str().unwrap(),
            ])
            .is_err()
        );
        assert_eq!(std::fs::read(&provider_anchor)?, provider_before);

        let odin_private = temp.path().join("odin-private.cc");
        let odin_anchor = temp.path().join("odin-anchor.cc");
        invoke(&[
            "enroll-odin-topology-identity",
            "--private-store",
            odin_private.to_str().unwrap(),
        ])?;
        assert!(open_service_identity_at::<GameCultProviderHealthIdentity>(&odin_private).is_err());
        assert!(
            invoke(&[
                "export-odin-topology-public-anchor",
                "--private-store",
                odin_private.to_str().unwrap(),
                "--public-anchor",
                odin_private.to_str().unwrap(),
            ])
            .is_err()
        );
        invoke(&[
            "export-odin-topology-public-anchor",
            "--private-store",
            odin_private.to_str().unwrap(),
            "--public-anchor",
            odin_anchor.to_str().unwrap(),
        ])?;
        let odin_entries = store_snapshot(&odin_anchor)?;
        let [odin_envelope] = odin_entries.as_slice() else {
            panic!("Odin anchor store must contain exactly one envelope");
        };
        assert_eq!(
            odin_envelope.r#type,
            OdinTopologyIdentity::TRUST_ANCHOR_TYPE
        );
        assert_eq!(odin_envelope.key, OdinTopologyIdentity::TRUST_ANCHOR_KEY);
        assert_eq!(
            odin_envelope.schema_id.as_deref(),
            Some(OdinTopologyIdentity::TRUST_ANCHOR_SCHEMA)
        );
        let odin: ServiceIdentityTrustAnchor = rmp_serde::from_slice(&odin_envelope.payload)?;
        assert_eq!(rmp_serde::to_vec(&odin)?, odin_envelope.payload);
        assert_eq!(
            odin.identity_id,
            derive_service_identity_id::<OdinTopologyIdentity>(&odin.public_key)?
        );
        assert_ne!(provider.identity_id, odin.identity_id);
        let odin_before = std::fs::read(&odin_anchor)?;
        assert!(
            invoke(&[
                "export-odin-topology-public-anchor",
                "--private-store",
                odin_private.to_str().unwrap(),
                "--public-anchor",
                odin_anchor.to_str().unwrap(),
            ])
            .is_err()
        );
        assert_eq!(std::fs::read(&odin_anchor)?, odin_before);

        let wrong_profile_output = temp.path().join("wrong-profile.cc");
        assert!(
            invoke(&[
                "export-provider-health-public-anchor",
                "--private-store",
                odin_private.to_str().unwrap(),
                "--public-anchor",
                wrong_profile_output.to_str().unwrap(),
            ])
            .is_err()
        );
        assert!(!wrong_profile_output.exists());
        Ok(())
    }

    #[test]
    fn identity_enrollment_and_export_are_immutable_and_paths_cannot_alias() -> Result<()> {
        let temp = TempDir::new()?;
        let private = temp.path().join("identity.cc");
        let public = temp.path().join("identity-public.cc");
        invoke(&[
            "enroll-idunn-identity",
            "--private-store",
            private.to_str().unwrap(),
        ])?;
        assert!(
            invoke(&[
                "enroll-idunn-identity",
                "--private-store",
                private.to_str().unwrap()
            ])
            .is_err()
        );
        assert!(
            invoke(&[
                "export-idunn-public-anchor",
                "--private-store",
                private.to_str().unwrap(),
                "--public-anchor",
                private.to_str().unwrap(),
            ])
            .is_err()
        );
        invoke(&[
            "export-idunn-public-anchor",
            "--private-store",
            private.to_str().unwrap(),
            "--public-anchor",
            public.to_str().unwrap(),
        ])?;
        assert!(
            invoke(&[
                "export-idunn-public-anchor",
                "--private-store",
                private.to_str().unwrap(),
                "--public-anchor",
                public.to_str().unwrap(),
            ])
            .is_err()
        );
        assert!(
            invoke(&[
                "enroll-idunn-identity",
                "--private-store",
                temp.path().join("new.cc").to_str().unwrap(),
                "--private-seed",
                "00",
            ])
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn health_binding_derives_identity_and_rejects_overwrite_and_corruption() -> Result<()> {
        let temp = TempDir::new()?;
        let output = temp.path().join("provider-binding.cc");
        let key = "07".repeat(32);
        let args = [
            "create-daemon-health-trust-binding",
            "--output",
            output.to_str().unwrap(),
            "--binding-id",
            "root/epiphany/health",
            "--daemon",
            "yggdrasil-epiphany",
            "--health-contract",
            "epiphany.cultnet-rudp-runtime-health",
            "--source-runtime",
            "epiphany-daemon-supervisor",
            "--signer-public-key-hex",
            &key,
            "--bound-at-unix-millis",
            "1784483100000",
            "--release-binding-required",
            "true",
        ];
        invoke(&args)?;
        assert!(invoke(&args).is_err());
        let mut record = read_typed::<IdunnDaemonHealthTrustBindingRecord>(
            &output,
            IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA,
        )?;
        assert_eq!(
            record.signer_identity_id,
            derive_service_identity_id::<GameCultProviderHealthIdentity>(
                &record.signer_public_key
            )?
        );
        record.signer_identity_id = "caller-chosen".into();
        replace_typed(
            &output,
            &record.binding_id,
            IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA,
            &record,
        )?;
        assert!(
            invoke(&[
                "validate-daemon-health-trust-binding",
                "--input",
                output.to_str().unwrap()
            ])
            .is_err()
        );

        record.signer_identity_id = derive_service_identity_id::<GameCultProviderHealthIdentity>(
            &record.signer_public_key,
        )?;
        record.schema_version = "wrong.schema.v9".into();
        replace_typed(
            &output,
            &record.binding_id,
            IDUNN_DAEMON_HEALTH_TRUST_BINDING_SCHEMA,
            &record,
        )?;
        assert!(
            invoke(&[
                "validate-daemon-health-trust-binding",
                "--input",
                output.to_str().unwrap()
            ])
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn projection_anchor_is_exact_profile_and_matches_low_level_anchor() -> Result<()> {
        let temp = TempDir::new()?;
        let private = temp.path().join("identity.cc");
        let public = temp.path().join("public.cc");
        let root = temp.path().join("root-anchor.cc");
        invoke(&[
            "enroll-idunn-identity",
            "--private-store",
            private.to_str().unwrap(),
        ])?;
        invoke(&[
            "export-idunn-public-anchor",
            "--private-store",
            private.to_str().unwrap(),
            "--public-anchor",
            public.to_str().unwrap(),
        ])?;
        let create = [
            "create-provider-projection-trust-anchor",
            "--output",
            root.to_str().unwrap(),
            "--trust-anchor-id",
            "root/idunn/provider-health",
            "--runtime-id",
            "idunn-yggdrasil",
            "--idunn-public-anchor",
            public.to_str().unwrap(),
            "--bound-at-unix-millis",
            "1784483100000",
            "--expires-at-unix-millis",
            "1815000000000",
        ];
        invoke(&create)?;
        invoke(&[
            "validate-provider-projection-trust-anchor",
            "--input",
            root.to_str().unwrap(),
            "--idunn-public-anchor",
            public.to_str().unwrap(),
        ])?;
        assert!(invoke(&create).is_err());
        assert!(
            invoke(&[
                "validate-provider-projection-trust-anchor",
                "--input",
                public.to_str().unwrap(),
                "--idunn-public-anchor",
                public.to_str().unwrap(),
            ])
            .is_err()
        );

        let mut record = read_typed::<GameCultServiceTrustAnchorRecord>(
            &root,
            GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA,
        )?;
        record.signing_purpose = "wrong-purpose".into();
        replace_typed(
            &root,
            &record.trust_anchor_id,
            GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA,
            &record,
        )?;
        assert!(
            invoke(&[
                "validate-provider-projection-trust-anchor",
                "--input",
                root.to_str().unwrap(),
                "--idunn-public-anchor",
                public.to_str().unwrap(),
            ])
            .is_err()
        );

        record.signing_purpose =
            IDUNN_AUTHENTICATED_PROVIDER_HEALTH_PROJECTION_SIGNING_PURPOSE.into();
        record.signer_public_key = vec![9; 32];
        replace_typed(
            &root,
            &record.trust_anchor_id,
            GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA,
            &record,
        )?;
        assert!(
            invoke(&[
                "validate-provider-projection-trust-anchor",
                "--input",
                root.to_str().unwrap(),
                "--idunn-public-anchor",
                public.to_str().unwrap(),
            ])
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn release_binding_policy_is_explicit_and_boolean() {
        let temp = TempDir::new().unwrap();
        let output = temp.path().join("binding.cc");
        let base = [
            "create-daemon-health-trust-binding",
            "--output",
            output.to_str().unwrap(),
            "--binding-id",
            "root/provider/health",
            "--daemon",
            "provider",
            "--health-contract",
            "provider.health",
            "--source-runtime",
            "provider-runtime",
            "--signer-public-key-hex",
            "0707070707070707070707070707070707070707070707070707070707070707",
            "--bound-at-unix-millis",
            "1784483100000",
        ];
        assert!(invoke(&base).is_err());
        let mut partial = base.to_vec();
        partial.extend(["--release-binding-required", "sometimes"]);
        assert!(invoke(&partial).is_err());
    }

    #[test]
    fn signer_rotation_changes_only_the_named_trust_binding_signer() -> Result<()> {
        let temp = TempDir::new()?;
        let output = temp.path().join("binding.cc");
        invoke(&[
            "create-daemon-health-trust-binding",
            "--output",
            output.to_str().unwrap(),
            "--binding-id",
            "heimdall-yggdrasil-provider-health",
            "--daemon",
            "yggdrasil-heimdall",
            "--health-contract",
            "heimdall.cultnet-rudp-provider-health",
            "--source-runtime",
            "heimdall-service",
            "--signer-public-key-hex",
            "0707070707070707070707070707070707070707070707070707070707070707",
            "--bound-at-unix-millis",
            "100",
            "--release-binding-required",
            "false",
        ])?;
        assert!(
            invoke(&[
                "rotate-daemon-health-trust-signer",
                "--output",
                output.to_str().unwrap(),
                "--binding-id",
                "heimdall-yggdrasil-provider-health",
                "--signer-public-key-hex",
                "0808080808080808080808080808080808080808080808080808080808080808",
                "--bound-at-unix-millis",
                "100",
            ])
            .is_err()
        );
        invoke(&[
            "rotate-daemon-health-trust-signer",
            "--output",
            output.to_str().unwrap(),
            "--binding-id",
            "heimdall-yggdrasil-provider-health",
            "--signer-public-key-hex",
            "0808080808080808080808080808080808080808080808080808080808080808",
            "--bound-at-unix-millis",
            "101",
        ])?;
        let [envelope] = SingleFileMessagePackBackingStore::new(&output)
            .pull_all_read_only_snapshot()?
            .try_into()
            .map_err(|_| anyhow!("expected one trust binding"))?;
        let binding: IdunnDaemonHealthTrustBindingRecord =
            rmp_serde::from_slice(&envelope.payload)?;
        assert_eq!(binding.binding_id, "heimdall-yggdrasil-provider-health");
        assert_eq!(binding.daemon_id, "yggdrasil-heimdall");
        assert_eq!(
            binding.health_contract,
            "heimdall.cultnet-rudp-provider-health"
        );
        assert_eq!(binding.source_runtime_id, "heimdall-service");
        assert_eq!(binding.signer_public_key, vec![8; 32]);
        assert_eq!(binding.bound_at_unix_millis, 101);
        assert!(!binding.release_binding_required);
        validate_health_binding_store(&output)
    }

    #[test]
    fn provision_engage_clear_sequence() -> Result<()> {
        let temp = TempDir::new()?;
        let store = temp.path().join("brake.cc");
        let me = process_uid();
        let engage = |verb: &str, reason: &str| {
            invoke(&[
                verb,
                "--store",
                store.to_str().unwrap(),
                "--runtime-id",
                "yggdrasil",
                "--reason",
                reason,
            ])
        };
        assert_eq!(read_stop(&store, "yggdrasil", me), Stop::Absent);
        invoke(&[
            "deployment-brake-status",
            "--store",
            store.to_str().unwrap(),
            "--runtime-id",
            "yggdrasil",
        ])?;
        engage("deployment-brake-engage", "freeze")?;
        let status = || {
            invoke(&[
                "deployment-brake-status",
                "--store",
                store.to_str().unwrap(),
                "--runtime-id",
                "yggdrasil",
            ])
        };
        assert!(status().is_err(), "an engaged stop must not pass status");
        let Stop::Engaged(first) = read_stop(&store, "yggdrasil", me) else {
            bail!("engage did not engage")
        };
        assert_eq!((first.sequence, first.reason.as_str()), (1, "freeze"));
        assert!(!first.by.is_empty() && first.at_unix_millis > 0);
        engage("deployment-brake-clear", "thawed")?;
        status()?;
        let Stop::Clear(second) = read_stop(&store, "yggdrasil", me) else {
            bail!("clear did not clear")
        };
        assert_eq!((second.sequence, second.by), (2, first.by));
        assert!(second.at_unix_millis >= first.at_unix_millis);
        // The author and the time are not arguments, and the retired
        // signing arguments are refused.
        for stray in ["--owner", "--observed-at-unix-millis", "--private-store"] {
            assert!(
                invoke(&[
                    "deployment-brake-engage",
                    "--store",
                    store.to_str().unwrap(),
                    "--runtime-id",
                    "yggdrasil",
                    "--reason",
                    "x",
                    stray,
                    "y",
                ])
                .is_err(),
                "{stray}"
            );
        }
        for gone in ["deployment-brake-release", "enroll-deployment-brake-operator"] {
            assert!(invoke(&[gone]).is_err(), "{gone}");
        }
        Ok(())
    }

    #[test]
    fn lifecycle_brake_is_absence_allow_and_exact_to_runtime_target_and_expiry() -> Result<()> {
        let temp = TempDir::new()?;
        let missing = temp.path().join("missing-lifecycle.cc");
        invoke(&[
            "lifecycle-brake-status",
            "--store",
            missing.to_str().unwrap(),
            "--runtime-id",
            "idunn-yggdrasil",
            "--target",
            "ghostlight",
            "--now-unix-millis",
            "100",
        ])?;
        assert!(!missing.exists());

        let store = temp.path().join("ghostlight-lifecycle.cc");
        invoke(&[
            "lifecycle-brake-engage",
            "--store",
            store.to_str().unwrap(),
            "--runtime-id",
            "idunn-yggdrasil",
            "--target",
            "ghostlight",
            "--reason",
            "operator maintenance",
            "--updated-at-unix-millis",
            "200",
        ])?;
        let engaged = read_lifecycle_brake(&store)?.context("lifecycle brake must exist")?;
        assert_eq!(engaged.status, "engaged");
        assert_eq!(engaged.runtime_id, "idunn-yggdrasil");
        assert_eq!(engaged.target, "ghostlight");
        assert!(
            invoke(&[
                "lifecycle-brake-status",
                "--store",
                store.to_str().unwrap(),
                "--runtime-id",
                "idunn-yggdrasil",
                "--target",
                "ghostlight",
                "--now-unix-millis",
                "201",
            ])
            .is_err()
        );

        let engaged_bytes = std::fs::read(&store)?;
        assert!(
            invoke(&[
                "lifecycle-brake-release",
                "--store",
                store.to_str().unwrap(),
                "--runtime-id",
                "idunn-nightwing",
                "--target",
                "ghostlight",
                "--reason",
                "wrong runtime",
                "--updated-at-unix-millis",
                "300",
            ])
            .is_err()
        );
        assert!(
            invoke(&[
                "lifecycle-brake-release",
                "--store",
                store.to_str().unwrap(),
                "--runtime-id",
                "idunn-yggdrasil",
                "--target",
                "odin",
                "--reason",
                "wrong target",
                "--updated-at-unix-millis",
                "300",
            ])
            .is_err()
        );
        assert_eq!(std::fs::read(&store)?, engaged_bytes);

        invoke(&[
            "lifecycle-brake-release",
            "--store",
            store.to_str().unwrap(),
            "--runtime-id",
            "idunn-yggdrasil",
            "--target",
            "ghostlight",
            "--reason",
            "bounded continuity window",
            "--updated-at-unix-millis",
            "300",
            "--released-until-unix-millis",
            "500",
        ])?;
        invoke(&[
            "lifecycle-brake-status",
            "--store",
            store.to_str().unwrap(),
            "--runtime-id",
            "idunn-yggdrasil",
            "--target",
            "ghostlight",
            "--now-unix-millis",
            "499",
        ])?;
        for (runtime, target, now) in [
            ("idunn-nightwing", "ghostlight", "499"),
            ("idunn-yggdrasil", "odin", "499"),
            ("idunn-yggdrasil", "ghostlight", "500"),
        ] {
            assert!(
                invoke(&[
                    "lifecycle-brake-status",
                    "--store",
                    store.to_str().unwrap(),
                    "--runtime-id",
                    runtime,
                    "--target",
                    target,
                    "--now-unix-millis",
                    now,
                ])
                .is_err()
            );
        }
        let released_bytes = std::fs::read(&store)?;
        assert!(
            invoke(&[
                "lifecycle-brake-engage",
                "--store",
                store.to_str().unwrap(),
                "--runtime-id",
                "idunn-yggdrasil",
                "--target",
                "ghostlight",
                "--reason",
                "stale replay",
                "--updated-at-unix-millis",
                "300",
            ])
            .is_err()
        );
        assert_eq!(std::fs::read(&store)?, released_bytes);
        Ok(())
    }
}
