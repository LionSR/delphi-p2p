//! Claim payload schema.
//!
//! Each claim is stored as the body of a p2panda operation. The p2panda header stores the author's
//! public key, signature, and timestamp; the body holds the claim kind, referenced claim IDs, and
//! kind-specific fields.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Envelope for all claim kinds.
///
/// `data` remains untyped JSON so peers can store and forward unknown claim kinds.
/// Callers decode typed payloads via [`Claim::read`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub kind: String,
    /// Optional agent name recorded by the client (for example, `"Claude Code"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Claim IDs referenced by this claim.
    #[serde(default)]
    pub refs: Vec<String>,
    pub data: Value,
}

/// Typed payload for a specific claim kind.
pub trait Kind: Serialize + DeserializeOwned {
    const NAME: &'static str;
}

impl Claim {
    pub fn new<K: Kind>(body: &K, refs: Vec<String>, agent: Option<String>) -> Self {
        Claim {
            kind: K::NAME.to_string(),
            agent,
            refs,
            data: serde_json::to_value(body).expect("claim fields serialize to JSON"),
        }
    }

    /// Decodes `data` as kind `K`, returning `None` if the kind does not match or parsing fails.
    pub fn read<K: Kind>(&self) -> Option<K> {
        if self.kind != K::NAME {
            return None;
        }
        serde_json::from_value(self.data.clone()).ok()
    }
}

/// A published version of a research artifact. `refs` lists parent version IDs (empty for the initial version).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Publish {
    pub title: String,
    #[serde(default)]
    pub authors: Vec<String>,
    /// Source URL for the work (for example, an arXiv or delphi.science record URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Summary of changes in this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// BLAKE3 hash of the iroh-blobs collection representing the directory snapshot.
    pub snapshot: String,
    pub files: u64,
    pub bytes: u64,
    /// Relative path inside the snapshot to a `lake comparator` config file.
    /// If omitted, `delphi-p2p check` runs `lake check` (or `lake comparator` if `comparator.json` exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparator: Option<String>,
}

impl Kind for Publish {
    const NAME: &'static str = "publish";
}

/// Announces that the signing peer holds and serves a snapshot. `refs` contains the `publish` claim ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Have {
    pub snapshot: String,
}

impl Kind for Have {
    const NAME: &'static str = "have";
}

/// Advertises a peer's iroh-blobs endpoint address and serving status. The latest claim per author applies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub blobs: iroh::EndpointAddr,
    /// True when the peer is running `delphi-p2p serve` and can be used as a bootstrap node.
    #[serde(default)]
    pub serving: bool,
}

impl Kind for Endpoint {
    const NAME: &'static str = "endpoint";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The checker accepted the snapshot.
    Pass,
    /// The checker ran and rejected the snapshot.
    Fail,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
        }
    }
}

/// Signed result of running a checker on a snapshot. `refs` contains the `publish` claim ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// Exact snapshot hash that was checked; does not apply to other versions.
    pub snapshot: String,
    /// Checker command that was executed (for example, `"lake check"` or `"lake comparator"`).
    pub checker: String,
    /// Lean toolchain string from `lean-toolchain` (for example, `"leanprover/lean4:v4.36.0"`).
    pub toolchain: String,
    /// Sandbox runner used: `"bwrap"`, `"docker+bwrap"`, or `"none"`.
    pub runner: String,
    pub result: Verdict,
    pub exit_code: i32,
    /// Truncated tail of checker output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Kind for Check {
    const NAME: &'static str = "check";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_reads_only_matching_kind() {
        let have = Have {
            snapshot: "ab".repeat(32),
        };
        let claim = Claim::new(&have, vec!["cd".repeat(32)], Some("Claude Code".into()));
        assert_eq!(claim.kind, "have");
        assert_eq!(claim.read::<Have>(), Some(have));
        assert_eq!(claim.read::<Publish>(), None);
    }

    #[test]
    fn unknown_claim_kind_decodes() {
        let claim: Claim = serde_json::from_str(r#"{"kind":"vote","refs":["00"],"data":{"value":1}}"#).unwrap();
        assert_eq!(claim.kind, "vote");
        assert_eq!(claim.read::<Have>(), None);
    }
}
