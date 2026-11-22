//! Live peer-to-peer node combining p2panda claim sync and iroh-blobs file transfer.
//!
//! Because p2panda does not expose its underlying endpoint for external protocols, file transfers
//! run over a separate iroh endpoint with its own keypair.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{EndpointAddr, EndpointId, RelayMode, RelayUrl};
use iroh_blobs::api::downloader::{DownloadProgress, DownloadProgressItem, Shuffled};
use iroh_blobs::protocol::GetRequest;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobsProtocol, Hash};
use p2panda::network::MdnsDiscoveryMode;
use p2panda::node::AckPolicy;
use p2panda::streams::{StreamEvent, StreamPublisher, StreamSubscription};
use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout};

use crate::claim::{self, Claim, Kind};
use crate::files;
use crate::home::{Config, Home};
use crate::index::{Index, Stored};

/// Network namespace identifier; only peers sharing this identifier sync with each other.
const NETWORK: &str = "delphi.science/1";
/// Single gossip topic used in v1 for all public records.
const TOPIC: &str = "delphi.science/1/records";

/// Maximum serialized claim size in bytes, kept below p2panda's 4,096-byte default gossip limit.
const MAX_CLAIM_BYTES: usize = 3000;

/// Maximum number of learned serving peers added as bootstrap candidates on startup.
const MAX_LEARNED_BOOTSTRAPS: i64 = 32;

/// Idle timeout after which a stalled blob download is aborted.
const STALLED: Duration = Duration::from_secs(60);

pub struct Node {
    pub home: Home,
    pub config: Config,
    /// Hex-encoded public key of this peer (used as the author of signed claims).
    pub me: String,
    pub index: Index,
    pub store: FsStore,
    /// True when running under `delphi-p2p serve`. Only serving peers announce `have` claims
    /// for snapshots they download, since short-lived commands exit immediately.
    pub serving: bool,
    // Keep the p2panda node alive for the lifetime of `Node` so networking stays active.
    _claims: p2panda::Node,
    publisher: StreamPublisher<Claim>,
    files: Router,
    lookup: MemoryLookup,
    sessions: watch::Receiver<u64>,
}

impl Node {
    pub async fn spawn(home: Home, serving: bool) -> Result<Self> {
        let config = home.config()?;
        let keys = home.keys()?;
        let index = Index::open(&home.index_db()).await?;

        let relays: Vec<RelayUrl> = config
            .relays
            .iter()
            .map(|url| {
                url.parse()
                    .with_context(|| format!("config.json: invalid relay URL: {url}"))
            })
            .collect::<Result<_>>()?;

        let mut builder = p2panda::builder()
            .credentials(keys.claims.clone())
            .database_url(&format!("sqlite://{}", home.claims_db().display()))
            .network_id(*p2panda::Hash::digest(NETWORK).as_bytes())
            .ack_policy(AckPolicy::Explicit)
            .mdns_mode(if config.mdns {
                MdnsDiscoveryMode::Active
            } else {
                MdnsDiscoveryMode::Disabled
            });
        for relay in &relays {
            builder = builder.relay_url(relay.clone());
        }
        for peer in &config.bootstrap {
            let node = peer
                .node
                .parse()
                .with_context(|| format!("config.json: invalid public key: {}", peer.node))?;
            let relay = peer
                .relay
                .parse()
                .with_context(|| format!("config.json: invalid relay URL: {}", peer.relay))?;
            builder = builder.bootstrap(node, relay);
        }
        let claims = builder.spawn().await.context("failed to start p2panda node")?;
        let me = claims.id().to_hex();

        // Add previously discovered serving peers as extra bootstraps so discovery continues if
        // the configured seed goes offline.
        for stored in index.latest_of_each::<claim::Endpoint>(MAX_LEARNED_BOOTSTRAPS).await? {
            let Some(endpoint) = stored.claim.read::<claim::Endpoint>() else {
                continue;
            };
            let (Ok(peer), Some(relay)) = (stored.author.parse(), endpoint.blobs.relay_urls().next()) else {
                continue;
            };
            if endpoint.serving && stored.author != me {
                let _ = claims.insert_bootstrap(peer, relay.clone()).await;
            }
        }

        let topic = p2panda::Topic::from(p2panda::Hash::digest(TOPIC));
        let (publisher, subscription) = claims
            .stream::<Claim>(topic)
            .await
            .context("failed to open records topic")?;
        let (ended, sessions) = watch::channel(0);
        tokio::spawn(consume(subscription, index.clone(), ended));

        let lookup = MemoryLookup::new();
        let relay_mode = if relays.is_empty() {
            RelayMode::Disabled
        } else {
            RelayMode::custom(relays)
        };
        let endpoint = iroh::Endpoint::builder(presets::Minimal)
            .secret_key(keys.blobs_key()?)
            .relay_mode(relay_mode)
            .address_lookup(lookup.clone())
            .bind()
            .await
            .context("failed to bind iroh file endpoint")?;
        let store = FsStore::load(home.blobs())
            .await
            .map_err(|err| anyhow::anyhow!("file store: {err}"))?;
        let files = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&store, None))
            .spawn();

        let node = Node {
            home,
            config,
            me,
            index,
            store,
            serving,
            _claims: claims,
            publisher,
            files,
            lookup,
            sessions,
        };
        node.announce().await?;
        Ok(node)
    }

    /// Signs, stores, and broadcasts a claim, returning its hex operation ID.
    pub async fn publish<K: Kind>(&self, body: &K, refs: Vec<String>, agent: Option<String>) -> Result<String> {
        let claim = Claim::new(body, refs, agent.or_else(|| self.config.agent.clone()));
        let size = serde_json::to_vec(&claim)?.len();
        if size > MAX_CLAIM_BYTES {
            bail!("claim is {size} bytes, exceeding the {MAX_CLAIM_BYTES}-byte limit");
        }
        let pending = self.publisher.publish(claim).await?;
        let id = pending.hash().to_hex();
        pending.await.context("claim was not processed by p2panda")?;
        // Wait for our own published claim to loop through the subscription into the local index.
        self.wait_for(&id, Duration::from_secs(10))
            .await?
            .context("published claim did not appear in the local index")?;
        Ok(id)
    }

    /// Returns the claim matching `id`, waiting up to `patience` for it to sync from peers.
    pub async fn wait_for(&self, id: &str, patience: Duration) -> Result<Option<Stored>> {
        let deadline = Instant::now() + patience;
        let mut arrivals = self.index.arrivals();
        loop {
            if let Some(found) = self.index.get(id).await? {
                return Ok(Some(found));
            }
            if timeout(deadline - Instant::now(), arrivals.changed()).await.is_err() {
                return Ok(None);
            }
        }
    }

    /// Waits for at least one sync session to finish and incoming claim traffic to go quiet,
    /// bounded by `patience`.
    pub async fn settle(&self, patience: Duration) {
        let deadline = Instant::now() + patience;
        let mut sessions = self.sessions.clone();
        if *sessions.borrow() == 0 && timeout(deadline - Instant::now(), sessions.changed()).await.is_err() {
            return;
        }
        let mut arrivals = self.index.arrivals();
        while Instant::now() < deadline {
            if timeout(Duration::from_millis(700), arrivals.changed()).await.is_err() {
                return;
            }
        }
    }

    /// Returns true if at least one sync session with a peer has completed since startup.
    pub fn met_someone(&self) -> bool {
        *self.sessions.borrow() > 0
    }

    /// Downloads any missing files of `snapshot` from peers holding it until complete locally.
    pub async fn fetch(&self, snapshot: Hash, patience: Duration) -> Result<()> {
        let deadline = Instant::now() + patience;
        while !files::complete(&self.store, snapshot).await? {
            self.ask(snapshot, GetRequest::all(snapshot), deadline).await?;
        }
        Ok(())
    }

    /// Returns the file manifest of `snapshot`, fetching only metadata and file sizes if needed.
    pub async fn list(&self, snapshot: Hash, patience: Duration) -> Result<Vec<files::Entry>> {
        let deadline = Instant::now() + patience;
        loop {
            if let Some(list) = files::list(&self.store, snapshot).await? {
                return Ok(list);
            }
            self.ask(snapshot, files::listing(snapshot), deadline).await?;
        }
    }

    /// Attempts one download round from known snapshot holders. `deadline` caps how long we search
    /// for a peer; once a transfer begins it runs until completion or stall timeout.
    async fn ask(&self, snapshot: Hash, request: GetRequest, deadline: Instant) -> Result<()> {
        let mut problem = None;
        let holders = self.holders(snapshot).await?;
        let known = holders.len();
        if known > 0 && Instant::now() < deadline {
            let download = self
                .store
                .downloader(self.files.endpoint())
                .download(request, Shuffled::new(holders));
            match transfer(download).await {
                Ok(()) => return Ok(()),
                Err(err) => problem = Some(err.to_string()),
            }
        }
        if Instant::now() >= deadline {
            match problem {
                Some(err) => bail!("failed to fetch snapshot {snapshot}: {err}"),
                None if known > 0 => bail!("snapshot {snapshot} is not in the local store"),
                None => bail!("no known peer holds snapshot {snapshot} yet"),
            }
        }
        // Retry briefly in case holder claims are still syncing or a peer is coming online.
        sleep(Duration::from_millis(500)).await;
        Ok(())
    }

    /// Counts distinct other peers that have announced a `have` claim for `snapshot`, waiting up
    /// to `patience` for at least one peer to confirm before returning.
    pub async fn other_holders(&self, snapshot: &str, patience: Duration) -> Result<usize> {
        let deadline = Instant::now() + patience;
        let mut arrivals = self.index.arrivals();
        loop {
            let holders = self.index.holders(snapshot).await?;
            let others = holders.iter().filter(|author| **author != self.me).count();
            if others > 0 || timeout(deadline - Instant::now(), arrivals.changed()).await.is_err() {
                return Ok(others);
            }
        }
    }

    /// Publishes a `have` claim for `snapshot` if this peer has not already announced one.
    pub async fn announce_holding(&self, version: &str, snapshot: &str, agent: Option<String>) -> Result<()> {
        if !self.index.holders(snapshot).await?.contains(&self.me) {
            let have = claim::Have {
                snapshot: snapshot.to_string(),
            };
            self.publish(&have, vec![version.to_string()], agent).await?;
        }
        Ok(())
    }

    /// Announces `have` claims for all complete snapshots in the local store when starting `serve`.
    pub async fn announce_all_held(&self) -> Result<()> {
        for stored in self.index.publishes().await? {
            let Some(publish) = stored.claim.read::<claim::Publish>() else {
                continue;
            };
            let Ok(hash) = publish.snapshot.parse() else { continue };
            if files::complete(&self.store, hash).await? {
                self.announce_holding(&stored.id, &publish.snapshot, None).await?;
            }
        }
        Ok(())
    }

    pub fn files_id(&self) -> EndpointId {
        self.files.endpoint().id()
    }

    pub async fn shutdown(self) {
        let _ = self.files.shutdown().await;
        let _ = self.store.shutdown().await;
    }

    /// Returns endpoint IDs of other peers holding `snapshot` and registers their addresses.
    async fn holders(&self, snapshot: Hash) -> Result<Vec<EndpointId>> {
        let mut ids = Vec::new();
        for author in self.index.holders(&snapshot.to_string()).await? {
            if author == self.me {
                continue;
            }
            let Some(stored) = self.index.latest_by::<claim::Endpoint>(&author).await? else {
                continue;
            };
            let Some(endpoint) = stored.claim.read::<claim::Endpoint>() else {
                continue;
            };
            ids.push(endpoint.blobs.id);
            self.lookup.add_endpoint_info(endpoint.blobs);
        }
        Ok(ids)
    }

    /// Publishes an `endpoint` claim whenever this peer's file address or serving status changes.
    async fn announce(&self) -> Result<()> {
        let now = claim::Endpoint {
            blobs: self.files_addr().await,
            serving: self.serving,
        };
        let path = self.home.announced();
        let last: Option<claim::Endpoint> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        if last.as_ref() != Some(&now) {
            self.publish(&now, vec![], None).await?;
            std::fs::write(&path, serde_json::to_string(&now)?)?;
        }
        Ok(())
    }

    /// Returns the endpoint address to advertise. When relays are configured, publishes only the
    /// relay URL so local LAN IP addresses are not leaked into public claims.
    async fn files_addr(&self) -> EndpointAddr {
        let endpoint = self.files.endpoint();
        if self.config.relays.is_empty() {
            for _ in 0..30 {
                if endpoint.addr().ip_addrs().next().is_some() {
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
            endpoint.addr()
        } else {
            let _ = timeout(Duration::from_secs(10), endpoint.online()).await;
            let addr = endpoint.addr();
            EndpointAddr::from_parts(addr.id, addr.addrs.into_iter().filter(|a| a.is_relay()))
        }
    }
}

/// Persists incoming p2panda claims into the SQLite index and acknowledges them after indexing.
/// Unacknowledged operations are redelivered by p2panda after a restart.
async fn consume(mut subscription: StreamSubscription<Claim>, index: Index, ended: watch::Sender<u64>) {
    while let Some(event) = subscription.next().await {
        match event {
            StreamEvent::Processed { operation, .. } => {
                let id = operation.id().to_hex();
                let author = operation.author().to_hex();
                match index
                    .insert(&id, &author, operation.timestamp(), operation.message())
                    .await
                {
                    Ok(_) => {
                        if let Err(err) = operation.ack().await {
                            eprintln!("delphi-p2p: failed to acknowledge claim {id}: {err}");
                        }
                    }
                    Err(err) => eprintln!("delphi-p2p: failed to index claim {id}: {err}"),
                }
            }
            StreamEvent::SyncEnded { .. } => ended.send_modify(|n| *n += 1),
            _ => {}
        }
    }
}

/// Drives a blob download to completion, aborting if no progress occurs within `STALLED`.
/// Partially downloaded chunks remain cached in the local blob store.
async fn transfer(download: DownloadProgress) -> Result<()> {
    let mut items = download.stream().await.map_err(|err| anyhow::anyhow!("{err}"))?;
    loop {
        match timeout(STALLED, items.next()).await {
            Ok(Some(DownloadProgressItem::Error(err))) => bail!("{err}"),
            Ok(Some(DownloadProgressItem::DownloadError)) => bail!("failed to download from available peers"),
            Ok(Some(_)) => {}
            Ok(None) => return Ok(()),
            Err(_) => bail!("transfer stalled"),
        }
    }
}
