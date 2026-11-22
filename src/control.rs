//! Unix control socket for forwarding CLI commands to a running `delphi-p2p serve` process.
//!
//! Because the SQLite databases and blob store can only be opened by one process at a time,
//! CLI invocations connect to the active server over this socket and print its response.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::node::Node;
use crate::ops::{self, Command};

#[derive(Serialize, Deserialize)]
enum Reply {
    Done(String),
    Failed(String),
}

/// Sends a command to the running `serve` process, or returns `Ok(None)` if none is listening.
pub async fn send(socket: &Path, command: &Command) -> Result<Option<Result<String>>> {
    let Ok(mut stream) = UnixStream::connect(socket).await else {
        return Ok(None);
    };
    stream
        .write_all((serde_json::to_string(command)? + "\n").as_bytes())
        .await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let reply: Reply = serde_json::from_str(&line).context("running server closed connection without replying")?;
    Ok(Some(match reply {
        Reply::Done(text) => Ok(text),
        Reply::Failed(error) => Err(anyhow::anyhow!(error)),
    }))
}

/// Binds the control socket so incoming connections queue until `serve` starts accepting them.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    // Remove any stale socket file left behind after an unclean exit.
    let _ = std::fs::remove_file(socket);
    UnixListener::bind(socket).with_context(|| format!("{}", socket.display()))
}

/// Accepts and handles commands on the control socket until shutdown.
pub async fn serve(listener: UnixListener, node: Arc<Node>) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(err) = answer(stream, &node).await {
                eprintln!("delphi-p2p: control socket: {err}");
            }
        });
    }
}

async fn answer(stream: UnixStream, node: &Node) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let reply = match serde_json::from_str::<Command>(&line) {
        Ok(command) => match ops::run(node, command).await {
            Ok(text) => Reply::Done(text),
            Err(err) => Reply::Failed(format!("{err:#}")),
        },
        Err(err) => Reply::Failed(format!("invalid command: {err}")),
    };
    write
        .write_all((serde_json::to_string(&reply)? + "\n").as_bytes())
        .await?;
    Ok(())
}
