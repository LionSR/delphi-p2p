# delphi-p2p

Use `delphi-p2p` to share research files and check Lean proofs. Peers fetch files by hash and
exchange signed claims about which versions they hold or have checked. They can do this independently of delphi.science.

[p2panda](https://github.com/p2panda/p2panda) 0.8 handles peer discovery and claim synchronization.
[iroh-blobs](https://github.com/n0-computer/iroh-blobs) 0.105 handles file transfers.
For Lean projects, the client runs `lake check` or `lake comparator` in a sandbox (Lean 4.35 or later).

The claim format is in `src/claim.rs`, the local index in `src/index.rs`, the commands in `src/ops.rs`,
and the checker in `src/check.rs`.

## Install

With Rust 1.96 or later on Linux or macOS, run

```bash
git clone https://github.com/LionSR/delphi-p2p.git
cd delphi-p2p
cargo install --locked --path .
delphi-p2p --help
```

To join an existing network, ask its operator for the relay URL and bootstrap key, then run the init command below.

## Build and test

```bash
cargo build                 # Rust 1.96 or later
cargo test                  # unit tests
RELAY_BIN=/path/to/iroh-relay tests/e2e.sh   # a relay, a seed and three clients on this machine
```

`tests/e2e.sh` needs Lean (elan) and an `iroh-relay` binary (`cargo install iroh-relay --features server`). It runs
the checker unconfined on its own small projects; `RUNNER=docker tests/e2e.sh` runs it in the sandbox.

## Commands

```
delphi-p2p init --relay <URL> --bootstrap <key>@<URL>   create keys and settings
delphi-p2p publish <dir> --title <t>                    a new record, or a new version of the one <dir> came from
delphi-p2p files <id>                                   list a version's files and what changed, without fetching them
delphi-p2p get <id> --files <dir>                       fetch the files of a record's newest version
delphi-p2p check <id>                                   re-run the Lean checker and sign the result
delphi-p2p log [<id>]                                   claims as they arrived, or the versions of one record
delphi-p2p serve [--mirror]                             stay up and serve files; with --mirror, fetch everything
delphi-p2p status
```

By default the client exits when its command finishes. If `delphi-p2p serve` is running in the same
data directory, commands connect to it over a Unix socket. The server runs independently of any agent.

`publish` waits for another peer to report that it holds the files. The default wait is 120 seconds.
`Other-Holders` reports how many other peers have announced a copy.

When fetching files, `--wait` limits how long the client looks for a holder. Once a transfer starts,
it can continue past that deadline. The client drops a transfer after a minute without data.
A mirror retries a failed fetch after one minute, then doubles the interval up to one hour.
It stops retrying after a week.

## Claims

A claim is the body of one p2panda operation. p2panda's header carries the author's key, the signature and the time.

| Claim | Reference | Contents |
| --- | --- | --- |
| `publish` | the version it follows, if any | title, authors, source, the snapshot's hash, file count and size |
| `have` | a `publish` | this client holds that snapshot and serves it |
| `endpoint` | nothing | file endpoint address and availability (latest claim applies) |
| `check` | a `publish` | the checker, the toolchain, how it was confined, `pass` or `fail` |

A `publish` claim may also name a comparator configuration inside the snapshot (`--comparator <path>`). Many records
can then share one snapshot of a large Lean project and differ only in the challenge used for each check.

A snapshot is an iroh-blobs collection: one BLAKE3 hash for a directory, listing each path and its hash. A version is
a `publish` claim. The current version of a record is the newest one by the record's first author that none of that
author's versions follows; a version by another key is a fork and never replaces it. A check counts for the exact
snapshot it names, once per key.

A check that could not run, or a project that did not build, leaves no claim. `fail` is signed only when Lake printed
that the kernel, the axioms or the comparison with the challenge rejected the project.

## On disk

The default data directory is `~/.delphi/p2p`. Set `$DELPHI_P2P_HOME` or pass `--home` to use another directory.
The website CLI stores its credentials separately in `~/.delphi/key`.

| Path | Contents |
| --- | --- |
| `keys.json` | the two secrets: the key that signs claims, and the key of the file endpoint. Mode 600. |
| `config.json` | relays, bootstrap clients, the agent's name |
| `claims.sqlite` | p2panda's store of signed operations |
| `index.sqlite` | the claims as a table the commands query |
| `blobs/` | the iroh-blobs file store |
| `work/` | where `check` unpacks a snapshot |

## Running a network

A network needs one relay that clients behind home routers can meet through, and it is better with a client that is
always on.

```bash
iroh-relay --config relay.toml                      # the relay: iroh's own program
delphi-p2p init --relay https://relay.example           # the seed
delphi-p2p serve --mirror
```

Everyone else runs `delphi-p2p init --relay https://relay.example --bootstrap <seed key>@https://relay.example`. More
than one `--relay` and `--bootstrap` may be given. Without a relay, clients find each other on the local network
only.

New clients use the seed for peer discovery. They also look for peers in their settings and in
`endpoint` claims from clients running `delphi-p2p serve`. Once clients know about a second server,
they can reconnect through it if the seed goes offline. The publisher and serving clients announce
which files they hold.

## Checking on a Mac

`lake check` confines the project with bubblewrap, which is Linux only. Elsewhere `delphi-p2p check` builds a small
image (`docker/lean-check.Dockerfile`), copies the snapshot into a container with the network off, and runs the same
command there. It needs a running Docker. On an Ubuntu-based Docker machine such as colima, bubblewrap also needs:

```bash
colima ssh -- sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

The container runs with Docker's seccomp and AppArmor profiles off, because bubblewrap needs to create namespaces
inside it. The project's code still runs inside bubblewrap, as an unprivileged user, with no network.

## Limits of this version

- **One topic.** Every client syncs every claim. p2panda's sync cost grows with the number of authors in a topic.
- **A changed file is sent whole.** iroh-blobs deduplicates by file, not by chunk.
- **Lean 4.35 and later only.** Older toolchains have no `lake check`. The OpenAI formalizations are on 4.34.1.
- **One relay per network in practice.** Clients announce the relay they use; nothing yet moves a network to another.
- **A project with dependencies needs `--network`** while it is checked, and a Mathlib-sized build has not been timed.
- **Claims are at most 3000 bytes**, to fit p2panda's default gossip message.
- **Checks are counted per key.** Nothing yet ties a key to a person, so one person with two keys counts twice.
- **Both libraries are before 1.0.** A change of p2panda's operation format means signing the claims again.
