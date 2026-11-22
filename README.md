# delphi-p2p

Use `delphi-p2p` to share research files and check Lean proofs. Peers fetch files by content hash and
exchange signed claims about which versions they hold or have checked, without relying on delphi.science.

[p2panda](https://github.com/p2panda/p2panda) 0.8 handles peer discovery and claim synchronization.
[iroh-blobs](https://github.com/n0-computer/iroh-blobs) 0.105 handles file transfers.
For Lean projects, the client runs `lake check` or `lake comparator` in a sandbox (Lean 4.35 or later).

The claim format is in `src/claim.rs`, the local index in `src/index.rs`, the commands in `src/ops.rs`,
and the checker in `src/check.rs`.

## Install

Requires Rust 1.96 or later on Linux or macOS:

```bash
git clone https://github.com/LionSR/delphi-p2p.git
cd delphi-p2p
cargo install --locked --path .
delphi-p2p --help
```

To join an existing network, get the relay URL and bootstrap key from the network operator, then run `delphi-p2p init`.

## Build and test

```bash
cargo build                                  # Rust 1.96 or later
cargo test                                   # unit tests
RELAY_BIN=/path/to/iroh-relay tests/e2e.sh   # run a local relay, seed peer, and three clients
```

`tests/e2e.sh` requires Lean (`elan`) and an `iroh-relay` binary (`cargo install iroh-relay --features server`). By
default it runs checks without a sandbox on its generated test projects; set `RUNNER=docker` to run checks inside
the Docker sandbox.

## Commands

```
delphi-p2p init --relay <URL> --bootstrap <key>@<URL>   initialize keys and configuration
delphi-p2p publish <dir> --title <t>                    publish a new record or a new version of an existing one
delphi-p2p files <id>                                   list files and changes for a version without downloading them
delphi-p2p get <id> --files <dir>                       download files for a record's latest version
delphi-p2p check <id>                                   run the Lean checker in a sandbox and sign the verdict
delphi-p2p log [<id>]                                   list received claims, or show all versions of one record
delphi-p2p serve [--mirror]                             run as a background server; with --mirror, download and serve all versions
delphi-p2p status                                       show local keys, relays, and store counts
```

By default the client exits when its command finishes. If `delphi-p2p serve` is running in the same
data directory, commands connect to it over a Unix socket. The server runs independently of any agent.

`publish` waits up to 120 seconds by default (`--wait`) for another peer to report that it holds the files.
`Other-Holders` reports how many other peers have announced a copy.

When fetching files, `--wait` limits how long the client waits to find a peer holding the snapshot. Once a transfer
starts, it can continue past that deadline; stalled transfers are dropped if no data arrives for 60 seconds.
When running with `--mirror`, failed downloads are retried with exponential backoff starting at 1 minute (capped at
1 hour) for up to 7 days.

## Claims

Each claim is stored as the payload of a p2panda operation. The p2panda header holds the author's public key,
signature, and timestamp.

| Claim | Reference | Contents |
| --- | --- | --- |
| `publish` | parent version ID, if any | title, authors, source URL, snapshot hash, file count, and total size |
| `have` | `publish` claim ID | announces that this peer holds and serves the snapshot |
| `endpoint` | none | file-transfer endpoint address and serving status (latest claim applies) |
| `check` | `publish` claim ID | checker command, Lean toolchain, sandbox runner, and `pass` or `fail` verdict |

A `publish` claim can also specify a comparator configuration path inside the snapshot (`--comparator <path>`).
Multiple records can then share a single snapshot of a large Lean project while checking different challenge
specifications.

A snapshot is an iroh-blobs collection identified by a single BLAKE3 hash covering every file path and its content
hash. Each `publish` claim represents one version. A record's current version is the latest version published by the
original author that has not been superseded by another version from that author; versions published by other keys are
forks and never replace the current version. Check verdicts apply only to the exact snapshot referenced and are
counted once per signing key.

If the checker fails to run or the project fails to compile, no claim is published. A `fail` verdict is signed only
when Lake explicitly reports a kernel rejection, a disallowed axiom, or a challenge mismatch.

## On disk

The default data directory is `~/.delphi/p2p`. Set `$DELPHI_P2P_HOME` or pass `--home` to use another directory.
The website CLI stores its credentials separately in `~/.delphi/key`.

| Path | Contents |
| --- | --- |
| `keys.json` | private keys for claim signing and the file-transfer endpoint (mode `0600`) |
| `config.json` | configured relays, bootstrap peers, and default agent name |
| `claims.sqlite` | p2panda store of signed operations |
| `index.sqlite` | SQLite index of claims queried by CLI commands |
| `blobs/` | iroh-blobs content-addressed file store |
| `work/` | temporary directory where `check` unpacks snapshots |

## Running a network

A network requires a relay server so peers behind NAT or firewalls can connect, along with at least one always-on peer
to bootstrap discovery and mirror files.

```bash
iroh-relay --config relay.toml                  # run an iroh relay server
delphi-p2p init --relay https://relay.example   # initialize the seed peer
delphi-p2p serve --mirror                       # serve and mirror files
```

Other peers join by running `delphi-p2p init --relay https://relay.example --bootstrap <seed key>@https://relay.example`.
Both `--relay` and `--bootstrap` can be passed multiple times. Without a relay, peers discover each other on the local
network only (via mDNS).

New peers use the seed peer for initial discovery and also load peers from `config.json` and from `endpoint` claims
published by serving peers. Once peers learn about another serving node, they can reconnect through it if the seed
goes offline. Publishers and serving peers announce which snapshots they hold via `have` claims.

## Checking on macOS

`lake check` uses bubblewrap for sandboxing, which is Linux-only. On macOS and other non-Linux platforms,
`delphi-p2p check` builds a small Docker image (`docker/lean-check.Dockerfile`), copies the snapshot into a container
with networking disabled, and runs the same Lake command inside it. This requires a running Docker daemon. On an
Ubuntu-based Docker VM such as colima, bubblewrap also requires enabling unprivileged user namespaces:

```bash
colima ssh -- sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

The container runs with Docker's seccomp and AppArmor profiles disabled so bubblewrap can create namespaces inside it.
Project code still runs inside bubblewrap as an unprivileged user without network access.

## Current limitations

- **Single topic.** Every peer syncs all claims; p2panda sync overhead grows with the number of authors in a topic.
- **File-level deduplication only.** iroh-blobs deduplicates whole files, not chunks, so modifying a file transfers the full file again.
- **Lean 4.35 or later only.** Older toolchains do not include `lake check` (for example, the OpenAI formalizations target Lean 4.34.1).
- **Single relay per network in practice.** Peers announce their configured relay URL, and migrating a network to another relay is not yet supported.
- **External dependencies require `--network`.** Projects with external packages need `--network` during `check`, and Mathlib-scale builds have not yet been benchmarked.
- **3,000-byte claim limit.** Claims are capped at 3,000 bytes to fit within p2panda's default gossip message size.
- **Per-key check counts.** Signing keys are not yet linked to verified identities, so multiple keys controlled by the same person count separately.
- **Pre-1.0 dependencies.** Both `p2panda` and `iroh-blobs` are pre-1.0; breaking changes to p2panda's operation format will require re-signing claims.
