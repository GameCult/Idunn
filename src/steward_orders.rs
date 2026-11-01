//! Typed orders from Idunn's steward.
//!
//! `idunn orders serve` is a second root unit from the same binary as
//! `idunn serve`, with its own store (`orders.cc`) and socket. It alone mints
//! order ids, decides acceptance and refusal, actuates, and records outcomes.
//! The survival daemon takes no part: `control.cc` never learns an order type.
//!
//! An order is a closed enum, never a command string: install packages, act on
//! a systemd unit, or deploy a target through the same commit primitive
//! `idunn up` uses. Every unit is allowed except the always-refused set, which
//! is compiled in here and read from nowhere: no binding field, record or flag
//! holds it. The `#operator` post is a retried projection of the store.
//!
//! Lifecycle, one row per step: ISSUE a frame on the socket; AUTHENTICATE the
//! peer's uid; VALIDATE the grammar (a malformed order is a typed refusal and
//! no record); ACCEPT a `steward_order`; SCOPE the always-refused check; BRAKE
//! `mind-control check`; START record before actuation, at most once; RUN;
//! RESULT outcome; POST accepted and result.
//!
//! No refusal or error here carries a value the issuer supplied: it names the
//! field and a code. A refused unit that a record names was resolved by
//! systemd and passed the unit grammar.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use cultcache_rs::{
    CultCacheExpectedEnvelope, DatabaseEntry, SingleFileMessagePackBackingStore,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::control_plane::{
    command_state, commit_deployment_command, decode_record, default_state_store, now_millis,
    path_value, require_id, require_selector, string_value, typed_envelope,
};
use crate::runner_evidence::{emit, escape};

const ORDER_SCHEMA: &str = "idunn.steward_order.v1";
const START_SCHEMA: &str = "idunn.steward_order_start.v1";
const OUTCOME_SCHEMA: &str = "idunn.steward_order_outcome.v1";
const POST_SCHEMA: &str = "idunn.steward_order_post.v1";

const REQUEST_FRAME_LIMIT: usize = 16 * 1024;
const RESPONSE_FRAME_LIMIT: usize = 256 * 1024;
const TAIL_BYTES: usize = 8 * 1024;
const REASON_LIMIT: usize = 300;
const PACKAGE_LIMIT: usize = 16;
const UNIT_LIMIT: usize = 200;
const CLOSURE_LIMIT: usize = 512;
const LIST_LIMIT: usize = 50;
const UNIT_SUFFIXES: [&str; 6] = [".service", ".socket", ".timer", ".path", ".mount", ".target"];

/// The always-refused set: Idunn's own units and the minds' own. Matched on
/// every name a unit answers to, ASCII case-insensitively (systemd units are
/// case-sensitive, so the fold only widens the refusal).
fn is_refused_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["idunn-", "mind-self@", "mind-persona@"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

// ---------------------------------------------------------------- grammar --

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Code {
    Empty,
    TooLong,
    TooMany,
    BadCharacter,
    BadShape,
    Malformed,
    Unauthorized,
    Unknown,
    Unavailable,
}

/// A refusal names the field and a code. It never carries the offending value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Refusal {
    pub(crate) field: String,
    pub(crate) code: Code,
}

impl Refusal {
    fn new(field: &'static str, code: Code) -> Self {
        Self {
            field: field.to_owned(),
            code,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} refused: {:?}", self.field, self.code)
    }
}

impl std::error::Error for Refusal {}

fn validate_package(spec: &str) -> Result<(), Refusal> {
    const FIELD: &str = "package";
    if spec.is_empty() {
        return Err(Refusal::new(FIELD, Code::Empty));
    }
    if spec.len() > UNIT_LIMIT {
        return Err(Refusal::new(FIELD, Code::TooLong));
    }
    let (name, version) = match spec.split_once('=') {
        Some((name, version)) => (name, Some(version)),
        None => (spec, None),
    };
    let bytes = name.as_bytes();
    if bytes.len() < 2 {
        return Err(Refusal::new(FIELD, Code::BadShape));
    }
    let head = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let rest = bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'.' | b'-')
    });
    if !head || !rest {
        return Err(Refusal::new(FIELD, Code::BadCharacter));
    }
    // apt reads `name-` as a removal and `name.deb` as a local file.
    if name.ends_with('-') || name.ends_with(".deb") {
        return Err(Refusal::new(FIELD, Code::BadShape));
    }
    if let Some(version) = version {
        if version.is_empty() {
            return Err(Refusal::new(FIELD, Code::BadShape));
        }
        if !version.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'~' | b':' | b'-')
        }) {
            return Err(Refusal::new(FIELD, Code::BadCharacter));
        }
    }
    Ok(())
}

fn validate_unit(unit: &str) -> Result<(), Refusal> {
    const FIELD: &str = "unit";
    if unit.is_empty() {
        return Err(Refusal::new(FIELD, Code::Empty));
    }
    if unit.len() > UNIT_LIMIT {
        return Err(Refusal::new(FIELD, Code::TooLong));
    }
    let allowed = unit.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'.' | b'@' | b'-')
    });
    if !allowed || unit.starts_with('-') {
        return Err(Refusal::new(FIELD, Code::BadCharacter));
    }
    match UNIT_SUFFIXES
        .iter()
        .find_map(|suffix| unit.strip_suffix(suffix))
    {
        Some(stem) if !stem.is_empty() => Ok(()),
        _ => Err(Refusal::new(FIELD, Code::BadShape)),
    }
}

fn validate_reason(reason: &str) -> Result<(), Refusal> {
    const FIELD: &str = "reason";
    if reason.is_empty() {
        return Err(Refusal::new(FIELD, Code::Empty));
    }
    if reason.len() > REASON_LIMIT {
        return Err(Refusal::new(FIELD, Code::TooLong));
    }
    if !reason.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(Refusal::new(FIELD, Code::BadCharacter));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Verb {
    Start,
    Stop,
    Restart,
    Reload,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
            Verb::Reload => "reload",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        [Verb::Start, Verb::Stop, Verb::Restart, Verb::Reload]
            .into_iter()
            .find(|verb| verb.as_str() == word)
    }
}

/// The closed set of things an order can ask for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Operation {
    PackageInstall { packages: Vec<String> },
    UnitAction { verb: Verb, unit: String },
    Deploy { selector: String },
}

/// One fixed argv for `systemd-run`, built from the typed operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) argv: Vec<String>,
    pub(crate) environment: Vec<(String, String)>,
}

impl Operation {
    fn validate(&self) -> Result<(), Refusal> {
        match self {
            Operation::PackageInstall { packages } => {
                if packages.is_empty() {
                    return Err(Refusal::new("package", Code::Empty));
                }
                if packages.len() > PACKAGE_LIMIT {
                    return Err(Refusal::new("package", Code::TooMany));
                }
                packages.iter().try_for_each(|package| validate_package(package))
            }
            Operation::UnitAction { unit, .. } => validate_unit(unit),
            Operation::Deploy { selector } => require_selector(selector)
                .map_err(|_| Refusal::new("selector", Code::BadShape)),
        }
    }

    fn steps(&self) -> Vec<Step> {
        let words = |words: &[&str]| words.iter().map(|word| (*word).to_owned()).collect();
        let apt_environment = vec![("DEBIAN_FRONTEND".to_owned(), "noninteractive".to_owned())];
        match self {
            Operation::PackageInstall { packages } => {
                // Never a removal, never a pattern: each name selects exactly
                // itself, and apt aborts rather than displace an installed package.
                let mut install: Vec<String> =
                    words(&["apt-get", "install", "-y", "-q", "--no-remove", "--"]);
                let mut present: Vec<String> = words(&["dpkg", "-s", "--"]);
                for package in packages {
                    let (name, version) = package.split_once('=').unwrap_or((package, ""));
                    install.push(match version {
                        "" => format!("?exact-name({name})"),
                        version => format!("?exact-name({name})={version}"),
                    });
                    present.push(name.to_owned());
                }
                // An exact name that matches nothing installs nothing and exits 0,
                // so the last step fails unless every named package is installed.
                vec![
                    Step {
                        argv: words(&["apt-get", "update", "-q"]),
                        environment: apt_environment.clone(),
                    },
                    Step {
                        argv: install,
                        environment: apt_environment,
                    },
                    Step {
                        argv: present,
                        environment: Vec::new(),
                    },
                ]
            }
            Operation::UnitAction { verb, unit } => vec![Step {
                argv: vec![
                    "systemctl".into(),
                    verb.as_str().into(),
                    "--".into(),
                    unit.clone(),
                ],
                environment: Vec::new(),
            }],
            Operation::Deploy { .. } => Vec::new(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Operation::PackageInstall { packages } => {
                format!("package-install {}", packages.join(" "))
            }
            Operation::UnitAction { verb, unit } => format!("unit {} {unit}", verb.as_str()),
            Operation::Deploy { selector } => format!("deploy {selector}"),
        }
    }
}

// ---------------------------------------------------------------- records --

#[derive(Clone, PartialEq, Eq, DatabaseEntry)]
#[cultcache(type = "idunn.steward_order", schema = "idunn.steward_order.v1")]
pub(crate) struct OrderRecord {
    #[cultcache(key = 0)]
    pub(crate) schema_version: String,
    #[cultcache(key = 1)]
    pub(crate) order_id: String,
    #[cultcache(key = 2)]
    pub(crate) principal: String,
    #[cultcache(key = 3)]
    pub(crate) issuer_uid: u32,
    #[cultcache(key = 4)]
    pub(crate) issuer_unit: String,
    #[cultcache(key = 5)]
    pub(crate) issuer_invocation: String,
    #[cultcache(key = 6)]
    pub(crate) issued_at_unix_millis: u64,
    #[cultcache(key = 7)]
    pub(crate) operation: Operation,
    #[cultcache(key = 8)]
    pub(crate) reason: String,
}

impl fmt::Debug for OrderRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OrderRecord")
            .field("order_id", &self.order_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.steward_order_start",
    schema = "idunn.steward_order_start.v1"
)]
pub(crate) struct StartRecord {
    #[cultcache(key = 0)]
    pub(crate) schema_version: String,
    #[cultcache(key = 1)]
    pub(crate) order_id: String,
    #[cultcache(key = 2)]
    pub(crate) started_at_unix_millis: u64,
    /// The transient unit name, or the deployment command id.
    #[cultcache(key = 3)]
    pub(crate) actuation: String,
}

impl fmt::Debug for StartRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartRecord")
            .field("order_id", &self.order_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Via {
    Name,
    Alias,
    Propagation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Refused {
    AlwaysRefused { unit: String, via: Via },
    Braked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Outcome {
    Ran,
    Failed {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Refused(Refused),
    Lost,
    Submitted { command_id: String },
}

impl Outcome {
    fn describe(&self) -> String {
        match self {
            Outcome::Ran => "ran (exit 0)".into(),
            Outcome::Failed {
                exit_code: Some(code),
                ..
            } => format!("failed (exit {code})"),
            Outcome::Failed {
                signal: Some(signal),
                ..
            } => format!("failed (signal {signal})"),
            Outcome::Failed { .. } => "failed (not run to an exit)".into(),
            Outcome::Refused(Refused::AlwaysRefused { unit, via }) => format!(
                "refused: always-refused unit {} reached by {}",
                escape(unit.as_bytes()),
                match via {
                    Via::Name => "name",
                    Via::Alias => "alias",
                    Via::Propagation => "propagation",
                }
            ),
            Outcome::Refused(Refused::Braked) => "refused: braked".into(),
            Outcome::Lost => "lost: the orders service restarted mid-run".into(),
            Outcome::Submitted { command_id } => {
                format!("submitted deployment {}", escape(command_id.as_bytes()))
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.steward_order_outcome",
    schema = "idunn.steward_order_outcome.v1"
)]
pub(crate) struct OutcomeRecord {
    #[cultcache(key = 0)]
    pub(crate) schema_version: String,
    #[cultcache(key = 1)]
    pub(crate) order_id: String,
    #[cultcache(key = 2)]
    pub(crate) ended_at_unix_millis: u64,
    #[cultcache(key = 3)]
    pub(crate) outcome: Outcome,
    #[cultcache(key = 4)]
    pub(crate) stdout_tail: Vec<u8>,
    #[cultcache(key = 5)]
    pub(crate) stderr_tail: Vec<u8>,
}

/// Tails stay out of `Debug`: they are raw program output.
impl fmt::Debug for OutcomeRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutcomeRecord")
            .field("order_id", &self.order_id)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PostKind {
    Accepted,
    Result,
}

impl PostKind {
    fn as_str(self) -> &'static str {
        match self {
            PostKind::Accepted => "accepted",
            PostKind::Result => "result",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.steward_order_post",
    schema = "idunn.steward_order_post.v1"
)]
pub(crate) struct PostRecord {
    #[cultcache(key = 0)]
    pub(crate) schema_version: String,
    /// `<order_id>:<accepted|result>`.
    #[cultcache(key = 1)]
    pub(crate) post_key: String,
    #[cultcache(key = 2)]
    pub(crate) event_id: String,
    #[cultcache(key = 3)]
    pub(crate) posted_at_unix_millis: u64,
}

// ------------------------------------------------------------------ store --

/// `orders.cc`: root-only, written by `idunn orders serve` alone.
pub(crate) struct OrderStore {
    directory: PathBuf,
}

#[derive(Default)]
struct Ledger {
    orders: Vec<OrderRecord>,
    starts: Vec<StartRecord>,
    outcomes: Vec<OutcomeRecord>,
    posts: Vec<PostRecord>,
}

impl Ledger {
    fn start_of(&self, order_id: &str) -> Option<&StartRecord> {
        self.starts.iter().find(|start| start.order_id == order_id)
    }

    fn outcome_of(&self, order_id: &str) -> Option<&OutcomeRecord> {
        self.outcomes
            .iter()
            .find(|outcome| outcome.order_id == order_id)
    }

    fn posted(&self, order_id: &str, kind: PostKind) -> bool {
        let key = post_key(order_id, kind);
        self.posts.iter().any(|post| post.post_key == key)
    }
}

fn post_key(order_id: &str, kind: PostKind) -> String {
    format!("{order_id}:{}", kind.as_str())
}

impl OrderStore {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_owned(),
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.join("orders.cc")
    }

    fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.directory).context("creating the orders directory")?;
        std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))
            .context("making the orders directory private")
    }

    /// Insert a record whose (type, key) is absent. `false` means it was there.
    fn put<T: DatabaseEntry + Serialize>(&self, key: &str, schema: &str, value: &T) -> Result<bool> {
        self.prepare()?;
        let path = self.path();
        let now = now_millis()?;
        let inserted = SingleFileMessagePackBackingStore::new(&path).compare_exchange(
            &[CultCacheExpectedEnvelope {
                r#type: T::TYPE.into(),
                key: key.into(),
                current: None,
            }],
            &[typed_envelope(key, T::TYPE, schema, value, now)?],
        )?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("making the orders store private")?;
        Ok(inserted)
    }

    fn ledger(&self) -> Result<Ledger> {
        let path = self.path();
        let mut ledger = Ledger::default();
        if !path.exists() {
            return Ok(ledger);
        }
        let envelopes = SingleFileMessagePackBackingStore::new(&path)
            .pull_all_read_only_snapshot()
            .context("reading the orders store")?;
        for envelope in &envelopes {
            let schema = envelope.schema_id.as_deref();
            let key = envelope.key.as_str();
            match envelope.r#type.as_str() {
                OrderRecord::TYPE if schema == Some(ORDER_SCHEMA) => {
                    let record: OrderRecord = decode_record(envelope)?;
                    ensure!(record.order_id == key, "an order is stored under another key");
                    ledger.orders.push(record);
                }
                StartRecord::TYPE if schema == Some(START_SCHEMA) => {
                    let record: StartRecord = decode_record(envelope)?;
                    ensure!(record.order_id == key, "a start is stored under another key");
                    ledger.starts.push(record);
                }
                OutcomeRecord::TYPE if schema == Some(OUTCOME_SCHEMA) => {
                    let record: OutcomeRecord = decode_record(envelope)?;
                    ensure!(record.order_id == key, "an outcome is stored under another key");
                    ledger.outcomes.push(record);
                }
                PostRecord::TYPE if schema == Some(POST_SCHEMA) => {
                    let record: PostRecord = decode_record(envelope)?;
                    ensure!(record.post_key == key, "a post is stored under another key");
                    ledger.posts.push(record);
                }
                _ => bail!("the orders store holds a document that is not an order record"),
            }
        }
        ledger
            .orders
            .sort_by_key(|order| (order.issued_at_unix_millis, order.order_id.clone()));
        Ok(ledger)
    }
}

// ------------------------------------------------------------------ ports --

/// Which way systemd moves along a dependency edge when an order is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    /// Units a start brings up with this one.
    Pulls,
    /// Units a start stops: the unit's `Conflicts=` and the units that conflict with it.
    Conflicts,
    /// Units a stop takes down with this one.
    Stops,
}

/// The `systemctl show` properties the closure reads, and the way each one
/// moves. This is the only list: the request and the parse both read it.
const EDGES: [(&str, Edge); 13] = [
    ("Requires", Edge::Pulls),
    ("Requisite", Edge::Pulls),
    ("BindsTo", Edge::Pulls),
    ("Wants", Edge::Pulls),
    ("Upholds", Edge::Pulls),
    ("Conflicts", Edge::Conflicts),
    ("ConflictedBy", Edge::Conflicts),
    ("RequiredBy", Edge::Stops),
    ("BoundBy", Edge::Stops),
    ("RequisiteOf", Edge::Stops),
    ("ConsistsOf", Edge::Stops),
    ("PropagatesStopTo", Edge::Stops),
    ("StopPropagatedFrom", Edge::Stops),
];

/// What `systemctl show` says about one unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UnitFacts {
    pub(crate) id: String,
    pub(crate) names: Vec<String>,
    pub(crate) not_found: bool,
    pub(crate) running: bool,
    pub(crate) pulls: Vec<String>,
    pub(crate) conflicts: Vec<String>,
    pub(crate) stops: Vec<String>,
}

impl UnitFacts {
    fn identities(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.id).chain(self.names.iter())
    }

    fn along(&self, edge: Edge) -> &[String] {
        match edge {
            Edge::Pulls => &self.pulls,
            Edge::Conflicts => &self.conflicts,
            Edge::Stops => &self.stops,
        }
    }

    fn along_mut(&mut self, edge: Edge) -> &mut Vec<String> {
        match edge {
            Edge::Pulls => &mut self.pulls,
            Edge::Conflicts => &mut self.conflicts,
            Edge::Stops => &mut self.stops,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RunReport {
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) stdout_tail: Vec<u8>,
    pub(crate) stderr_tail: Vec<u8>,
}

pub(crate) trait Systemd: Send + Sync {
    fn show(&self, unit: &str) -> Result<UnitFacts>;
    fn invocation(&self, unit: &str) -> Option<String>;
    /// Run one step as the transient unit `unit`, to its end.
    fn run(&self, unit: &str, step: &Step) -> Result<RunReport>;
}

pub(crate) struct Peer {
    pub(crate) uid: u32,
    pub(crate) issuer_unit: String,
}

pub(crate) trait PeerCredentials: Send + Sync {
    fn peer(&self, stream: &UnixStream) -> Result<Peer>;
}

pub(crate) trait BrakeCheck: Send + Sync {
    /// `true` only when the instance's brake check exits 0.
    fn clear(&self, principal: &str) -> bool;
}

pub(crate) trait Homeserver: Send + Sync {
    /// Idempotent by `txn_id`; returns the event id.
    fn post(&self, txn_id: &str, text: &str) -> Result<String>;
}

pub(crate) struct Ctx {
    pub(crate) store: OrderStore,
    pub(crate) state_store: PathBuf,
    pub(crate) steward_uid: u32,
    pub(crate) principal: String,
    pub(crate) systemd: Arc<dyn Systemd>,
    pub(crate) peers: Arc<dyn PeerCredentials>,
    pub(crate) brake: Arc<dyn BrakeCheck>,
    pub(crate) homeserver: Arc<dyn Homeserver>,
}

// ------------------------------------------------------------------ scope --

fn refused(unit: &str, via: Via) -> Refused {
    Refused::AlwaysRefused {
        unit: unit.to_owned(),
        via,
    }
}

/// The units systemd would touch for one order, read from `systemctl show`.
struct Transaction<'a> {
    systemd: &'a dyn Systemd,
    facts: BTreeMap<String, UnitFacts>,
}

impl Transaction<'_> {
    fn facts(&mut self, unit: &str) -> Result<&UnitFacts> {
        if !self.facts.contains_key(unit) {
            ensure!(
                self.facts.len() < CLOSURE_LIMIT,
                "the units an order would reach are too many to check"
            );
            let facts = self.systemd.show(unit)?;
            ensure!(!facts.not_found, "systemd has no such unit to check");
            self.facts.insert(unit.to_owned(), facts);
        }
        Ok(&self.facts[unit])
    }

    /// Every unit reached from `seeds` along `edge`, seeds included.
    fn reach(&mut self, seeds: &BTreeSet<String>, edge: Edge) -> Result<BTreeSet<String>> {
        let mut reached = BTreeSet::new();
        let mut queue: VecDeque<String> = seeds.iter().cloned().collect();
        while let Some(next) = queue.pop_front() {
            if !reached.insert(next.clone()) {
                continue;
            }
            queue.extend(self.facts(&next)?.along(edge).iter().cloned());
        }
        Ok(reached)
    }

    /// Every unit a start of `started` stops through `Conflicts=`, and what
    /// that stop takes down with it.
    fn conflicted(&mut self, started: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        let mut seeds = BTreeSet::new();
        for unit in started {
            seeds.extend(self.facts(unit)?.along(Edge::Conflicts).iter().cloned());
        }
        self.reach(&seeds, Edge::Stops)
    }
}

/// Would `verb` on `unit` reach an always-refused unit: by its name, by an
/// alias it resolves from, or through the transaction systemd builds for it.
/// A start pulls in what the unit requires or wants and stops what conflicts;
/// a stop takes down what depends on the unit; a restart does both. A unit
/// the transaction would stop is refused when it is refused; a unit it would
/// start is refused when it is refused and not running. Every unit in the
/// closure must be one systemd can show.
fn refusal_for(systemd: &dyn Systemd, unit: &str, verb: Verb) -> Result<Option<Refused>> {
    if is_refused_name(unit) {
        return Ok(Some(refused(unit, Via::Name)));
    }
    let mut transaction = Transaction {
        systemd,
        facts: BTreeMap::new(),
    };
    if let Some(name) = transaction
        .facts(unit)?
        .identities()
        .find(|name| is_refused_name(name))
    {
        return Ok(Some(refused(name, Via::Alias)));
    }
    let root = BTreeSet::from([unit.to_owned()]);
    let mut stopped = BTreeSet::new();
    let mut started = BTreeSet::new();
    match verb {
        Verb::Reload => {}
        Verb::Start => started = transaction.reach(&root, Edge::Pulls)?,
        Verb::Stop => stopped = transaction.reach(&root, Edge::Stops)?,
        Verb::Restart => {
            stopped = transaction.reach(&root, Edge::Stops)?;
            started = transaction.reach(&stopped, Edge::Pulls)?;
        }
    }
    stopped.extend(transaction.conflicted(&started)?);
    for (units, only_if_stopped) in [(&stopped, false), (&started, true)] {
        for member in units {
            let facts = transaction.facts(member)?;
            if only_if_stopped && facts.running {
                continue;
            }
            if let Some(name) = facts.identities().find(|name| is_refused_name(name)) {
                return Ok(Some(refused(name, Via::Propagation)));
            }
        }
    }
    Ok(None)
}

// -------------------------------------------------------------- execution --

fn tail(bytes: &[u8]) -> Vec<u8> {
    bytes[bytes.len().saturating_sub(TAIL_BYTES)..].to_vec()
}

fn conclude(
    ctx: &Ctx,
    order_id: &str,
    outcome: Outcome,
    stdout_tail: &[u8],
    stderr_tail: &[u8],
) -> Result<()> {
    ctx.store.put(
        order_id,
        OUTCOME_SCHEMA,
        &OutcomeRecord {
            schema_version: OUTCOME_SCHEMA.into(),
            order_id: order_id.into(),
            ended_at_unix_millis: now_millis()?,
            outcome,
            stdout_tail: tail(stdout_tail),
            stderr_tail: tail(stderr_tail),
        },
    )?;
    Ok(())
}

fn order_uuid(order_id: &str) -> &str {
    order_id.strip_prefix("ord-").unwrap_or(order_id)
}

pub(crate) fn requester(principal: &str, order_id: &str) -> String {
    format!("steward:{principal} order {order_id}")
}

/// Run one accepted order. A start record is written before anything is
/// actuated and an order that has one is never run again.
pub(crate) fn execute(ctx: &Ctx, order_id: &str) -> Result<()> {
    let ledger = ctx.store.ledger()?;
    let order = ledger
        .orders
        .iter()
        .find(|order| order.order_id == order_id)
        .context("the order is not in the store")?;
    if ledger.start_of(order_id).is_some() || ledger.outcome_of(order_id).is_some() {
        return Ok(());
    }
    if order.operation.validate().is_err() {
        return conclude(
            ctx,
            order_id,
            Outcome::Failed {
                exit_code: None,
                signal: None,
            },
            b"",
            b"the stored order no longer satisfies the grammar",
        );
    }
    if let Operation::UnitAction { verb, unit } = &order.operation {
        match refusal_for(ctx.systemd.as_ref(), unit, *verb) {
            Ok(None) => {}
            Ok(Some(hit)) => return conclude(ctx, order_id, Outcome::Refused(hit), b"", b""),
            Err(_) => {
                return conclude(
                    ctx,
                    order_id,
                    Outcome::Failed {
                        exit_code: None,
                        signal: None,
                    },
                    b"",
                    b"the unit could not be resolved, so it was not acted on",
                );
            }
        }
    }
    if !ctx.brake.clear(&order.principal) {
        return conclude(ctx, order_id, Outcome::Refused(Refused::Braked), b"", b"");
    }
    let uuid = order_uuid(order_id);
    let command_id = format!("up-{uuid}");
    let steps = order.operation.steps();
    let actuation = match &order.operation {
        Operation::Deploy { .. } => command_id.clone(),
        _ => format!("idunn-order-{uuid}"),
    };
    let started = StartRecord {
        schema_version: START_SCHEMA.into(),
        order_id: order_id.into(),
        started_at_unix_millis: now_millis()?,
        actuation,
    };
    if !ctx.store.put(order_id, START_SCHEMA, &started)? {
        return Ok(());
    }
    if let Operation::Deploy { selector } = &order.operation {
        let outcome = match commit_deployment_command(
            &ctx.state_store,
            &command_id,
            selector,
            &requester(&order.principal, order_id),
        ) {
            Ok(_) => Outcome::Submitted { command_id },
            Err(_) => Outcome::Failed {
                exit_code: None,
                signal: None,
            },
        };
        return conclude(ctx, order_id, outcome, b"", b"");
    }
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut outcome = Outcome::Ran;
    for (index, step) in steps.iter().enumerate() {
        let unit = if steps.len() == 1 {
            format!("idunn-order-{uuid}")
        } else {
            format!("idunn-order-{uuid}-{}", index + 1)
        };
        match ctx.systemd.run(&unit, step) {
            Ok(report) => {
                stdout = report.stdout_tail;
                stderr = report.stderr_tail;
                if report.exit_code != Some(0) {
                    outcome = Outcome::Failed {
                        exit_code: report.exit_code,
                        signal: report.signal,
                    };
                    break;
                }
            }
            Err(_) => {
                outcome = Outcome::Failed {
                    exit_code: None,
                    signal: None,
                };
                stderr = b"the transient unit could not be started".to_vec();
                break;
            }
        }
    }
    conclude(ctx, order_id, outcome, &stdout, &stderr)
}

/// On service start: an order with a start record and no outcome ran, or began
/// to, in a life that ended. It is Lost and never run again. Returns the
/// orders that never started, oldest first.
pub(crate) fn recover(ctx: &Ctx) -> Result<Vec<String>> {
    let ledger = ctx.store.ledger()?;
    for start in &ledger.starts {
        if ledger.outcome_of(&start.order_id).is_none() {
            conclude(ctx, &start.order_id, Outcome::Lost, b"", b"")?;
        }
    }
    Ok(ledger
        .orders
        .iter()
        .filter(|order| {
            ledger.start_of(&order.order_id).is_none() && ledger.outcome_of(&order.order_id).is_none()
        })
        .map(|order| order.order_id.clone())
        .collect())
}

// ---------------------------------------------------------------- posting --

struct Due {
    order_id: String,
    kind: PostKind,
    text: String,
}

fn quoted(reason: &str) -> String {
    format!("\"{}\"", escape(reason.as_bytes()))
}

fn accepted_text(order: &OrderRecord) -> String {
    format!(
        "Steward order {} accepted: {}\nissuer {} (invocation {})\nreason: {}",
        order.order_id,
        escape(order.operation.describe().as_bytes()),
        escape(order.issuer_unit.as_bytes()),
        escape(order.issuer_invocation.as_bytes()),
        quoted(&order.reason)
    )
}

/// A deploy's command has finished once its state is neither queued nor running.
fn deploy_finished(state: &str) -> bool {
    !matches!(state, "queued" | "running")
}

fn due_posts(ctx: &Ctx, ledger: &Ledger) -> Vec<Due> {
    let mut due = Vec::new();
    for order in &ledger.orders {
        let id = &order.order_id;
        if !ledger.posted(id, PostKind::Accepted) {
            due.push(Due {
                order_id: id.clone(),
                kind: PostKind::Accepted,
                text: accepted_text(order),
            });
        }
        if ledger.posted(id, PostKind::Result) {
            continue;
        }
        let Some(record) = ledger.outcome_of(id) else {
            continue;
        };
        let mut line = record.outcome.describe();
        if let Outcome::Submitted { command_id } = &record.outcome {
            match command_state(&ctx.state_store, command_id) {
                Ok((state, detail)) if deploy_finished(state) => {
                    line = format!("{line}: {state} {}", escape(detail.as_bytes()));
                }
                _ => continue,
            }
        }
        due.push(Due {
            order_id: id.clone(),
            kind: PostKind::Result,
            text: format!(
                "Steward order {id}: {line}\n{}",
                escape(order.operation.describe().as_bytes())
            ),
        });
    }
    due
}

/// Post what is due, in order, stopping at the first failure. Returns how many
/// went out.
pub(crate) fn post_pass(ctx: &Ctx) -> Result<usize> {
    let ledger = ctx.store.ledger()?;
    let mut sent = 0;
    for due in due_posts(ctx, &ledger) {
        let transaction = format!("idunn-{}-{}", due.order_id, due.kind.as_str());
        let event_id = ctx.homeserver.post(&transaction, &due.text)?;
        ctx.store.put(
            &post_key(&due.order_id, due.kind),
            POST_SCHEMA,
            &PostRecord {
                schema_version: POST_SCHEMA.into(),
                post_key: post_key(&due.order_id, due.kind),
                event_id,
                posted_at_unix_millis: now_millis()?,
            },
        )?;
        sent += 1;
    }
    Ok(sent)
}

// ----------------------------------------------------------------- frames --

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrderRequest {
    pub(crate) operation: Operation,
    pub(crate) reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShowRequest {
    pub(crate) id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Request {
    Order(OrderRequest),
    Show(ShowRequest),
    List,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OrderView {
    pub(crate) order: OrderRecord,
    pub(crate) start: Option<StartRecord>,
    pub(crate) outcome: Option<OutcomeRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OrderLine {
    pub(crate) order_id: String,
    pub(crate) status: String,
    pub(crate) summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Response {
    Accepted { order_id: String },
    Refused(Refusal),
    Show(Box<OrderView>),
    List(Vec<OrderLine>),
}

fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = rmp_serde::to_vec_named(value)?;
    stream.write_all(&u32::try_from(bytes.len())?.to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(stream.flush()?)
}

fn read_frame(stream: &mut impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(length <= limit, "a frame exceeds its limit");
    let mut bytes = vec![0u8; length];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn status_word(view: &OrderView) -> &'static str {
    match (&view.outcome, &view.start) {
        (Some(record), _) => match record.outcome {
            Outcome::Ran => "ran",
            Outcome::Failed { .. } => "failed",
            Outcome::Refused(_) => "refused",
            Outcome::Lost => "lost",
            Outcome::Submitted { .. } => "submitted",
        },
        (None, Some(_)) => "running",
        (None, None) => "accepted",
    }
}

fn view_of(ledger: &Ledger, order: &OrderRecord) -> OrderView {
    OrderView {
        order: order.clone(),
        start: ledger.start_of(&order.order_id).cloned(),
        outcome: ledger.outcome_of(&order.order_id).cloned(),
    }
}

fn answer(stream: &mut UnixStream, response: Response) -> Result<()> {
    write_frame(stream, &response)
}

fn refuse(stream: &mut UnixStream, field: &'static str, code: Code) -> Result<()> {
    answer(stream, Response::Refused(Refusal::new(field, code)))
}

/// Serve one connection. The peer is authenticated before its frame is read.
pub(crate) fn handle(
    ctx: &Ctx,
    stream: &mut UnixStream,
    queue: &mpsc::Sender<String>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let peer = ctx.peers.peer(stream)?;
    if peer.uid != ctx.steward_uid {
        eprintln!("steward socket: refused a peer that is not the steward");
        return refuse(stream, "peer", Code::Unauthorized);
    }
    let request = read_frame(stream, REQUEST_FRAME_LIMIT)
        .ok()
        .and_then(|bytes| rmp_serde::from_slice::<Request>(&bytes).ok());
    let Some(request) = request else {
        eprintln!("steward socket: refused a malformed frame");
        return refuse(stream, "frame", Code::Malformed);
    };
    match request {
        Request::Order(request) => {
            let checked =
                validate_reason(&request.reason).and_then(|()| request.operation.validate());
            if let Err(refusal) = checked {
                eprintln!("steward socket: refused an order at the grammar");
                return answer(stream, Response::Refused(refusal));
            }
            let order = OrderRecord {
                schema_version: ORDER_SCHEMA.into(),
                order_id: format!("ord-{}", Uuid::new_v4()),
                principal: ctx.principal.clone(),
                issuer_uid: peer.uid,
                issuer_invocation: ctx.systemd.invocation(&peer.issuer_unit).unwrap_or_default(),
                issuer_unit: peer.issuer_unit,
                issued_at_unix_millis: now_millis()?,
                operation: request.operation,
                reason: request.reason,
            };
            match ctx.store.put(&order.order_id, ORDER_SCHEMA, &order) {
                Ok(true) => {}
                _ => return refuse(stream, "store", Code::Unavailable),
            }
            queue
                .send(order.order_id.clone())
                .map_err(|_| anyhow!("the executor has stopped"))?;
            answer(
                stream,
                Response::Accepted {
                    order_id: order.order_id,
                },
            )
        }
        Request::Show(request) => {
            if request.id.strip_prefix("ord-").map(Uuid::parse_str).is_none_or(|parsed| parsed.is_err())
            {
                return refuse(stream, "id", Code::BadShape);
            }
            let ledger = ctx.store.ledger()?;
            match ledger.orders.iter().find(|order| order.order_id == request.id) {
                Some(order) => answer(stream, Response::Show(Box::new(view_of(&ledger, order)))),
                None => refuse(stream, "id", Code::Unknown),
            }
        }
        Request::List => {
            let ledger = ctx.store.ledger()?;
            let skip = ledger.orders.len().saturating_sub(LIST_LIMIT);
            let lines = ledger
                .orders
                .iter()
                .skip(skip)
                .map(|order| {
                    let view = view_of(&ledger, order);
                    OrderLine {
                        order_id: order.order_id.clone(),
                        status: status_word(&view).into(),
                        summary: order.operation.describe(),
                    }
                })
                .collect();
            answer(stream, Response::List(lines))
        }
    }
}

// ------------------------------------------------------------- real ports --

/// `systemctl` and `systemd-run`, found by name.
pub(crate) struct HostSystemd;

/// The names in one `systemctl show` list value. systemd quotes a name that
/// holds a backslash and doubles the backslash inside the quotes.
fn parse_names(value: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut open, mut escaped) = (false, false, false);
    for character in value.chars() {
        match character {
            _ if escaped => {
                current.push(character);
                escaped = false;
            }
            '\\' if quoted => escaped = true,
            '"' => {
                quoted = !quoted;
                open = true;
            }
            _ if character.is_whitespace() && !quoted => {
                if open {
                    names.push(std::mem::take(&mut current));
                    open = false;
                }
            }
            _ => {
                current.push(character);
                open = true;
            }
        }
    }
    if open {
        names.push(current);
    }
    names
}

/// The properties one `systemctl show` is asked for: identity, state, edges.
fn show_properties() -> String {
    ["Id", "Names", "LoadState", "ActiveState"]
        .into_iter()
        .chain(EDGES.iter().map(|(property, _)| *property))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_show(output: &str) -> Result<UnitFacts> {
    let mut facts = UnitFacts::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Id" => facts.id = value.to_owned(),
            "Names" => facts.names = parse_names(value),
            "LoadState" => facts.not_found = value == "not-found",
            "ActiveState" => {
                facts.running = matches!(value, "active" | "activating" | "reloading");
            }
            _ => {
                if let Some((_, edge)) = EDGES.iter().find(|(property, _)| *property == key) {
                    facts.along_mut(*edge).extend(parse_names(value));
                }
            }
        }
    }
    ensure!(!facts.id.is_empty(), "systemctl named no unit");
    Ok(facts)
}

impl Systemd for HostSystemd {
    fn show(&self, unit: &str) -> Result<UnitFacts> {
        let output = Command::new("systemctl")
            .args(["show", "--no-pager"])
            .arg(format!("--property={}", show_properties()))
            .args(["--", unit])
            .stdin(Stdio::null())
            .output()
            .context("running systemctl show")?;
        ensure!(output.status.success(), "systemctl show failed");
        parse_show(&String::from_utf8_lossy(&output.stdout))
    }

    fn invocation(&self, unit: &str) -> Option<String> {
        let output = Command::new("systemctl")
            .args(["show", "--no-pager", "--property=InvocationID", "--value", "--"])
            .arg(unit)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (output.status.success() && !value.is_empty()).then_some(value)
    }

    fn run(&self, unit: &str, step: &Step) -> Result<RunReport> {
        let mut command = Command::new("systemd-run");
        command.args(["--unit", unit, "--wait", "--pipe", "--collect"]);
        command.args(["-p", "RuntimeMaxSec=15min"]);
        for (name, value) in &step.environment {
            command.arg(format!("--setenv={name}={value}"));
        }
        let output = command
            .arg("--")
            .args(&step.argv)
            .stdin(Stdio::null())
            .output()
            .context("running systemd-run")?;
        Ok(RunReport {
            exit_code: output.status.code(),
            signal: output.status.signal(),
            stdout_tail: tail(&output.stdout),
            stderr_tail: tail(&output.stderr),
        })
    }
}

/// The last `.service` or `.scope` in a process's cgroup path names its issuer.
fn unit_from_cgroup(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| line.starts_with("0::") || line.contains(":name=systemd:"))
        .unwrap_or_default();
    let path = line.rsplit_once(':').map_or("", |(_, path)| path);
    path.rsplit('/')
        .find(|segment| segment.ends_with(".service") || segment.ends_with(".scope"))
        .filter(|segment| segment.len() <= UNIT_LIMIT)
        .map_or_else(|| "unknown".to_owned(), str::to_owned)
}

/// Kernel peer credentials (`SO_PEERCRED`) and the peer's cgroup.
pub(crate) struct KernelPeers;

impl PeerCredentials for KernelPeers {
    fn peer(&self, stream: &UnixStream) -> Result<Peer> {
        use std::os::fd::AsRawFd;
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the descriptor is a live socket and the pointer and length
        // describe `credentials`, which outlives the call.
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        ensure!(
            status == 0 && length as usize == std::mem::size_of::<libc::ucred>(),
            "peer credentials are unavailable"
        );
        let cgroup =
            std::fs::read_to_string(format!("/proc/{}/cgroup", credentials.pid)).unwrap_or_default();
        Ok(Peer {
            uid: credentials.uid,
            issuer_unit: unit_from_cgroup(&cgroup),
        })
    }
}

/// `mind-control --instance <principal> check`; a missing binary is a held brake.
pub(crate) struct MindControl {
    pub(crate) program: PathBuf,
}

impl BrakeCheck for MindControl {
    fn clear(&self, principal: &str) -> bool {
        Command::new(&self.program)
            .args(["--instance", principal, "check"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// The Synapse client API on loopback, over plain HTTP/1.0 so the reply is
/// never chunked. The token is read from its credential file for each post.
pub(crate) struct Synapse {
    pub(crate) address: std::net::SocketAddr,
    pub(crate) room: String,
    pub(crate) token_file: PathBuf,
}

fn percent_encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

impl Homeserver for Synapse {
    fn post(&self, txn_id: &str, text: &str) -> Result<String> {
        let token = std::fs::read_to_string(&self.token_file)
            .context("the homeserver token is unreadable")?;
        let token = token.trim();
        ensure!(
            !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()),
            "the homeserver token is not a bearer token"
        );
        let body = serde_json::to_vec(&serde_json::json!({"msgtype": "m.text", "body": text}))?;
        let head = format!(
            "PUT /_matrix/client/v3/rooms/{}/send/m.room.message/{txn_id} HTTP/1.0\r\n\
             Host: {}\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            percent_encode(&self.room),
            self.address,
            body.len()
        );
        let mut socket = std::net::TcpStream::connect_timeout(&self.address, Duration::from_secs(5))
            .context("the homeserver is unreachable")?;
        socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        socket.set_write_timeout(Some(Duration::from_secs(10)))?;
        socket.write_all(head.as_bytes())?;
        socket.write_all(&body)?;
        let mut reply = Vec::new();
        socket
            .take(RESPONSE_FRAME_LIMIT as u64)
            .read_to_end(&mut reply)
            .context("the homeserver reply was cut short")?;
        let split = reply
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .context("the homeserver reply has no body")?;
        let status = String::from_utf8_lossy(&reply[..split]);
        ensure!(
            status.split_whitespace().nth(1) == Some("200"),
            "the homeserver refused the post"
        );
        #[derive(Deserialize)]
        struct Sent {
            event_id: String,
        }
        let sent: Sent = serde_json::from_slice(&reply[split + 4..])
            .map_err(|_| anyhow!("the homeserver reply has no event id"))?;
        Ok(sent.event_id)
    }
}

// ---------------------------------------------------------------- binding --

/// `/etc/gamecult/idunn/steward.toml`: who the steward is and where to post.
/// It lists no unit of any kind, and any field outside this set is refused.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StewardBinding {
    pub(crate) principal: String,
    pub(crate) user: String,
    pub(crate) mind_control: PathBuf,
    pub(crate) room: String,
    pub(crate) homeserver: String,
    pub(crate) token_credential: String,
}

impl StewardBinding {
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let binding: Self = toml::from_str(text)
            .map_err(|error| anyhow!("the steward binding does not parse: {}", error.message()))?;
        binding.validate()?;
        Ok(binding)
    }

    fn validate(&self) -> Result<()> {
        require_id(&self.principal, "steward principal")?;
        ensure!(
            !self.principal.starts_with('-') && !self.principal.contains('/'),
            "steward principal is invalid"
        );
        let user_ok = !self.user.is_empty()
            && self.user.len() <= 32
            && self.user.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
            && !self.user.starts_with(|first: char| first.is_ascii_digit() || first == '-');
        ensure!(user_ok && self.user != "root", "steward user is invalid or root");
        ensure!(
            self.mind_control.is_absolute(),
            "mind_control must be an absolute path"
        );
        let room_ok = self.room.starts_with('!')
            && self.room.len() <= 255
            && self.room.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'!' | b'.' | b'_' | b':' | b'=' | b'-')
            });
        ensure!(room_ok, "room must be a Matrix room id");
        self.address()?;
        let credential_ok = !self.token_credential.is_empty()
            && !self.token_credential.starts_with('.')
            && self.token_credential.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
            });
        ensure!(credential_ok, "token_credential is not a credential name");
        Ok(())
    }

    fn address(&self) -> Result<std::net::SocketAddr> {
        let address: std::net::SocketAddr = self
            .homeserver
            .strip_prefix("http://")
            .and_then(|rest| rest.parse().ok())
            .context("homeserver must be http://<loopback address>:<port>")?;
        ensure!(address.ip().is_loopback(), "homeserver must be loopback");
        Ok(address)
    }
}

/// Root-admitted: owned by root and writable by no one else.
fn admitted(owner: u32, mode: u32) -> bool {
    owner == 0 && mode & 0o022 == 0
}

fn load_binding(path: &Path) -> Result<StewardBinding> {
    let metadata = std::fs::metadata(path).context("reading the steward binding")?;
    ensure!(
        metadata.is_file() && admitted(metadata.uid(), metadata.mode()),
        "the steward binding is not a root-owned file closed to writes"
    );
    let text = std::fs::read_to_string(path).context("reading the steward binding")?;
    StewardBinding::parse(&text)
}

fn user_uid(name: &str) -> Result<u32> {
    let name = std::ffi::CString::new(name)?;
    // SAFETY: `passwd` is plain data and getpwnam_r fills it, with strings in
    // `buffer`, which outlives every read of them below (none are read).
    let (status, found, uid) = unsafe {
        let mut passwd: libc::passwd = std::mem::zeroed();
        let mut buffer = vec![0 as libc::c_char; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let status = libc::getpwnam_r(
            name.as_ptr(),
            &mut passwd,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        );
        (status, !result.is_null(), passwd.pw_uid)
    };
    ensure!(status == 0 && found, "the steward user is unknown");
    ensure!(uid != 0, "the steward user resolves to root");
    Ok(uid)
}

// ------------------------------------------------------------------ serve --

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServeOptions {
    pub(crate) state_store: PathBuf,
    pub(crate) store: PathBuf,
    pub(crate) steward_binding: PathBuf,
    pub(crate) socket: PathBuf,
}

const DEFAULT_STORE: &str = "/var/lib/gamecult/idunn-orders";
const DEFAULT_BINDING: &str = "/etc/gamecult/idunn/steward.toml";
const DEFAULT_SOCKET: &str = "/run/idunn-orders/steward.sock";

pub(crate) fn parse_serve(mut args: impl Iterator<Item = String>) -> Result<ServeOptions> {
    ensure!(
        args.next().as_deref() == Some("serve"),
        "idunn orders takes the subcommand serve"
    );
    let mut options = ServeOptions {
        state_store: default_state_store(),
        store: DEFAULT_STORE.into(),
        steward_binding: DEFAULT_BINDING.into(),
        socket: DEFAULT_SOCKET.into(),
    };
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--state-store" => options.state_store = path_value(&mut args, "--state-store")?,
            "--store" => options.store = path_value(&mut args, "--store")?,
            "--steward-binding" => {
                options.steward_binding = path_value(&mut args, "--steward-binding")?
            }
            "--socket" => options.socket = path_value(&mut args, "--socket")?,
            _ => bail!("an orders serve option is not part of the grammar"),
        }
    }
    Ok(options)
}

/// Run a thread whose end ends the service: systemd restarts it, and the
/// start recovery settles what it left.
fn vital(name: &'static str, work: impl FnOnce() + Send + 'static) {
    struct ExitOnDrop(&'static str);
    impl Drop for ExitOnDrop {
        fn drop(&mut self) {
            eprintln!("steward orders: the {} thread ended", self.0);
            std::process::exit(1);
        }
    }
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _guard = ExitOnDrop(name);
            work();
        })
        .expect("a thread can be spawned");
}

fn bind_socket(path: &Path) -> Result<UnixListener> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        ensure!(
            metadata.file_type().is_socket(),
            "the socket path holds something that is not a socket"
        );
        std::fs::remove_file(path).context("removing a stale socket")?;
    }
    let listener = UnixListener::bind(path).context("binding the steward socket")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .context("setting the steward socket mode")?;
    Ok(listener)
}

pub(crate) fn serve(options: ServeOptions) -> Result<()> {
    let binding = load_binding(&options.steward_binding)?;
    let credentials = std::env::var_os("CREDENTIALS_DIRECTORY")
        .map(PathBuf::from)
        .context("CREDENTIALS_DIRECTORY is unset; the token comes from LoadCredential")?;
    let ctx = Arc::new(Ctx {
        store: OrderStore::new(&options.store),
        state_store: options.state_store,
        steward_uid: user_uid(&binding.user)?,
        principal: binding.principal.clone(),
        systemd: Arc::new(HostSystemd),
        peers: Arc::new(KernelPeers),
        brake: Arc::new(MindControl {
            program: binding.mind_control.clone(),
        }),
        homeserver: Arc::new(Synapse {
            address: binding.address()?,
            room: binding.room.clone(),
            token_file: credentials.join(&binding.token_credential),
        }),
    });
    ctx.store.prepare()?;
    let pending = recover(&ctx)?;
    let listener = bind_socket(&options.socket)?;
    let (queue, orders) = mpsc::channel::<String>();
    for order_id in pending {
        queue.send(order_id)?;
    }
    let executor = Arc::clone(&ctx);
    vital("executor", move || {
        for order_id in orders {
            if let Err(error) = execute(&executor, &order_id) {
                eprintln!("steward orders: {order_id} was not run to a record: {error:#}");
            }
        }
    });
    let poster = Arc::clone(&ctx);
    vital("poster", move || {
        let mut failures = 0u32;
        loop {
            let pause = match post_pass(&poster) {
                Ok(_) => {
                    failures = 0;
                    2
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    eprintln!("steward orders: posting is delayed: {error:#}");
                    (2u64 << failures.min(5)).min(60)
                }
            };
            thread::sleep(Duration::from_secs(pause));
        }
    });
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle(&ctx, &mut stream, &queue) {
                    eprintln!("steward socket: a connection ended in error: {error:#}");
                }
            }
            Err(error) => eprintln!("steward socket: accept failed: {}", error.kind()),
        }
    }
    bail!("the steward socket stopped accepting")
}

// ----------------------------------------------------------------- client --

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OrderAction {
    Issue { operation: Operation, reason: String },
    Show { id: String },
    List,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OrderCommand {
    pub(crate) socket: PathBuf,
    pub(crate) action: OrderAction,
}

pub(crate) fn parse_order(mut args: impl Iterator<Item = String>) -> Result<OrderCommand> {
    let kind = args.next().context("idunn order requires an operation")?;
    let mut socket = PathBuf::from(DEFAULT_SOCKET);
    let mut reason = None;
    let mut positional = Vec::new();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--socket" => socket = path_value(&mut args, "--socket")?,
            "--reason" => reason = Some(string_value(&mut args, "--reason")?),
            word if word.starts_with('-') => bail!("an order option is not part of the grammar"),
            _ => positional.push(argument),
        }
    }
    let action = match kind.as_str() {
        "list" => {
            ensure!(positional.is_empty() && reason.is_none(), "list takes no arguments");
            OrderAction::List
        }
        "show" => {
            ensure!(positional.len() == 1 && reason.is_none(), "show takes one order id");
            OrderAction::Show {
                id: positional.remove(0),
            }
        }
        "package-install" | "unit" | "deploy" => {
            let operation = match kind.as_str() {
                "package-install" => Operation::PackageInstall {
                    packages: positional,
                },
                "unit" => {
                    ensure!(positional.len() == 2, "unit takes a verb and a unit");
                    let verb = Verb::parse(&positional[0])
                        .context("the unit verb is not start, stop, restart or reload")?;
                    Operation::UnitAction {
                        verb,
                        unit: positional.remove(1),
                    }
                }
                _ => {
                    ensure!(positional.len() == 1, "deploy takes one selector");
                    Operation::Deploy {
                        selector: positional.remove(0),
                    }
                }
            };
            let reason = reason.context("an order requires --reason")?;
            validate_reason(&reason)?;
            operation.validate()?;
            OrderAction::Issue { operation, reason }
        }
        _ => bail!("the order operation is not part of the grammar"),
    };
    Ok(OrderCommand { socket, action })
}

fn call(socket: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).context("connecting to the steward socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    write_frame(&mut stream, request)?;
    let bytes = read_frame(&mut stream, RESPONSE_FRAME_LIMIT)?;
    rmp_serde::from_slice(&bytes).map_err(|_| anyhow!("the steward socket sent an unreadable reply"))
}

fn render_tail(lines: &mut Vec<Vec<u8>>, stream: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    lines.push(format!("  {stream} (last {} bytes)", bytes.len()).into_bytes());
    let text = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    lines.extend(text.split(|byte| *byte == b'\n').map(|line| {
        let mut shown = b"    | ".to_vec();
        shown.extend_from_slice(line);
        shown
    }));
}

/// Raw lines for the terminal; `emit` is their only way out.
fn render_view(view: &OrderView) -> Vec<Vec<u8>> {
    let order = &view.order;
    let mut lines: Vec<Vec<u8>> = vec![
        format!("order {}", order.order_id).into_bytes(),
        format!("status {}", status_word(view)).into_bytes(),
        format!("operation {}", order.operation.describe()).into_bytes(),
        format!(
            "issuer {} (invocation {}, uid {})",
            order.issuer_unit, order.issuer_invocation, order.issuer_uid
        )
        .into_bytes(),
        format!("reason {}", quoted(&order.reason)).into_bytes(),
    ];
    if let Some(start) = &view.start {
        lines.push(format!("started {}", start.actuation).into_bytes());
    }
    if let Some(record) = &view.outcome {
        lines.push(format!("outcome {}", record.outcome.describe()).into_bytes());
        render_tail(&mut lines, "stdout", &record.stdout_tail);
        render_tail(&mut lines, "stderr", &record.stderr_tail);
    }
    lines
}

pub(crate) fn run_order(command: OrderCommand) -> Result<()> {
    let request = match command.action {
        OrderAction::Issue { operation, reason } => Request::Order(OrderRequest { operation, reason }),
        OrderAction::Show { id } => Request::Show(ShowRequest { id }),
        OrderAction::List => Request::List,
    };
    let mut out = std::io::stdout().lock();
    match call(&command.socket, &request)? {
        Response::Accepted { order_id } => writeln!(out, "{order_id}")?,
        Response::Refused(refusal) => bail!("the steward refused the request: {refusal}"),
        Response::Show(view) => emit(&mut out, &render_view(&view))?,
        Response::List(lines) => {
            let lines: Vec<Vec<u8>> = lines
                .iter()
                .map(|line| format!("{} {} {}", line.order_id, line.status, line.summary).into_bytes())
                .collect();
            emit(&mut out, &lines)?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tempfile::TempDir;

    use super::*;

    const STEWARD: u32 = 1000;
    const CANARY: &str = "CANARY-7f3a91";

    /// What `systemctl show` prints for a loaded, active unit with no edges,
    /// plus the given properties (a given key replaces its default).
    fn show_text(id: &str, properties: &[(&str, &str)]) -> String {
        let mut text = format!("Id={id}\n");
        for (key, default) in [("Names", id), ("LoadState", "loaded"), ("ActiveState", "active")] {
            if !properties.iter().any(|(given, _)| *given == key) {
                text += &format!("{key}={default}\n");
            }
        }
        for (key, value) in properties {
            text += &format!("{key}={value}\n");
        }
        text
    }

    struct FakeSystemd {
        units: Mutex<BTreeMap<String, String>>,
        runs: Mutex<Vec<(String, Step)>>,
        report: Mutex<RunReport>,
    }

    impl Systemd for FakeSystemd {
        fn show(&self, unit: &str) -> Result<UnitFacts> {
            let known = self.units.lock().unwrap().get(unit).cloned();
            parse_show(&known.unwrap_or_else(|| show_text(unit, &[])))
        }

        fn invocation(&self, _unit: &str) -> Option<String> {
            Some("invocation-1".into())
        }

        fn run(&self, unit: &str, step: &Step) -> Result<RunReport> {
            self.runs.lock().unwrap().push((unit.into(), step.clone()));
            Ok(self.report.lock().unwrap().clone())
        }
    }

    struct FakePeers(u32);

    impl PeerCredentials for FakePeers {
        fn peer(&self, _stream: &UnixStream) -> Result<Peer> {
            Ok(Peer {
                uid: self.0,
                issuer_unit: "mind-eureka-self.service".into(),
            })
        }
    }

    struct FakeBrake(AtomicBool);

    impl BrakeCheck for FakeBrake {
        fn clear(&self, _principal: &str) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct FakeHome {
        failing: AtomicBool,
        sent: Mutex<Vec<(String, String)>>,
    }

    impl Homeserver for FakeHome {
        fn post(&self, txn_id: &str, text: &str) -> Result<String> {
            ensure!(!self.failing.load(Ordering::SeqCst), "the homeserver is down");
            let mut sent = self.sent.lock().unwrap();
            sent.push((txn_id.into(), text.into()));
            Ok(format!("$event{}", sent.len()))
        }
    }

    struct Harness {
        directory: TempDir,
        ctx: Ctx,
        systemd: Arc<FakeSystemd>,
        brake: Arc<FakeBrake>,
        home: Arc<FakeHome>,
        queue: mpsc::Sender<String>,
        _orders: mpsc::Receiver<String>,
    }

    fn harness_for(peer_uid: u32) -> Harness {
        let directory = TempDir::new().unwrap();
        let systemd = Arc::new(FakeSystemd {
            units: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(Vec::new()),
            report: Mutex::new(RunReport {
                exit_code: Some(0),
                ..RunReport::default()
            }),
        });
        let brake = Arc::new(FakeBrake(AtomicBool::new(true)));
        let home = Arc::new(FakeHome::default());
        let (queue, orders) = mpsc::channel();
        let ctx = Ctx {
            store: OrderStore::new(&directory.path().join("orders")),
            state_store: directory.path().join("state").join("control.cc"),
            steward_uid: STEWARD,
            principal: "eureka".into(),
            systemd: systemd.clone(),
            peers: Arc::new(FakePeers(peer_uid)),
            brake: brake.clone(),
            homeserver: home.clone(),
        };
        Harness {
            directory,
            ctx,
            systemd,
            brake,
            home,
            queue,
            _orders: orders,
        }
    }

    fn harness() -> Harness {
        harness_for(STEWARD)
    }

    impl Harness {
        fn exchange(&self, write: impl FnOnce(&mut UnixStream)) -> Response {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            write(&mut client);
            handle(&self.ctx, &mut server, &self.queue).unwrap();
            let bytes = read_frame(&mut client, RESPONSE_FRAME_LIMIT).unwrap();
            rmp_serde::from_slice(&bytes).unwrap()
        }

        fn ask(&self, request: &Request) -> Response {
            self.exchange(|client| write_frame(client, request).unwrap())
        }

        fn issue(&self, operation: Operation) -> Response {
            self.ask(&Request::Order(OrderRequest {
                operation,
                reason: "probe".into(),
            }))
        }

        fn issue_unit(&self, verb: Verb, unit: &str) -> Response {
            self.issue(Operation::UnitAction {
                verb,
                unit: unit.into(),
            })
        }

        fn accept(&self, response: Response) -> String {
            match response {
                Response::Accepted { order_id } => order_id,
                other => panic!("expected an accepted order, got {other:?}"),
            }
        }

        fn outcome(&self, order_id: &str) -> Outcome {
            self.ctx
                .store
                .ledger()
                .unwrap()
                .outcome_of(order_id)
                .expect("the order has an outcome")
                .outcome
                .clone()
        }

        fn runs(&self) -> Vec<(String, Step)> {
            self.systemd.runs.lock().unwrap().clone()
        }

        /// Tell the fake `systemctl show` what it prints for `name`.
        fn describe_unit(&self, name: &str, text: String) {
            self.systemd.units.lock().unwrap().insert(name.into(), text);
        }

        fn describe(&self, name: &str, properties: &[(&str, &str)]) {
            self.describe_unit(name, show_text(name, properties));
        }
    }

    fn all_verbs() -> [Verb; 4] {
        [Verb::Start, Verb::Stop, Verb::Restart, Verb::Reload]
    }

    #[test]
    fn foreign_uid_is_refused_at_the_door() {
        let h = harness_for(STEWARD + 1);
        let response = h.issue_unit(Verb::Restart, "nginx.service");
        assert_eq!(
            response,
            Response::Refused(Refusal::new("peer", Code::Unauthorized))
        );
        let response = h.ask(&Request::List);
        assert_eq!(
            response,
            Response::Refused(Refusal::new("peer", Code::Unauthorized))
        );
        assert!(!h.ctx.store.path().exists(), "nothing is written for a foreign peer");
        assert!(h.runs().is_empty());
    }

    #[derive(Serialize)]
    struct ForgedBody {
        operation: Operation,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        order_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        requested_by: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    }

    #[derive(Serialize)]
    struct ForgedFrame {
        order: ForgedBody,
    }

    #[test]
    fn order_id_and_requester_are_minted_not_declared() {
        let h = harness();
        let forge = |order_id: Option<&str>, requested_by: Option<&str>, command: Option<&str>| {
            let frame = ForgedFrame {
                order: ForgedBody {
                    operation: Operation::UnitAction {
                        verb: Verb::Restart,
                        unit: "nginx.service".into(),
                    },
                    reason: "probe".into(),
                    order_id: order_id.map(str::to_owned),
                    requested_by: requested_by.map(str::to_owned),
                    command: command.map(str::to_owned),
                },
            };
            h.exchange(|client| write_frame(client, &frame).unwrap())
        };
        // The same frame without the extra field is a good order.
        let honest = h.accept(forge(None, None, None));
        for forged in [
            forge(Some("ord-declared"), None, None),
            forge(None, Some("steward:someone-else order x"), None),
            forge(None, None, Some("reboot-now")),
        ] {
            assert_eq!(
                forged,
                Response::Refused(Refusal::new("frame", Code::Malformed))
            );
        }
        let ledger = h.ctx.store.ledger().unwrap();
        assert_eq!(ledger.orders.len(), 1, "a forged frame leaves no record");
        let order = &ledger.orders[0];
        assert_eq!(order.order_id, honest);
        let uuid = order.order_id.strip_prefix("ord-").expect("ord- prefix");
        assert_eq!(Uuid::parse_str(uuid).unwrap().get_version_num(), 4);
        assert_eq!(order.issuer_unit, "mind-eureka-self.service");
        assert_eq!(order.issuer_invocation, "invocation-1");
        assert_eq!(order.principal, "eureka");
        assert_eq!(order.issuer_uid, STEWARD);
        assert_eq!(
            requester(&order.principal, &order.order_id),
            format!("steward:eureka order {}", order.order_id)
        );
    }

    #[test]
    fn package_names_outside_grammar_are_refused() {
        let h = harness();
        let bad = [
            "-y",
            "--allow-unauthenticated",
            "-",
            "a",
            "",
            "Foo",
            "a/b",
            "/tmp/x",
            "./x",
            "x.deb",
            "foo.deb",
            "foo bar",
            "foo;reboot",
            "foo$(id)",
            "foo`id`",
            "foo|bar",
            "foo&bar",
            "foo\nbar",
            "foo*",
            "foo?",
            "pkg:amd64",
            "pkg/stable",
            "libc6-",
            "foo=",
            "foo=1 2",
            "foo=1;2",
            "foo=1=2",
            "fo\u{ff0f}o",
            "f\u{43e}o",
        ];
        for name in bad {
            let response = h.issue(Operation::PackageInstall {
                packages: vec!["git-lfs".into(), name.into()],
            });
            assert!(matches!(response, Response::Refused(_)), "{name:?} was not refused");
        }
        let many: Vec<String> = (0..17).map(|n| format!("pkg-{n}x")).collect();
        assert_eq!(
            h.issue(Operation::PackageInstall { packages: many }),
            Response::Refused(Refusal::new("package", Code::TooMany))
        );
        assert_eq!(
            h.issue(Operation::PackageInstall { packages: vec![] }),
            Response::Refused(Refusal::new("package", Code::Empty))
        );
        assert!(!h.ctx.store.path().exists(), "no refusal left a record");

        let good = [
            "git-lfs",
            "matrix-synapse-py3",
            "libnss-systemd",
            "bubblewrap",
            "socat",
            "g++",
            "libstdc++6",
            "python3.11",
            "nginx=1.24.0-2ubuntu7",
            "foo=1:2.3-4+b1~rc1",
        ];
        let id = h.accept(h.issue(Operation::PackageInstall {
            packages: good.iter().map(|name| (*name).to_owned()).collect(),
        }));
        execute(&h.ctx, &id).unwrap();
        assert_eq!(h.outcome(&id), Outcome::Ran);
        let runs = h.runs();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].1.argv, ["apt-get", "update", "-q"]);
        // Each name selects exactly itself and apt may remove nothing.
        let mut install = vec!["apt-get".to_owned(), "install".into(), "-y".into(), "-q".into()];
        install.extend(["--no-remove".to_owned(), "--".into()]);
        install.extend(good.iter().map(|spec| match spec.split_once('=') {
            Some((name, version)) => format!("?exact-name({name})={version}"),
            None => format!("?exact-name({spec})"),
        }));
        assert_eq!(runs[1].1.argv, install);
        assert!(install.contains(&"?exact-name(libstdc++6)".to_owned()));
        assert!(install.contains(&"?exact-name(nginx)=1.24.0-2ubuntu7".to_owned()));
        // The last step fails unless every named package is installed.
        let mut present = vec!["dpkg".to_owned(), "-s".into(), "--".into()];
        present.extend(good.iter().map(|spec| spec.split('=').next().unwrap().to_owned()));
        assert_eq!(runs[2].1.argv, present);
        for (unit, step) in &runs {
            assert!(unit.starts_with("idunn-order-"));
            let apt = step.argv[0] == "apt-get";
            assert_eq!(
                step.environment,
                if apt {
                    vec![("DEBIAN_FRONTEND".to_owned(), "noninteractive".to_owned())]
                } else {
                    Vec::new()
                }
            );
        }
        assert_ne!(runs[0].0, runs[1].0, "each step is its own transient unit");
        assert_ne!(runs[1].0, runs[2].0, "each step is its own transient unit");

        // A package whose name apt would read as a regex or a conflict is
        // exactly itself and never a removal.
        for spec in ["cowsa.", "iptables-persistent", "a.", "nginx+"] {
            let argv = Operation::PackageInstall { packages: vec![spec.into()] }.steps()[1].argv.clone();
            assert_eq!(argv[4..], ["--no-remove", "--", &format!("?exact-name({spec})")]);
        }
        // A step that failed ends the order: the absent package is a failure.
        *h.systemd.report.lock().unwrap() = RunReport {
            exit_code: Some(1),
            ..RunReport::default()
        };
        let absent = h.accept(h.issue(Operation::PackageInstall { packages: vec!["cowsay".into()] }));
        execute(&h.ctx, &absent).unwrap();
        assert!(matches!(h.outcome(&absent), Outcome::Failed { exit_code: Some(1), .. }));
        assert_eq!(h.runs().len(), 4, "the update failed, so nothing else ran");
    }

    #[test]
    fn any_unit_outside_the_refused_set_runs() {
        let h = harness();
        let units = [
            "wg-quick@wg0.service",
            "nginx.service",
            "matrix-synapse.service",
            "ssh.socket",
            "huginn@eureka.service",
            "claude-connector.service",
            "persona-matrix-mouth.service",
            "mind-heartbeat.service",
            "apt-daily.timer",
            "multi-user.target",
            // Near misses: the prefix is the rule, not a substring or a stem.
            "midunn-x.service",
            "not-idunn-x.service",
            "idunn.service",
            "mind-self.service",
            "mind-selfx@a.service",
            "mind-persona.service",
            "mind-personal@a.service",
        ];
        for verb in all_verbs() {
            for unit in units {
                let id = h.accept(h.issue_unit(verb, unit));
                execute(&h.ctx, &id).unwrap();
                assert_eq!(h.outcome(&id), Outcome::Ran, "{verb:?} {unit}");
                let (transient, step) = h.runs().pop().unwrap();
                assert!(transient.starts_with("idunn-order-"));
                assert_eq!(step.argv, ["systemctl", verb.as_str(), "--", unit]);
            }
        }
        assert_eq!(h.runs().len(), units.len() * 4);
    }

    #[test]
    fn refused_set_is_refused_by_name_alias_and_propagation() {
        let h = harness();
        h.describe_unit(
            "backup-name.service",
            show_text(
                "idunn-yggdrasil.service",
                &[("Names", "idunn-yggdrasil.service backup-name.service")],
            ),
        );
        h.describe(
            "quiet-alias.service",
            &[("Names", "quiet-alias.service mind-persona@void.service")],
        );
        // fuse <- wrapper <- (bound by) mind-self@eureka
        h.describe("fuse.service", &[("RequiredBy", "wrapper.service")]);
        h.describe("wrapper.service", &[("BoundBy", "mind-self@eureka.service")]);
        h.describe("stack.target", &[("ConsistsOf", "mind-persona@void.service")]);
        // A cycle that reaches an alias of a refused unit on its second hop.
        h.describe("loop-a.service", &[("RequiredBy", "loop-b.service")]);
        h.describe("loop-b.service", &[("RequiredBy", "loop-a.service hidden.service")]);
        h.describe_unit(
            "hidden.service",
            show_text(
                "idunn-orders.service",
                &[("Names", "idunn-orders.service hidden.service")],
            ),
        );

        let direct = [
            "idunn-yggdrasil.service",
            "idunn-orders.service",
            "idunn-vili-x.service",
            "idunn-order-3f2c.service",
            "idunn-yggdrasil.timer",
            "idunn-x.socket",
            "idunn-.service",
            "idunn-yggdrasil.service.service",
            "idunn-x@y.service",
            "mind-self@eureka.service",
            "mind-persona@void.service",
            "mind-self@.service",
            "mind-persona@a@b.service",
            // Case folds: a lookalike is still refused, never run.
            "IDUNN-yggdrasil.service",
            "Idunn-Orders.service",
            "MIND-SELF@eureka.service",
            "Mind-Persona@Void.service",
        ];
        for verb in all_verbs() {
            for unit in direct {
                let id = h.accept(h.issue_unit(verb, unit));
                execute(&h.ctx, &id).unwrap();
                match h.outcome(&id) {
                    Outcome::Refused(Refused::AlwaysRefused { unit: hit, via }) => {
                        assert_eq!(hit, unit, "{verb:?} {unit}");
                        assert_eq!(via, Via::Name);
                    }
                    other => panic!("{verb:?} {unit} was {other:?}"),
                }
            }
        }
        for verb in all_verbs() {
            for (unit, hit) in [
                ("backup-name.service", "idunn-yggdrasil.service"),
                ("quiet-alias.service", "mind-persona@void.service"),
            ] {
                let id = h.accept(h.issue_unit(verb, unit));
                execute(&h.ctx, &id).unwrap();
                assert_eq!(
                    h.outcome(&id),
                    Outcome::Refused(Refused::AlwaysRefused {
                        unit: hit.into(),
                        via: Via::Alias
                    }),
                    "{verb:?} {unit}"
                );
            }
        }
        for verb in [Verb::Stop, Verb::Restart] {
            for (unit, hit) in [
                ("fuse.service", "mind-self@eureka.service"),
                ("stack.target", "mind-persona@void.service"),
                ("loop-a.service", "idunn-orders.service"),
            ] {
                let id = h.accept(h.issue_unit(verb, unit));
                execute(&h.ctx, &id).unwrap();
                assert_eq!(
                    h.outcome(&id),
                    Outcome::Refused(Refused::AlwaysRefused {
                        unit: hit.into(),
                        via: Via::Propagation
                    }),
                    "{verb:?} {unit}"
                );
            }
        }
        assert!(h.runs().is_empty(), "no systemd verb ran for any refused order");
        // start and reload take nothing down, so the same units are not refused.
        for verb in [Verb::Start, Verb::Reload] {
            let id = h.accept(h.issue_unit(verb, "fuse.service"));
            execute(&h.ctx, &id).unwrap();
            assert_eq!(h.outcome(&id), Outcome::Ran);
        }
        // Every refusal is recorded and has a result post due.
        let ledger = h.ctx.store.ledger().unwrap();
        let refusals = ledger
            .outcomes
            .iter()
            .filter(|record| matches!(record.outcome, Outcome::Refused(_)))
            .count();
        assert_eq!(refusals, direct.len() * 4 + 8 + 6);
        let due = due_posts(&h.ctx, &ledger);
        let results = due
            .iter()
            .filter(|due| due.kind == PostKind::Result && due.text.contains("always-refused"))
            .count();
        assert_eq!(results, refusals);
        // A refused order never started.
        assert!(ledger.starts.len() == 2);
    }

    /// Yggdrasil's `systemctl show` shapes (systemd 257, 2026-11-01), cut down
    /// to the units an order can reach: seven refused units all conflict with
    /// shutdown.target, and the shutdown services require it.
    fn yggdrasil(h: &Harness, idunn_running: bool) {
        let state = if idunn_running { "active" } else { "inactive" };
        let refused = [
            "idunn-yggdrasil.service",
            "idunn-orders.service",
            "idunn-vili-e0ada640.service",
            "mind-self@eureka.service",
            "mind-persona@void.service",
        ];
        h.describe(
            "shutdown.target",
            &[
                ("ActiveState", "inactive"),
                (
                    "RequiredBy",
                    "systemd-soft-reboot.service systemd-reboot.service systemd-poweroff.service",
                ),
                (
                    "ConflictedBy",
                    "nginx.service idunn-yggdrasil.service idunn-orders.service \
                     idunn-vili-e0ada640.service mind-self@eureka.service mind-persona@void.service",
                ),
            ],
        );
        for refused in refused {
            h.describe(
                refused,
                &[
                    ("Requires", "sysinit.target system.slice -.mount"),
                    ("Wants", "tmp.mount network-online.target"),
                    ("Conflicts", "shutdown.target"),
                    ("ActiveState", if refused == "idunn-yggdrasil.service" { state } else { "inactive" }),
                ],
            );
        }
        h.describe("nginx.service", &[("Wants", "network-online.target"), ("Conflicts", "shutdown.target")]);
        for (service, target) in [
            ("systemd-poweroff.service", "poweroff.target"),
            ("systemd-reboot.service", "reboot.target"),
            ("systemd-soft-reboot.service", "soft-reboot.target"),
        ] {
            h.describe(
                service,
                &[
                    ("Requires", "shutdown.target final.target system.slice umount.target"),
                    ("RequiredBy", target),
                    ("ActiveState", "inactive"),
                ],
            );
        }
        for (target, service) in [
            ("poweroff.target", "systemd-poweroff.service"),
            ("reboot.target", "systemd-reboot.service"),
            ("soft-reboot.target", "systemd-soft-reboot.service"),
        ] {
            h.describe(target, &[("Requires", service), ("ActiveState", "inactive")]);
        }
        h.describe(
            "umount.target",
            &[
                ("ActiveState", "inactive"),
                ("RequiredBy", "systemd-poweroff.service systemd-reboot.service"),
                ("ConflictedBy", "boot-efi.mount boot.mount \"blockdev@dev-vda2.target\""),
            ],
        );
        h.describe(
            "multi-user.target",
            &[
                ("Requires", "basic.target"),
                ("Wants", "cron.service ssh.service idunn-yggdrasil.service"),
                ("Conflicts", "shutdown.target rescue.target rescue.service"),
            ],
        );
        h.describe(
            "voidbot.service",
            &[
                ("Requires", "docker.service -.mount sysinit.target system.slice"),
                ("Wants", "idunn-yggdrasil.service network-online.target"),
                ("Conflicts", "shutdown.target"),
            ],
        );
    }

    fn outcome_of_unit(h: &Harness, verb: Verb, unit: &str) -> Outcome {
        let id = h.accept(h.issue_unit(verb, unit));
        execute(&h.ctx, &id).unwrap();
        h.outcome(&id)
    }

    fn hit(outcome: &Outcome) -> Option<&str> {
        match outcome {
            Outcome::Refused(Refused::AlwaysRefused { unit, via: Via::Propagation }) => Some(unit.as_str()),
            _ => None,
        }
    }

    #[test]
    fn a_transaction_that_reaches_a_refused_unit_is_refused() {
        let h = harness();
        yggdrasil(&h, true);
        // The targets and services that power the host down reach every refused
        // unit through Conflicts=, however many requirements away.
        for (verb, unit) in [
            (Verb::Start, "shutdown.target"),
            (Verb::Start, "poweroff.target"),
            (Verb::Restart, "poweroff.target"),
            (Verb::Start, "reboot.target"),
            (Verb::Restart, "reboot.target"),
            (Verb::Start, "soft-reboot.target"),
            (Verb::Start, "systemd-soft-reboot.service"),
            (Verb::Restart, "systemd-poweroff.service"),
        ] {
            let outcome = outcome_of_unit(&h, verb, unit);
            assert!(hit(&outcome).is_some(), "{verb:?} {unit} was {outcome:?}");
        }
        assert!(h.runs().is_empty(), "no refused order ran anything");
        // A stop of shutdown.target takes down what requires it and runs no start.
        let outcome = outcome_of_unit(&h, Verb::Stop, "shutdown.target");
        assert_eq!(outcome, Outcome::Ran);

        // A start that wants a refused unit that is stopped would start it.
        h.describe("idunn-yggdrasil.service", &[("ActiveState", "inactive"), ("Conflicts", "shutdown.target")]);
        for (verb, unit) in [
            (Verb::Start, "multi-user.target"),
            (Verb::Restart, "multi-user.target"),
            (Verb::Start, "voidbot.service"),
            (Verb::Restart, "voidbot.service"),
        ] {
            let outcome = outcome_of_unit(&h, verb, unit);
            assert_eq!(hit(&outcome), Some("idunn-yggdrasil.service"), "{verb:?} {unit}");
        }
        // The same orders start nothing while the refused unit runs.
        h.describe("idunn-yggdrasil.service", &[("ActiveState", "active"), ("Conflicts", "shutdown.target")]);
        for (verb, unit) in [
            (Verb::Start, "multi-user.target"),
            (Verb::Restart, "voidbot.service"),
            (Verb::Start, "voidbot.service"),
        ] {
            assert_eq!(outcome_of_unit(&h, verb, unit), Outcome::Ran, "{verb:?} {unit}");
        }
        // A unit that wants only what is not refused runs, and so does a target
        // that unmounts nothing a refused unit requires.
        for verb in [Verb::Start, Verb::Restart, Verb::Stop, Verb::Reload] {
            assert_eq!(outcome_of_unit(&h, verb, "nginx.service"), Outcome::Ran);
        }
        assert_eq!(outcome_of_unit(&h, Verb::Start, "umount.target"), Outcome::Ran);
    }

    #[test]
    fn the_closure_follows_every_edge_and_every_hop() {
        let h = harness();
        // Start: three hops of Wants/Requires/Requisite/BindsTo/Upholds to a
        // stopped refused unit.
        h.describe("hop-1.service", &[("Wants", "hop-2.service")]);
        h.describe("hop-2.service", &[("Requires", "hop-3.service")]);
        h.describe("hop-3.service", &[("Requisite", "hop-4.service")]);
        h.describe("hop-4.service", &[("BindsTo", "hop-5.service")]);
        h.describe("hop-5.service", &[("Upholds", "idunn-orders.service")]);
        h.describe("idunn-orders.service", &[("ActiveState", "inactive")]);
        let outcome = outcome_of_unit(&h, Verb::Start, "hop-1.service");
        assert_eq!(hit(&outcome), Some("idunn-orders.service"));
        // Conflicts of a pulled-in unit stop it and what requires it: the
        // conflict, then two hops of reverse requirement, to a refused unit.
        h.describe("rival.service", &[("Conflicts", "victim.service")]);
        h.describe("victim.service", &[("RequiredBy", "mid.service")]);
        h.describe("mid.service", &[("BoundBy", "mind-self@eureka.service")]);
        h.describe("pulls-rival.service", &[("Requires", "rival.service")]);
        for unit in ["rival.service", "pulls-rival.service"] {
            let outcome = outcome_of_unit(&h, Verb::Start, unit);
            assert_eq!(hit(&outcome), Some("mind-self@eureka.service"), "{unit}");
        }
        // The reverse Conflicts edge: a unit conflicted by a refused one.
        h.describe("incumbent.service", &[("ConflictedBy", "mind-persona@void.service")]);
        let outcome = outcome_of_unit(&h, Verb::Start, "incumbent.service");
        assert_eq!(hit(&outcome), Some("mind-persona@void.service"));
        // Stop and restart: every stop edge, one at a time.
        for (property, reached) in [
            ("RequiredBy", "idunn-vili-x.service"),
            ("BoundBy", "idunn-vili-x.service"),
            ("RequisiteOf", "idunn-vili-x.service"),
            ("ConsistsOf", "idunn-vili-x.service"),
            ("PropagatesStopTo", "mind-persona@void.service"),
            ("StopPropagatedFrom", "mind-persona@void.service"),
        ] {
            let name = format!("edge-{property}.service");
            h.describe(&name, &[(property, reached)]);
            for verb in [Verb::Stop, Verb::Restart] {
                let outcome = outcome_of_unit(&h, verb, &name);
                assert_eq!(hit(&outcome), Some(reached), "{verb:?} {property}");
            }
        }
        // A restart starts what its stopped dependents pull in.
        h.describe("base.service", &[("RequiredBy", "dependent.service")]);
        h.describe("dependent.service", &[("Wants", "idunn-orders.service")]);
        let outcome = outcome_of_unit(&h, Verb::Restart, "base.service");
        assert_eq!(hit(&outcome), Some("idunn-orders.service"));
        assert_eq!(outcome_of_unit(&h, Verb::Stop, "base.service"), Outcome::Ran);
        assert_eq!(outcome_of_unit(&h, Verb::Reload, "base.service"), Outcome::Ran);
    }

    #[test]
    fn a_quoted_name_in_a_dependency_list_is_the_name_it_quotes() {
        let h = harness();
        // systemctl show quotes a name with a backslash and doubles the backslash.
        h.describe(
            "huginn.service",
            &[("RequiredBy", "\"mind-self@foo\\\\x2dbar.service\"")],
        );
        let outcome = outcome_of_unit(&h, Verb::Stop, "huginn.service");
        assert_eq!(hit(&outcome), Some("mind-self@foo\\x2dbar.service"));
        h.describe("quiet.service", &[("Wants", "\"idunn-a\\\\x2db.service\" other.service")]);
        h.describe("idunn-a\\x2db.service", &[("ActiveState", "inactive")]);
        let outcome = outcome_of_unit(&h, Verb::Start, "quiet.service");
        assert_eq!(hit(&outcome), Some("idunn-a\\x2db.service"));
        assert!(h.runs().is_empty());
    }

    #[test]
    fn a_unit_systemd_cannot_show_is_a_failure_not_a_leaf() {
        let h = harness();
        h.describe("ghost.service", &[("LoadState", "not-found")]);
        h.describe("pulls-ghost.service", &[("Wants", "ghost.service")]);
        h.describe("needed-by-ghost.service", &[("RequiredBy", "ghost.service")]);
        for (verb, unit) in [
            (Verb::Start, "ghost.service"),
            (Verb::Start, "pulls-ghost.service"),
            (Verb::Stop, "needed-by-ghost.service"),
            (Verb::Restart, "needed-by-ghost.service"),
        ] {
            let id = h.accept(h.issue_unit(verb, unit));
            execute(&h.ctx, &id).unwrap();
            assert!(
                matches!(h.outcome(&id), Outcome::Failed { exit_code: None, signal: None }),
                "{verb:?} {unit}"
            );
        }
        assert!(h.runs().is_empty());
    }

    #[test]
    fn globs_slices_and_scopes_are_not_units() {
        let h = harness();
        let bad = [
            "idunn*",
            "idunn-*.service",
            "idunn-?ggdrasil.service",
            "mind-?elf@eureka.service",
            "mind-self@*.service",
            "mind-[s]elf@eureka.service",
            "system.slice",
            "user.slice",
            "session-1.scope",
            "init.scope",
            "idunn-yggdrasil",
            "mind-self@eureka",
            "nginx",
            ".service",
            "",
            "-idunn-x.service",
            "-x.service",
            "/etc/systemd/system/idunn-yggdrasil.service",
            "../idunn-yggdrasil.service",
            "idunn-yggdrasil.service\n",
            "idunn-yggdrasil.service ",
            " idunn-yggdrasil.service",
            "idunn-yggdrasil.service\0",
            "idunn-yggdrasil.service;reboot",
            "idunn\\x2dyggdrasil.service",
            "idunn%2dyggdrasil.service",
            // Unicode lookalikes of a refused name.
            "\u{131}dunn-yggdrasil.service",
            "idunn\u{2010}yggdrasil.service",
            "idunn\u{ff0d}yggdrasil.service",
            "idunn-yggdrasil.service\u{200b}",
            "\u{200b}idunn-yggdrasil.service",
            "idunn-yggdrasil\u{2024}service",
            "mind-self\u{ff20}eureka.service",
        ];
        for unit in bad {
            for verb in all_verbs() {
                assert!(
                    matches!(h.issue_unit(verb, unit), Response::Refused(_)),
                    "{unit:?} was not refused by the grammar"
                );
            }
        }
        let long = format!("{}.service", "a".repeat(UNIT_LIMIT));
        assert_eq!(
            h.issue_unit(Verb::Start, &long),
            Response::Refused(Refusal::new("unit", Code::TooLong))
        );
        assert!(!h.ctx.store.path().exists(), "the grammar refuses before any record");
    }

    #[test]
    fn binding_with_a_unit_list_is_refused() {
        let valid = "principal = \"eureka\"\nuser = \"mind-eureka-self\"\n\
                     mind_control = \"/usr/local/bin/mind-control\"\n\
                     room = \"!abc:example.org\"\nhomeserver = \"http://127.0.0.1:8008\"\n\
                     token_credential = \"idunn-matrix-token\"\n";
        let binding = StewardBinding::parse(valid).unwrap();
        assert_eq!(binding.principal, "eureka");
        assert_eq!(binding.address().unwrap().port(), 8008);
        for extra in [
            "units = [\"nginx.service\"]",
            "allow = [\"nginx.service\"]",
            "deny = [\"idunn-*\"]",
            "denylist = [\"idunn-*\"]",
            "refused = [\"idunn-*\"]",
            "unit = \"nginx.service\"",
            "[units]\nnginx = true",
        ] {
            assert!(
                StewardBinding::parse(&format!("{valid}{extra}\n")).is_err(),
                "{extra} loaded"
            );
        }
        let swap = |from: &str, to: &str| valid.replace(from, to);
        for broken in [
            swap("user = \"mind-eureka-self\"", "user = \"root\""),
            swap("user = \"mind-eureka-self\"", "user = \"\""),
            swap("/usr/local/bin/mind-control", "mind-control"),
            swap("!abc:example.org", "#alias:example.org"),
            swap("http://127.0.0.1:8008", "http://10.77.0.1:8008"),
            swap("http://127.0.0.1:8008", "https://127.0.0.1:8008"),
            swap("http://127.0.0.1:8008", "http://127.0.0.1:8008/path"),
            swap("idunn-matrix-token", "../token"),
            swap("principal = \"eureka\"\n", ""),
        ] {
            assert!(StewardBinding::parse(&broken).is_err());
        }
        assert!(admitted(0, 0o100600));
        assert!(admitted(0, 0o100644));
        assert!(!admitted(0, 0o100664));
        assert!(!admitted(0, 0o100666));
        assert!(!admitted(0, 0o100602));
        assert!(!admitted(1000, 0o100600));
    }

    #[test]
    fn held_brake_refuses_at_start() {
        let h = harness();
        h.brake.0.store(false, Ordering::SeqCst);
        let id = h.accept(h.issue_unit(Verb::Restart, "nginx.service"));
        execute(&h.ctx, &id).unwrap();
        assert_eq!(h.outcome(&id), Outcome::Refused(Refused::Braked));
        assert!(h.ctx.store.ledger().unwrap().start_of(&id).is_none());
        assert!(h.runs().is_empty());
        // The deploy path is gated the same way and writes no command.
        let deploy = h.accept(h.issue(Operation::Deploy {
            selector: "vili".into(),
        }));
        execute(&h.ctx, &deploy).unwrap();
        assert_eq!(h.outcome(&deploy), Outcome::Refused(Refused::Braked));
        assert!(!h.ctx.state_store.exists());
        // A brake that cannot be asked is a held brake.
        let directory = TempDir::new().unwrap();
        assert!(!MindControl { program: directory.path().join("absent") }.clear("eureka"));
    }

    #[test]
    fn brake_check_is_the_exit_status_of_mind_control_check() {
        let directory = TempDir::new().unwrap();
        let program = |name: &str, body: &str| {
            let path = directory.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            MindControl { program: path }
        };
        let recorder = directory.path().join("argv");
        let clear = program(
            "clear",
            &format!("#!/bin/sh\necho \"$@\" > {}\nexit 0\n", recorder.display()),
        );
        assert!(clear.clear("eureka"));
        assert_eq!(
            std::fs::read_to_string(&recorder).unwrap().trim(),
            "--instance eureka check"
        );
        assert!(!program("held", "#!/bin/sh\nexit 1\n").clear("eureka"));
    }

    #[test]
    fn started_order_is_never_rerun() {
        let h = harness();
        let id = h.accept(h.issue_unit(Verb::Restart, "nginx.service"));
        let waiting = h.accept(h.issue_unit(Verb::Restart, "ssh.socket"));
        // The first order began and its service died: a start with no outcome.
        h.ctx
            .store
            .put(
                &id,
                START_SCHEMA,
                &StartRecord {
                    schema_version: START_SCHEMA.into(),
                    order_id: id.clone(),
                    started_at_unix_millis: 1,
                    actuation: format!("idunn-order-{}", order_uuid(&id)),
                },
            )
            .unwrap();
        let pending = recover(&h.ctx).unwrap();
        assert_eq!(h.outcome(&id), Outcome::Lost);
        assert_eq!(pending, std::slice::from_ref(&waiting), "an order that never started waits");
        execute(&h.ctx, &id).unwrap();
        assert!(h.runs().is_empty(), "a Lost order makes no systemd call");
        // Recovery is idempotent, and a finished order is not run again.
        assert_eq!(recover(&h.ctx).unwrap(), std::slice::from_ref(&waiting));
        execute(&h.ctx, &waiting).unwrap();
        execute(&h.ctx, &waiting).unwrap();
        assert_eq!(h.runs().len(), 1);
        assert!(recover(&h.ctx).unwrap().is_empty());
    }

    #[test]
    fn deploy_order_uses_the_up_primitive() {
        let h = harness();
        let id = h.accept(h.issue(Operation::Deploy {
            selector: "vili".into(),
        }));
        execute(&h.ctx, &id).unwrap();
        let command_id = format!("up-{}", order_uuid(&id));
        assert_eq!(
            h.outcome(&id),
            Outcome::Submitted {
                command_id: command_id.clone()
            }
        );
        let ledger = h.ctx.store.ledger().unwrap();
        assert_eq!(ledger.start_of(&id).unwrap().actuation, command_id);
        assert!(h.runs().is_empty(), "a deploy runs no transient unit");
        let mut shown = Vec::new();
        crate::control_plane::status(
            &mut shown,
            &h.ctx.state_store,
            &h.directory.path().join("incidents.cc"),
            Some(&command_id),
        )
        .unwrap();
        let shown = String::from_utf8(shown).unwrap();
        assert_eq!(shown.lines().count(), 1, "exactly one command: {shown}");
        assert!(shown.starts_with(&format!("{command_id} vili queued")), "{shown}");
        // Re-executing writes no second command.
        execute(&h.ctx, &id).unwrap();
        let mut again = Vec::new();
        crate::control_plane::status(
            &mut again,
            &h.ctx.state_store,
            &h.directory.path().join("incidents.cc"),
            None,
        )
        .unwrap();
        assert_eq!(String::from_utf8(again).unwrap().lines().count(), 1);
        // The result post waits for the transactions: queued is not finished.
        let due = due_posts(&h.ctx, &ledger);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].kind, PostKind::Accepted);
        assert!(!deploy_finished("queued") && !deploy_finished("running"));
        assert!(deploy_finished("succeeded") && deploy_finished("failed"));
        assert!(deploy_finished("complete"));
    }

    #[test]
    fn chat_outage_delays_posts_not_orders() {
        let h = harness();
        h.home.failing.store(true, Ordering::SeqCst);
        let id = h.accept(h.issue_unit(Verb::Restart, "mind-heartbeat.service"));
        execute(&h.ctx, &id).unwrap();
        assert_eq!(h.outcome(&id), Outcome::Ran);
        assert_eq!(h.runs().len(), 1);
        assert!(post_pass(&h.ctx).is_err());
        assert!(h.ctx.store.ledger().unwrap().posts.is_empty());
        h.home.failing.store(false, Ordering::SeqCst);
        assert_eq!(post_pass(&h.ctx).unwrap(), 2);
        assert_eq!(post_pass(&h.ctx).unwrap(), 0, "each post goes out once");
        let sent = h.home.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, format!("idunn-{id}-accepted"));
        assert_eq!(sent[1].0, format!("idunn-{id}-result"));
        assert!(sent[0].1.contains("unit restart mind-heartbeat.service"));
        assert!(sent[0].1.contains("mind-eureka-self.service"));
        assert!(sent[0].1.contains("reason: \"probe\""));
        assert!(sent[1].1.contains("ran (exit 0)"));
    }

    #[test]
    fn failed_runs_keep_bounded_tails_that_never_leave_the_store() {
        let h = harness();
        *h.systemd.report.lock().unwrap() = RunReport {
            exit_code: Some(100),
            signal: None,
            stdout_tail: vec![b'o'; 3 * TAIL_BYTES],
            stderr_tail: b"E: secret-looking \x1b[31m\xe2\x80\xae line\n".to_vec(),
        };
        let id = h.accept(h.issue(Operation::PackageInstall {
            packages: vec!["git-lfs".into()],
        }));
        execute(&h.ctx, &id).unwrap();
        assert_eq!(
            h.outcome(&id),
            Outcome::Failed {
                exit_code: Some(100),
                signal: None
            }
        );
        assert_eq!(h.runs().len(), 1, "the install did not follow a failed update");
        let ledger = h.ctx.store.ledger().unwrap();
        let record = ledger.outcome_of(&id).unwrap();
        assert_eq!(record.stdout_tail.len(), TAIL_BYTES);
        assert!(!format!("{record:?}").contains("secret-looking"));
        // Neither the post nor the journal text carries a tail.
        let results: Vec<_> = due_posts(&h.ctx, &ledger)
            .into_iter()
            .filter(|due| due.kind == PostKind::Result)
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].text.contains("failed (exit 100)"));
        assert!(!results[0].text.contains("secret-looking"));
        // The steward reads them through the escape sink and nothing raw.
        let Response::Show(view) = h.ask(&Request::Show(ShowRequest { id: id.clone() })) else {
            panic!("expected a show")
        };
        let mut shown = Vec::new();
        emit(&mut shown, &render_view(&view)).unwrap();
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.contains("secret-looking"));
        assert!(!shown.contains('\u{1b}') && !shown.contains('\u{202e}'));
        assert!(shown.contains("\\u{1b}") && shown.contains("\\u{202e}"));
        assert!(shown.contains("status failed"));
        // A show of an unknown or malformed id names the field only.
        assert_eq!(
            h.ask(&Request::Show(ShowRequest {
                id: format!("ord-{}", Uuid::new_v4())
            })),
            Response::Refused(Refusal::new("id", Code::Unknown))
        );
        assert_eq!(
            h.ask(&Request::Show(ShowRequest { id: "ord-../x".into() })),
            Response::Refused(Refusal::new("id", Code::BadShape))
        );
        let Response::List(lines) = h.ask(&Request::List) else {
            panic!("expected a list")
        };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].status, "failed");
    }

    #[test]
    fn a_refusal_or_error_never_echoes_an_input_value() {
        let h = harness();
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut note = |response: Response| {
            let bytes = rmp_serde::to_vec_named(&response).unwrap();
            if let Response::Refused(refusal) = &response {
                seen.push(refusal.to_string().into_bytes());
            }
            seen.push(bytes);
        };
        for unit in [
            format!("{CANARY}*.service"),
            format!("{CANARY}/../idunn-orders.service"),
            format!("idunn-{CANARY};x.service"),
            CANARY.to_string(),
        ] {
            note(h.issue_unit(Verb::Restart, &unit));
        }
        note(h.issue(Operation::PackageInstall {
            packages: vec![format!("{CANARY}/x")],
        }));
        note(h.issue(Operation::PackageInstall {
            packages: vec![format!("pkg={CANARY} 1")],
        }));
        note(h.issue(Operation::Deploy {
            selector: format!("{CANARY} bad"),
        }));
        note(h.ask(&Request::Order(OrderRequest {
            operation: Operation::UnitAction {
                verb: Verb::Start,
                unit: "nginx.service".into(),
            },
            reason: format!("{CANARY}\n"),
        })));
        note(h.ask(&Request::Order(OrderRequest {
            operation: Operation::UnitAction {
                verb: Verb::Start,
                unit: "nginx.service".into(),
            },
            reason: CANARY.repeat(40),
        })));
        note(h.ask(&Request::Show(ShowRequest {
            id: format!("ord-{CANARY}"),
        })));
        // A frame whose own keys and values are the canary, and plain garbage.
        let named: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> =
            [(
                CANARY.to_owned(),
                [(CANARY.to_owned(), CANARY.to_owned())].into_iter().collect(),
            )]
            .into_iter()
            .collect();
        note(h.exchange(|client| write_frame(client, &named).unwrap()));
        note(h.exchange(|client| {
            let garbage = format!("{CANARY}{CANARY}");
            client
                .write_all(&(garbage.len() as u32).to_be_bytes())
                .unwrap();
            client.write_all(garbage.as_bytes()).unwrap();
        }));
        note(h.exchange(|client| {
            client.write_all(&u32::MAX.to_be_bytes()).unwrap();
        }));
        // The client grammar fails the same way.
        let mut messages = Vec::new();
        let words = |line: &[&str]| line.iter().map(|word| (*word).to_owned()).collect::<Vec<_>>();
        for line in [
            words(&["unit", "restart", &format!("{CANARY};x.service"), "--reason", "r"]),
            words(&["unit", CANARY, "nginx.service", "--reason", "r"]),
            words(&["unit", "restart", "nginx.service", &format!("--{CANARY}")]),
            words(&["unit", "restart", "nginx.service", "--reason", &format!("{CANARY}\n")]),
            words(&[CANARY]),
            words(&["package-install", &format!("{CANARY}/x"), "--reason", "r"]),
            words(&["deploy", &format!("{CANARY} x"), "--reason", "r"]),
            words(&["show"]),
        ] {
            messages.push(format!("{:#}", parse_order(line.into_iter()).unwrap_err()));
        }
        for line in [
            words(&[CANARY]),
            words(&["serve", &format!("--{CANARY}")]),
            words(&["serve", "--store"]),
        ] {
            messages.push(format!("{:#}", parse_serve(line.into_iter()).unwrap_err()));
        }
        for bytes in &seen {
            assert!(
                !String::from_utf8_lossy(bytes).contains(CANARY),
                "a response carried an input value"
            );
        }
        for message in &messages {
            assert!(!message.contains(CANARY), "an error carried an input value: {message}");
        }
        assert!(!h.ctx.store.path().exists());
    }

    #[test]
    fn cgroup_names_the_issuer_unit() {
        assert_eq!(
            unit_from_cgroup("0::/system.slice/claude-connector.service\n"),
            "claude-connector.service"
        );
        assert_eq!(
            unit_from_cgroup("0::/system.slice/huginn@eureka.service/sub/leaf\n"),
            "huginn@eureka.service"
        );
        assert_eq!(
            unit_from_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n"),
            "session-3.scope"
        );
        assert_eq!(
            unit_from_cgroup("12:cpu:/x\n1:name=systemd:/system.slice/a.service\n"),
            "a.service"
        );
        assert_eq!(unit_from_cgroup("0::/\n"), "unknown");
        assert_eq!(unit_from_cgroup(""), "unknown");
    }

    #[test]
    fn systemctl_show_output_is_read_into_facts() {
        let facts = parse_show(
            "Id=a.service\nNames=a.service b.service\nLoadState=loaded\nActiveState=inactive\n\
             Requires=r.service \"q\\\\x2dr.slice\" -.mount\nRequisite=t.service\nWants=w.service\n\
             BindsTo=b.service\nUpholds=u.service\nConflicts=x.target\nConflictedBy=y.service\n\
             RequiredBy=c.service\nBoundBy=\nRequisiteOf=ro.service\nConsistsOf=d.target e.service\n\
             PropagatesStopTo=p.service\nStopPropagatedFrom=f.service\nOther=x\n",
        )
        .unwrap();
        assert_eq!(facts.id, "a.service");
        assert_eq!(facts.names, ["a.service", "b.service"]);
        assert!(!facts.not_found && !facts.running);
        // A quoted name holds one backslash, not two and not a quote mark.
        assert_eq!(
            facts.pulls,
            ["r.service", "q\\x2dr.slice", "-.mount", "t.service", "w.service", "b.service", "u.service"]
        );
        assert_eq!(facts.conflicts, ["x.target", "y.service"]);
        assert_eq!(
            facts.stops,
            ["c.service", "ro.service", "d.target", "e.service", "p.service", "f.service"]
        );
        assert!(parse_show("Names=a.service\n").is_err());
        // The request asks for exactly the properties the parse reads.
        assert_eq!(
            show_properties(),
            "Id,Names,LoadState,ActiveState,Requires,Requisite,BindsTo,Wants,Upholds,\
             Conflicts,ConflictedBy,RequiredBy,BoundBy,RequisiteOf,ConsistsOf,\
             PropagatesStopTo,StopPropagatedFrom"
        );
        // A backslash outside quotes is a backslash; inside, it escapes one character.
        assert_eq!(parse_names("a\\b c\\\\d"), ["a\\b", "c\\\\d"]);
        assert_eq!(parse_names("\"a\\\"b\" \"\" c"), ["a\"b", "", "c"]);
        assert!(parse_names("   ").is_empty());
        let gone = parse_show("Id=\\x22a.service\\x22.service\nLoadState=not-found\n").unwrap();
        assert!(gone.not_found);
        for (state, running) in [
            ("active", true),
            ("activating", true),
            ("reloading", true),
            ("inactive", false),
            ("deactivating", false),
            ("failed", false),
        ] {
            let facts = parse_show(&format!("Id=a.service\nActiveState={state}\n")).unwrap();
            assert_eq!(facts.running, running, "{state}");
        }
    }

    #[test]
    fn the_store_refuses_a_second_record_and_a_foreign_document() {
        let h = harness();
        let order = OrderRecord {
            schema_version: ORDER_SCHEMA.into(),
            order_id: "ord-x".into(),
            principal: "eureka".into(),
            issuer_uid: 1,
            issuer_unit: "u.service".into(),
            issuer_invocation: String::new(),
            issued_at_unix_millis: 1,
            operation: Operation::Deploy {
                selector: "vili".into(),
            },
            reason: "r".into(),
        };
        assert!(h.ctx.store.put("ord-x", ORDER_SCHEMA, &order).unwrap());
        assert!(!h.ctx.store.put("ord-x", ORDER_SCHEMA, &order).unwrap());
        assert_eq!(
            std::fs::metadata(h.ctx.store.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let foreign = typed_envelope("k", "idunn.other", "idunn.other.v1", &1u8, 1).unwrap();
        SingleFileMessagePackBackingStore::new(h.ctx.store.path())
            .compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: "idunn.other".into(),
                    key: "k".into(),
                    current: None,
                }],
                &[foreign],
            )
            .unwrap();
        assert!(h.ctx.store.ledger().is_err());
    }

    #[test]
    fn synapse_post_is_one_idempotent_put_on_loopback() {
        let directory = TempDir::new().unwrap();
        let token_file = directory.path().join("token");
        std::fs::write(&token_file, "tok-123\n").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, requests) = mpsc::channel();
        let server = thread::spawn(move || {
            for reply in [
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"event_id\":\"$ev1\"}",
                "HTTP/1.0 403 Forbidden\r\n\r\n{\"errcode\":\"M_FORBIDDEN\"}",
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut received = Vec::new();
                let mut chunk = [0u8; 1024];
                while !received.windows(4).any(|window| window == b"\r\n\r\n")
                    || !String::from_utf8_lossy(&received).contains("\"body\"")
                {
                    let count = socket.read(&mut chunk).unwrap();
                    if count == 0 {
                        break;
                    }
                    received.extend_from_slice(&chunk[..count]);
                }
                sent.send(String::from_utf8_lossy(&received).into_owned()).unwrap();
                socket.write_all(reply.as_bytes()).unwrap();
            }
        });
        let synapse = Synapse {
            address,
            room: "!room:example.org".into(),
            token_file,
        };
        assert_eq!(synapse.post("txn-1", "hello \"quoted\"").unwrap(), "$ev1");
        let request = requests.recv().unwrap();
        assert!(request.starts_with(
            "PUT /_matrix/client/v3/rooms/%21room%3Aexample.org/send/m.room.message/txn-1 HTTP/1.0\r\n"
        ));
        assert!(request.contains("Authorization: Bearer tok-123\r\n"));
        assert!(request.contains("{\"body\":\"hello \\\"quoted\\\"\",\"msgtype\":\"m.text\"}"));
        let error = synapse.post("txn-2", "x").unwrap_err();
        assert!(!format!("{error:#}").contains("tok-123"));
        server.join().unwrap();
        assert!(
            Synapse {
                address,
                room: "!r:e".into(),
                token_file: directory.path().join("absent")
            }
            .post("t", "x")
            .is_err()
        );
    }

    #[test]
    fn grammar_limits_are_exact_at_their_edges() {
        let h = harness();
        let order = |operation: Operation, reason: &str| {
            h.ask(&Request::Order(OrderRequest {
                operation,
                reason: reason.into(),
            }))
        };
        let packages = |names: Vec<String>| Operation::PackageInstall { packages: names };
        let unit = |name: String| Operation::UnitAction {
            verb: Verb::Start,
            unit: name,
        };
        // Package: two characters is the shortest name, 200 bytes the longest.
        assert!(matches!(order(packages(vec!["ab".into()]), "r"), Response::Accepted { .. }));
        assert!(matches!(order(packages(vec!["a".repeat(200)]), "r"), Response::Accepted { .. }));
        assert_eq!(
            order(packages(vec!["a".repeat(201)]), "r"),
            Response::Refused(Refusal::new("package", Code::TooLong))
        );
        assert_eq!(
            order(packages(vec!["a".into()]), "r"),
            Response::Refused(Refusal::new("package", Code::BadShape))
        );
        let sixteen: Vec<String> = (0..16).map(|n| format!("pkg-{n}x")).collect();
        assert!(matches!(order(packages(sixteen), "r"), Response::Accepted { .. }));
        // Unit: 200 bytes with its suffix is the longest.
        let longest = format!("{}.service", "a".repeat(UNIT_LIMIT - ".service".len()));
        assert_eq!(longest.len(), UNIT_LIMIT);
        assert!(matches!(order(unit(longest), "r"), Response::Accepted { .. }));
        let too_long = format!("{}.service", "a".repeat(UNIT_LIMIT - ".service".len() + 1));
        assert_eq!(
            order(unit(too_long), "r"),
            Response::Refused(Refusal::new("unit", Code::TooLong))
        );
        // Reason: 300 bytes is the longest, and it must be there.
        assert!(matches!(
            order(unit("nginx.service".into()), &"r".repeat(REASON_LIMIT)),
            Response::Accepted { .. }
        ));
        assert_eq!(
            order(unit("nginx.service".into()), &"r".repeat(REASON_LIMIT + 1)),
            Response::Refused(Refusal::new("reason", Code::TooLong))
        );
        assert_eq!(
            order(unit("nginx.service".into()), ""),
            Response::Refused(Refusal::new("reason", Code::Empty))
        );
        assert_eq!(
            order(unit("nginx.service".into()), "caf\u{e9}"),
            Response::Refused(Refusal::new("reason", Code::BadCharacter))
        );
    }

    #[test]
    fn a_concluded_order_is_never_run_later() {
        let h = harness();
        h.brake.0.store(false, Ordering::SeqCst);
        let id = h.accept(h.issue_unit(Verb::Restart, "nginx.service"));
        execute(&h.ctx, &id).unwrap();
        assert_eq!(h.outcome(&id), Outcome::Refused(Refused::Braked));
        // Someone else clears the brake; the refused order stays refused.
        h.brake.0.store(true, Ordering::SeqCst);
        execute(&h.ctx, &id).unwrap();
        assert!(h.runs().is_empty());
        let ledger = h.ctx.store.ledger().unwrap();
        assert!(ledger.start_of(&id).is_none());
        assert_eq!(h.outcome(&id), Outcome::Refused(Refused::Braked));
    }

    #[test]
    fn the_orders_cli_has_no_command_string() {
        let parse = |line: &[&str]| parse_order(line.iter().map(|word| (*word).to_owned()));
        let parsed = parse(&["unit", "restart", "nginx.service", "--reason", "why"]).unwrap();
        assert_eq!(
            parsed.action,
            OrderAction::Issue {
                operation: Operation::UnitAction {
                    verb: Verb::Restart,
                    unit: "nginx.service".into()
                },
                reason: "why".into()
            }
        );
        assert!(parse(&["package-install", "git-lfs", "socat", "--reason", "r"]).is_ok());
        assert!(parse(&["deploy", "profile:aetheria", "--reason", "r"]).is_ok());
        assert!(parse(&["show", "ord-x"]).is_ok());
        assert!(parse(&["list"]).is_ok());
        for refused in [
            vec!["unit", "restart", "nginx.service", "--reason", "r", "--command", "reboot-now"],
            vec!["unit", "restart", "nginx.service", "--reason", "r", "--shell"],
            vec!["package-install", "git-lfs", "--reason", "r", "--command", "x"],
            vec!["run", "sh", "--reason", "r"],
            vec!["exec", "--reason", "r"],
            vec!["unit", "enable", "nginx.service", "--reason", "r"],
            vec!["unit", "restart", "--reason", "r"],
            vec!["unit", "restart", "a.service", "b.service", "--reason", "r"],
            vec!["unit", "restart", "nginx.service"],
            vec!["unit", "restart", "nginx*", "--reason", "r"],
            vec!["package-install", "--reason", "r"],
            vec!["package-install", "-y", "x", "--reason", "r"],
            vec!["deploy", "--reason", "r"],
            vec!["deploy", "a", "b", "--reason", "r"],
            vec!["list", "extra"],
            vec!["show"],
            vec![],
        ] {
            assert!(parse(&refused).is_err(), "{refused:?} parsed");
        }
    }
}
