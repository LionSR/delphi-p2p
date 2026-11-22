//! CLI command definitions and handlers.
//!
//! `Command` is parsed by clap and also serialized over the Unix control socket when forwarding
//! requests to a running `delphi-p2p serve` process.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::check::{self, Runner};
use crate::claim::{self, Verdict};
use crate::files;
use crate::home::Origin;
use crate::index::Stored;
use crate::node::Node;

#[derive(Clone, Debug, Subcommand, Serialize, Deserialize)]
pub enum Command {
    /// Publish a directory as a new record or as a new version of an existing one.
    Publish {
        dir: PathBuf,
        /// Record title (required for a new record; inherited from the parent version otherwise).
        #[arg(long)]
        title: Option<String>,
        /// Author name (repeatable).
        #[arg(long = "author")]
        authors: Vec<String>,
        /// Source URL (for example, a paper or delphi.science record URL).
        #[arg(long)]
        source: Option<String>,
        /// Summary of changes in this version.
        #[arg(long)]
        note: Option<String>,
        /// Parent version ID. Defaults to the version recorded in `.delphi/origin.json`.
        #[arg(long)]
        after: Option<String>,
        /// Relative path inside the directory to a `lake comparator` config file.
        /// Defaults to `comparator.json` if present.
        #[arg(long)]
        comparator: Option<String>,
        /// Agent name recorded in the published claim.
        #[arg(long)]
        agent: Option<String>,
        /// Seconds to wait for another peer to mirror the files before exiting (0 disables waiting).
        #[arg(long, default_value_t = 120)]
        wait: u64,
    },
    /// Download a record's files into a local directory.
    Get {
        /// Record ID, version ID, or unique prefix. Resolves to the original author's latest version.
        id: String,
        /// Target directory. Defaults to a new directory named after the ID prefix.
        #[arg(long)]
        files: Option<PathBuf>,
        /// Download the exact specified version instead of resolving to the latest version.
        #[arg(long)]
        exact: bool,
        /// Seconds to wait for the claim and a peer holding the files.
        #[arg(long, default_value_t = 30)]
        wait: u64,
        #[arg(long)]
        agent: Option<String>,
    },
    /// List files in a version and diff them against its parent without downloading file contents.
    Files {
        /// Record ID, version ID, or unique prefix. Lists the original author's latest version.
        id: String,
        /// List the exact specified version instead of resolving to the latest version.
        #[arg(long)]
        exact: bool,
        /// Seconds to wait for the claim and a peer holding the files.
        #[arg(long, default_value_t = 30)]
        wait: u64,
        #[arg(long)]
        json: bool,
    },
    /// Run the Lean checker on a version in a sandbox and sign the verdict.
    Check {
        id: String,
        /// Check the exact specified version instead of resolving to the latest version.
        #[arg(long)]
        exact: bool,
        #[arg(long, value_enum, default_value_t)]
        runner: Runner,
        /// Allow network access during the build to fetch external dependencies.
        #[arg(long)]
        network: bool,
        #[arg(long, default_value_t = 30)]
        wait: u64,
        #[arg(long)]
        agent: Option<String>,
    },
    /// List received claims in arrival order, or inspect all versions and checks for one record.
    Log {
        id: Option<String>,
        /// Return only claims after this arrival sequence number (first column of `log` output).
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// Filter by claim kind: publish, check, have, or endpoint.
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
    /// Show this client's keys, relays, and local store counts.
    Status,
}

impl Command {
    /// Resolves relative paths against the caller's working directory before sending over the control socket.
    pub fn absolute(mut self) -> Result<Self> {
        match &mut self {
            Command::Publish { dir, .. } => *dir = std::path::absolute(&*dir)?,
            Command::Get { id, files, .. } => {
                let name = id.chars().take(12).collect::<String>();
                *files = Some(std::path::absolute(
                    files.clone().unwrap_or_else(|| PathBuf::from(name)),
                )?);
            }
            _ => {}
        }
        Ok(self)
    }

    /// Returns true if the command publishes claims that should linger briefly to sync before exit.
    pub fn writes(&self) -> bool {
        matches!(
            self,
            Command::Publish { .. } | Command::Get { .. } | Command::Check { .. }
        )
    }
}

pub async fn run(node: &Node, command: Command) -> Result<String> {
    match command {
        Command::Publish {
            dir,
            title,
            authors,
            source,
            note,
            after,
            comparator,
            agent,
            wait,
        } => {
            let wait = Duration::from_secs(wait);
            publish(node, &dir, title, authors, source, note, after, comparator, agent, wait).await
        }
        Command::Get {
            id,
            files,
            exact,
            wait,
            agent,
        } => {
            let dir = files.context("get: no target directory")?;
            get(node, &id, &dir, exact, Duration::from_secs(wait), agent).await
        }
        Command::Check {
            id,
            exact,
            runner,
            network,
            wait,
            agent,
        } => check(node, &id, exact, runner, network, Duration::from_secs(wait), agent).await,
        Command::Files { id, exact, wait, json } => files(node, &id, exact, Duration::from_secs(wait), json).await,
        Command::Log { id: Some(id), json, .. } => record_log(node, &id, json).await,
        Command::Log {
            id: None,
            since,
            kind,
            limit,
            json,
        } => log(node, since, kind.as_deref(), limit, json).await,
        Command::Status => status(node).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish(
    node: &Node,
    dir: &Path,
    title: Option<String>,
    authors: Vec<String>,
    source: Option<String>,
    note: Option<String>,
    after: Option<String>,
    comparator: Option<String>,
    agent: Option<String>,
    wait: Duration,
) -> Result<String> {
    if let Some(config) = &comparator
        && !dir.join(files::checked(config)?).is_file()
    {
        bail!("--comparator {config}: file not found in {}", dir.display());
    }
    let origin = Origin::read(dir);
    let parent = match after.or_else(|| origin.as_ref().map(|o| o.version.clone())) {
        Some(id) => Some(version(node, &id, Duration::ZERO).await?),
        None => None,
    };

    let snapshot = files::import(&node.store, dir).await?;
    let hash = snapshot.hash.to_string();
    if let Some((stored, previous)) = &parent
        && previous.snapshot == hash
    {
        bail!("nothing changed since version {}", stored.id);
    }

    // Inherit unspecified metadata fields from the parent version.
    let previous = parent.as_ref().map(|(_, publish)| publish);
    let body = claim::Publish {
        title: title
            .or_else(|| previous.map(|p| p.title.clone()))
            .context("--title is required for a new record")?,
        authors: if authors.is_empty() {
            previous.map(|p| p.authors.clone()).unwrap_or_default()
        } else {
            authors
        },
        source: source.or_else(|| previous.and_then(|p| p.source.clone())),
        note,
        snapshot: hash.clone(),
        files: snapshot.files,
        bytes: snapshot.bytes,
        comparator: comparator.or_else(|| previous.and_then(|p| p.comparator.clone())),
    };
    let refs = parent.iter().map(|(stored, _)| stored.id.clone()).collect();
    let id = node.publish(&body, refs, agent.clone()).await?;
    node.announce_holding(&id, &hash, agent).await?;
    Origin {
        version: id.clone(),
        snapshot: hash.clone(),
    }
    .write(dir)?;

    let mut out = Fields::default();
    out.put("Id", &id);
    if let Some((stored, _)) = &parent {
        out.put("Follows", &stored.id);
    }
    out.put("Title", &body.title);
    out.put("Snapshot", &hash);
    out.put("Files", format!("{} ({} bytes)", body.files, body.bytes));
    // Serving nodes keep serving the files directly; short-lived clients wait for a peer to mirror them.
    if !node.serving {
        let others = node.other_holders(&hash, wait).await?;
        out.put("Other-Holders", others);
        if others == 0 {
            out.put(
                "Note",
                "no other peer holds the files yet; they are only downloadable while `delphi-p2p serve` is running locally",
            );
        }
    }
    Ok(out.text())
}

async fn get(node: &Node, id: &str, dir: &Path, exact: bool, wait: Duration, agent: Option<String>) -> Result<String> {
    let (stored, publish) = resolve(node, id, exact, wait).await?;
    let occupied = dir.read_dir().is_ok_and(|mut entries| entries.next().is_some());
    if occupied {
        bail!(
            "{} is not empty; specify a different directory with --files",
            dir.display()
        );
    }
    let hash = publish.snapshot.parse().context("invalid snapshot hash in claim")?;
    node.fetch(hash, wait).await?;
    files::export(&node.store, hash, dir).await?;
    Origin {
        version: stored.id.clone(),
        snapshot: publish.snapshot.clone(),
    }
    .write(dir)?;
    if node.serving {
        node.announce_holding(&stored.id, &publish.snapshot, agent).await?;
    }

    let mut out = Fields::default();
    out.put("Id", &stored.id);
    out.put("Title", &publish.title);
    out.put("Snapshot", &publish.snapshot);
    out.put("Files", format!("{} ({} bytes)", publish.files, publish.bytes));
    out.put("Path", dir.display());
    let tally = tally(node, &stored, &publish).await?;
    out.put("Checks", format!("{} pass, {} fail", tally.pass, tally.fail));
    Ok(out.text())
}

/// File status relative to the parent version.
#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Change {
    Added,
    Changed,
    Same,
}

impl Change {
    fn as_str(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Changed => "changed",
            Change::Same => "same",
        }
    }
}

#[derive(Serialize)]
struct FileLine {
    #[serde(flatten)]
    file: files::Entry,
    /// Omitted when there is no parent version to diff against.
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<Change>,
}

async fn files(node: &Node, id: &str, exact: bool, wait: Duration, json: bool) -> Result<String> {
    let deadline = tokio::time::Instant::now() + wait;
    let (stored, publish) = resolve(node, id, exact, wait).await?;
    let hash = publish.snapshot.parse().context("invalid snapshot hash in claim")?;
    let list = node.list(hash, deadline - tokio::time::Instant::now()).await?;

    // Fetch the parent version's manifest if available so we can compute file diffs.
    let mut before = None;
    for parent in &stored.claim.refs {
        if let Ok((earlier, body)) = version(node, parent, Duration::ZERO).await
            && let Ok(snapshot) = body.snapshot.parse()
            && let Ok(files) = node.list(snapshot, deadline - tokio::time::Instant::now()).await
        {
            before = Some((earlier.id, files));
            break;
        }
    }
    let earlier: Option<BTreeMap<&str, &files::Entry>> = before
        .as_ref()
        .map(|(_, files)| files.iter().map(|f| (f.path.as_str(), f)).collect());
    let lines: Vec<FileLine> = list
        .iter()
        .map(|file| FileLine {
            file: file.clone(),
            change: earlier.as_ref().map(|earlier| match earlier.get(file.path.as_str()) {
                None => Change::Added,
                Some(old) if old.hash == file.hash => Change::Same,
                Some(_) => Change::Changed,
            }),
        })
        .collect();
    let kept: HashSet<&str> = list.iter().map(|f| f.path.as_str()).collect();
    let removed: Vec<&files::Entry> = before
        .iter()
        .flat_map(|(_, files)| files)
        .filter(|f| !kept.contains(f.path.as_str()))
        .collect();

    let after = before.as_ref().map(|(id, _)| id);
    if json {
        let value = serde_json::json!({
            "id": stored.id, "snapshot": publish.snapshot, "after": after, "files": lines, "removed": removed,
        });
        return Ok(serde_json::to_string(&value)? + "\n");
    }
    let mut out = Fields::default();
    out.put("Id", &stored.id);
    out.put("Snapshot", &publish.snapshot);
    out.put("Files", format!("{} ({} bytes)", publish.files, publish.bytes));
    if let Some(after) = after {
        out.put("Compared-With", after);
    }
    let mut text = out.text();
    text.push('\n');
    for line in &lines {
        let change = line.change.map_or("-", Change::as_str);
        let file = &line.file;
        text.push_str(&format!("{change}\t{}\t{}\t{}\n", file.bytes, file.hash, file.path));
    }
    for file in removed {
        text.push_str(&format!("removed\t{}\t{}\t{}\n", file.bytes, file.hash, file.path));
    }
    Ok(text)
}

async fn check(
    node: &Node,
    id: &str,
    exact: bool,
    runner: Runner,
    network: bool,
    wait: Duration,
    agent: Option<String>,
) -> Result<String> {
    let (stored, publish) = resolve(node, id, exact, wait).await?;
    let hash = publish.snapshot.parse().context("invalid snapshot hash in claim")?;
    node.fetch(hash, wait).await?;

    // Unpack into a clean directory so leftovers from earlier runs cannot affect the build.
    let dir = node.home.work(&publish.snapshot);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    files::export(&node.store, hash, &dir).await?;
    let outcome = check::run(&dir, publish.comparator.as_deref(), runner, network).await?;

    let body = claim::Check {
        snapshot: publish.snapshot.clone(),
        checker: outcome.checker,
        toolchain: outcome.toolchain,
        runner: outcome.runner,
        result: outcome.result,
        exit_code: outcome.exit_code,
        detail: outcome.detail,
    };
    let check_id = node.publish(&body, vec![stored.id.clone()], agent.clone()).await?;
    if node.serving {
        node.announce_holding(&stored.id, &publish.snapshot, agent).await?;
    }

    let mut out = Fields::default();
    out.put("Id", &check_id);
    out.put("Version", &stored.id);
    out.put("Snapshot", &publish.snapshot);
    out.put(
        "Checker",
        format!("{}, {}, runner {}", body.checker, body.toolchain, body.runner),
    );
    out.put("Result", body.result.as_str());
    let tally = tally(node, &stored, &publish).await?;
    out.put("Checks", format!("{} pass, {} fail", tally.pass, tally.fail));
    let mut text = out.text();
    if let Some(detail) = &body.detail {
        text.push('\n');
        text.push_str(detail);
        text.push('\n');
    }
    Ok(text)
}

async fn log(node: &Node, since: i64, kind: Option<&str>, limit: i64, json: bool) -> Result<String> {
    let entries = node.index.since(since, kind, limit).await?;
    if json {
        let entries: Vec<_> = entries
            .iter()
            .map(|e| {
                // `micros` is the raw signed timestamp; `at` is the ISO-8601 UTC string.
                serde_json::json!({
                    "seq": e.seq, "id": e.id, "author": e.author, "at": date(e.at), "micros": e.at, "claim": e.claim,
                })
            })
            .collect();
        return Ok(serde_json::to_string(&serde_json::json!({ "entries": entries }))? + "\n");
    }
    let mut text = String::new();
    for e in &entries {
        let fields = [
            e.seq.to_string(),
            date(e.at),
            e.claim.kind.clone(),
            e.id.clone(),
            e.author.clone(),
            summary(e),
        ];
        text.push_str(&fields.join("\t"));
        text.push('\n');
    }
    Ok(text)
}

/// Short human-readable summary column for `log` output.
fn summary(stored: &Stored) -> String {
    let claim = &stored.claim;
    if let Some(publish) = claim.read::<claim::Publish>() {
        let version = if claim.refs.is_empty() { "" } else { " (new version)" };
        return format!("{}{version}", publish.title);
    }
    if let Some(check) = claim.read::<claim::Check>() {
        return format!("{} by {}", check.result.as_str(), check.checker);
    }
    if let Some(have) = claim.read::<claim::Have>() {
        return format!("holds {}", have.snapshot);
    }
    if let Some(endpoint) = claim.read::<claim::Endpoint>() {
        return format!("files at {}", endpoint.blobs.id);
    }
    String::new()
}

#[derive(Serialize)]
struct VersionLine {
    id: String,
    at: String,
    author: String,
    /// `"current"`, `"earlier"`, `"open"` (multiple unmerged author heads), or `"fork"`.
    state: &'static str,
    snapshot: String,
    holders: usize,
    pass: usize,
    fail: usize,
    note: Option<String>,
}

async fn record_log(node: &Node, id: &str, json: bool) -> Result<String> {
    let (given, _) = version(node, id, Duration::ZERO).await?;
    let record = Record::of(node, given).await?;
    let head = record.head().id.clone();
    let followed = record.followed_by_author();

    let mut lines = Vec::new();
    for (stored, publish) in &record.versions {
        let state = if stored.id == head {
            "current"
        } else if stored.author != record.author() {
            "fork"
        } else if followed.contains(&stored.id) {
            "earlier"
        } else {
            "open"
        };
        let tally = tally(node, stored, publish).await?;
        lines.push(VersionLine {
            id: stored.id.clone(),
            at: date(stored.at),
            author: stored.author.clone(),
            state,
            snapshot: publish.snapshot.clone(),
            holders: node.index.holders(&publish.snapshot).await?.len(),
            pass: tally.pass,
            fail: tally.fail,
            note: publish.note.clone(),
        });
    }

    let (root, first) = &record.versions[0];
    if json {
        let value =
            serde_json::json!({ "record": root.id, "title": first.title, "author": root.author, "versions": lines });
        return Ok(serde_json::to_string(&value)? + "\n");
    }
    let mut out = Fields::default();
    out.put("Record", &root.id);
    out.put("Title", &first.title);
    out.put("Author", &root.author);
    out.put("Versions", lines.len());
    let mut text = out.text();
    text.push('\n');
    for line in &lines {
        let fields = [
            line.id.clone(),
            line.at.clone(),
            line.state.to_string(),
            line.author.clone(),
            format!("holders={}", line.holders),
            format!("pass={}", line.pass),
            format!("fail={}", line.fail),
            line.note.clone().unwrap_or_default(),
        ];
        text.push_str(&fields.join("\t"));
        text.push('\n');
    }
    Ok(text)
}

async fn status(node: &Node) -> Result<String> {
    let mut out = Fields::default();
    out.put("Node", &node.me);
    out.put("Files-Endpoint", node.files_id());
    out.put("Home", node.home.dir.display());
    out.put(
        "Relays",
        if node.config.relays.is_empty() {
            "none (local network only)".into()
        } else {
            node.config.relays.join(", ")
        },
    );
    out.put("Claims", node.index.count().await?);
    out.put(
        "Records",
        node.index
            .publishes()
            .await?
            .iter()
            .filter(|p| p.claim.refs.is_empty())
            .count(),
    );
    Ok(out.text())
}

/// Fetches a `publish` claim by ID or prefix, waiting up to `wait` for it to sync.
async fn version(node: &Node, id: &str, wait: Duration) -> Result<(Stored, claim::Publish)> {
    let stored = node
        .wait_for(id, wait)
        .await?
        .with_context(|| format!("claim {id} not found locally"))?;
    let publish = stored
        .claim
        .read::<claim::Publish>()
        .with_context(|| format!("{} is a {} claim, not a publish claim", stored.id, stored.claim.kind))?;
    Ok((stored, publish))
}

/// Resolves the target version: either the exact ID passed or the original author's latest head.
async fn resolve(node: &Node, id: &str, exact: bool, wait: Duration) -> Result<(Stored, claim::Publish)> {
    let named = version(node, id, wait).await?;
    if exact {
        return Ok(named);
    }
    let record = Record::of(node, named.0).await?;
    let head = record.head().id.clone();
    Ok(record
        .versions
        .into_iter()
        .find(|(stored, _)| stored.id == head)
        .expect("head version exists in record"))
}

/// All known versions belonging to a single record, starting with the root version.
struct Record {
    versions: Vec<(Stored, claim::Publish)>,
}

impl Record {
    async fn of(node: &Node, version: Stored) -> Result<Self> {
        // Walk backwards along parent refs to the earliest known version.
        let mut root = version;
        let mut seen = HashSet::from([root.id.clone()]);
        'back: loop {
            for parent in &root.claim.refs {
                if let Some(stored) = node.index.get(parent).await?
                    && stored.claim.read::<claim::Publish>().is_some()
                    && seen.insert(stored.id.clone())
                {
                    root = stored;
                    continue 'back;
                }
            }
            break;
        }
        // Traverse forward to collect all descendant versions and forks.
        let mut versions = Vec::new();
        let mut queue = vec![root];
        let mut listed = HashSet::new();
        while let Some(stored) = queue.pop() {
            if !listed.insert(stored.id.clone()) {
                continue;
            }
            let Some(publish) = stored.claim.read::<claim::Publish>() else {
                continue;
            };
            queue.extend(node.index.referring::<claim::Publish>(&stored.id).await?);
            versions.push((stored, publish));
        }
        versions[1..].sort_by(|a, b| (a.0.at, &a.0.id).cmp(&(b.0.at, &b.0.id)));
        Ok(Record { versions })
    }

    fn author(&self) -> &str {
        &self.versions[0].0.author
    }

    /// Version IDs superseded by a later version from the original author.
    fn followed_by_author(&self) -> HashSet<String> {
        self.versions
            .iter()
            .filter(|(stored, _)| stored.author == self.author())
            .flat_map(|(stored, _)| stored.claim.refs.iter().cloned())
            .collect()
    }

    /// Current head version: the original author's latest unsuperseded version.
    /// Forks by other keys never replace it.
    fn head(&self) -> &Stored {
        let followed = self.followed_by_author();
        self.versions
            .iter()
            .map(|(stored, _)| stored)
            .filter(|stored| stored.author == self.author() && !followed.contains(&stored.id))
            .max_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)))
            .unwrap_or(&self.versions[0].0)
    }
}

struct Tally {
    pass: usize,
    fail: usize,
}

/// Counts distinct signing keys using each key's latest verdict for this exact snapshot.
async fn tally(node: &Node, stored: &Stored, publish: &claim::Publish) -> Result<Tally> {
    let mut verdicts = BTreeMap::new();
    for check in node.index.referring::<claim::Check>(&stored.id).await? {
        if let Some(body) = check.claim.read::<claim::Check>()
            && body.snapshot == publish.snapshot
        {
            verdicts.insert(check.author, body.result);
        }
    }
    Ok(Tally {
        pass: verdicts.values().filter(|v| **v == Verdict::Pass).count(),
        fail: verdicts.values().filter(|v| **v == Verdict::Fail).count(),
    })
}

/// Key-value output builder (`Name: value\n`), matching the `delphi` CLI format.
#[derive(Default)]
struct Fields(String);

impl Fields {
    fn put(&mut self, name: &str, value: impl std::fmt::Display) {
        self.0.push_str(&format!("{name}: {value}\n"));
    }

    fn text(self) -> String {
        self.0
    }
}

/// Formats microseconds since the Unix epoch as an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
fn date(micros: u64) -> String {
    let seconds = micros / 1_000_000;
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Civil date conversion from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::date;

    #[test]
    fn formats_utc_calendar_dates() {
        assert_eq!(date(0), "1970-01-01T00:00:00Z");
        assert_eq!(date(951_782_400_000_000), "2000-02-29T00:00:00Z");
        assert_eq!(date(1_795_342_500_000_000), "2026-11-22T10:15:00Z");
    }
}
