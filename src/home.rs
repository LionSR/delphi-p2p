//! Client data directory layout: keys, configuration, databases, and working paths.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Bootstrap peer public key and relay URL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bootstrap {
    /// Peer public key in hex.
    pub node: String,
    pub relay: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Relay URLs to connect through. If empty, peers connect on the local network only.
    #[serde(default)]
    pub relays: Vec<String>,
    #[serde(default)]
    pub bootstrap: Vec<Bootstrap>,
    /// Default agent name recorded in claims unless overridden by `--agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Enable mDNS local-network peer discovery.
    #[serde(default = "yes")]
    pub mdns: bool,
}

fn yes() -> bool {
    true
}

/// Private keys stored in `keys.json` (mode `0600`).
#[derive(Serialize, Deserialize)]
pub struct Keys {
    /// Keypair used to sign claims and identify this peer in p2panda.
    pub claims: p2panda::Credentials,
    /// Hex-encoded secret key for the separate iroh-blobs file-transfer endpoint.
    pub blobs: String,
}

impl Keys {
    fn generate() -> Self {
        let blobs = iroh::SecretKey::generate().to_bytes();
        Keys {
            claims: p2panda::Credentials::generate(),
            blobs: blobs.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    pub fn blobs_key(&self) -> Result<iroh::SecretKey> {
        let mut bytes = [0u8; 32];
        if self.blobs.len() != 64 {
            bail!("keys.json: blobs key must be 32 hex-encoded bytes");
        }
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&self.blobs[2 * i..2 * i + 2], 16)
                .context("keys.json: blobs key is not valid hex")?;
        }
        Ok(iroh::SecretKey::from_bytes(&bytes))
    }
}

/// Safe limit below the shortest Unix `sun_path` buffer on supported platforms (104 bytes on macOS).
const SOCKET_PATH_LIMIT: usize = 100;

#[derive(Clone, Debug)]
pub struct Home {
    pub dir: PathBuf,
}

impl Home {
    /// Resolves the data directory from `--home`, `$DELPHI_P2P_HOME`, or `~/.delphi/p2p`.
    pub fn locate(flag: Option<PathBuf>) -> Result<Self> {
        let dir = match flag.or_else(|| std::env::var_os("DELPHI_P2P_HOME").map(PathBuf::from)) {
            Some(dir) => dir,
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".delphi/p2p"),
        };
        Ok(Home {
            dir: std::path::absolute(dir)?,
        })
    }

    pub fn exists(&self) -> bool {
        self.dir.join("keys.json").exists()
    }

    /// Initializes the data directory and config. Preserves existing keys if already present.
    pub fn init(&self, config: &Config) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        // Restrict directory permissions to the current user because it stores private keys and the control socket.
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        if !self.exists() {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.dir.join("keys.json"))?;
            file.write_all(serde_json::to_string_pretty(&Keys::generate())?.as_bytes())?;
        }
        self.write_config(config)
    }

    pub fn keys(&self) -> Result<Keys> {
        let path = self.dir.join("keys.json");
        let text = fs::read_to_string(&path).with_context(|| {
            format!(
                "client not initialized at {}: run delphi-p2p init first",
                self.dir.display()
            )
        })?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn config(&self) -> Result<Config> {
        let path = self.dir.join("config.json");
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("{}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn write_config(&self, config: &Config) -> Result<()> {
        fs::write(
            self.dir.join("config.json"),
            serde_json::to_string_pretty(config)? + "\n",
        )?;
        Ok(())
    }

    /// Acquires an exclusive file lock on the data directory. Returns `Ok(None)` if another process
    /// holds it; the lock is released when the returned file handle is dropped.
    pub fn lock(&self) -> Result<Option<fs::File>> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(err)) => Err(err.into()),
        }
    }

    pub fn claims_db(&self) -> PathBuf {
        self.dir.join("claims.sqlite")
    }

    pub fn index_db(&self) -> PathBuf {
        self.dir.join("index.sqlite")
    }

    pub fn blobs(&self) -> PathBuf {
        self.dir.join("blobs")
    }

    /// Working directory where `check` unpacks a snapshot before running Lake.
    pub fn work(&self, snapshot: &str) -> PathBuf {
        self.dir.join("work").join(snapshot)
    }

    /// Path to the control socket. Falls back to a hashed name in the runtime/temp directory
    /// if the path inside `self.dir` would exceed the Unix socket length limit.
    pub fn socket(&self) -> PathBuf {
        let inside = self.dir.join("control.sock");
        if inside.as_os_str().len() <= SOCKET_PATH_LIMIT {
            return inside;
        }
        let runtime = ["XDG_RUNTIME_DIR", "TMPDIR"]
            .iter()
            .find_map(std::env::var_os)
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let name = p2panda::Hash::digest(self.dir.as_os_str().as_encoded_bytes()).to_hex();
        runtime.join(format!("delphi-p2p-{}.sock", &name[..16]))
    }

    /// Cache file storing the last published `endpoint` claim to avoid redundant announcements.
    pub fn announced(&self) -> PathBuf {
        self.dir.join("announced.json")
    }
}

/// Metadata written to `.delphi/origin.json` when fetching or publishing a directory so subsequent
/// `publish` calls automatically link to the previous version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    /// ID of the `publish` claim corresponding to this directory.
    pub version: String,
    pub snapshot: String,
}

impl Origin {
    fn path(dir: &Path) -> PathBuf {
        dir.join(".delphi/origin.json")
    }

    pub fn read(dir: &Path) -> Option<Self> {
        serde_json::from_str(&fs::read_to_string(Self::path(dir)).ok()?).ok()
    }

    pub fn write(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir.join(".delphi"))?;
        fs::write(Self::path(dir), serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }
}
