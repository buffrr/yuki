use bitcoin::{BlockHash, Network};
use clap::{Parser};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use anyhow::anyhow;
use jsonrpsee::server::{Server, ServerHandle};
use tokio::sync::{broadcast};
use crate::rpc::server::{RpcServer, RpcServerImpl};
use crate::{Client, HeaderCheckpoint, Log, NodeBuilder, PeerStoreSizeConfig, Requester};

const DEFAULT_RPC_PORT: u16 = 8225;

#[derive(Parser, Debug, Serialize, Deserialize)]
#[command(args_override_self = true, author, version, about, long_about = None)]
pub struct Args {
    #[arg(long, env = "YUKI_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Network to use
    #[arg(long, env = "YUKI_CHAIN", default_value = "bitcoin")]
    chain: Network,

    /// Bind to given address to listen for JSON-RPC connections.
    #[arg(long, default_values = ["127.0.0.1"], env = "YUKI_RPC_BIND")]
    rpc_bind: Vec<String>,

    /// Listen for JSON-RPC connections on <port>
    #[arg(long, default_value_t = DEFAULT_RPC_PORT, env = "YUKI_RPC_PORT")]
    rpc_port: u16,

    /// Specify an optional checkpoint to start syncing from in the format <block-hash>:<block-height>
    #[arg(long, env = "YUKI_CHECKPOINT")]
    checkpoint: Option<String>,

    /// The point at which blocks begin to download to act as a pruned Bitcoin node
    /// in the format <block-hash>:<block-height>
    #[arg(long, env = "YUKI_PRUNE_POINT")]
    prune_point: Option<String>,
}

pub async fn run(args: Vec<String>, shutdown: broadcast::Sender<()>) -> anyhow::Result<()> {
    let args = Args::try_parse_from(args)?;

    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    // Map CLI network to Kyoto network
    let network: Network = args.chain.into();

    // Use provided data directory or default
    let data_dir = args.data_dir.unwrap_or_else(|| PathBuf::from("./data"));

    // Create checkpoint
    let checkpoint = parse_checkpoint(args.checkpoint.as_deref(), network)?;
    let prune_point = parse_checkpoint(args.prune_point.as_deref(), network)?;

    // Build the node
    let builder = NodeBuilder::new(network);
    let (node, client) = builder
        .anchor_checkpoint(checkpoint)
        .prune_point(prune_point)
        .peer_db_size(PeerStoreSizeConfig::Limit(256))
        .required_peers(8)
        .data_dir(data_dir)
        .halt_filter_download()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build node: {}", e))?;

    // Create a channel to signal shutdown
    let mut shutdown_rx = shutdown.subscribe();

    // Spawn node task
    let mut node_handle = tokio::spawn(
        async move { Arc::new(node).run().await
    });

    let Client {
        requester,
        mut log_rx,
        mut warn_rx,
        mut event_rx,
        ..
    } = client;

    // Start RPC servers
    let rpc_handles = start_rpc_listeners(args.rpc_bind, args.rpc_port, requester.clone()).await?;

    // Main event loop
    loop {
        tokio::select! {
            // Handle node events
            event = event_rx.recv() => {
                if event.is_none() {
                    tracing::info!("Event channel closed, shutting down");
                    break;
                }
            }
            // Handle logs
            log = log_rx.recv() => {
                if let Some(log) = log {
                    match log {
                        Log::Debug(d) => tracing::info!("{d}"),
                        Log::StateChange(node_state) => tracing::info!("{node_state}"),
                        Log::ConnectionsMet => tracing::info!("All required connections met"),
                        _ => (),
                    }
                } else {
                    tracing::info!("Log channel closed, shutting down");
                    break;
                }
            }
            // Handle warnings
            warn = warn_rx.recv() => {
                if let Some(warn) = warn {
                    tracing::warn!("{warn}");
                } else {
                    tracing::info!("Warn channel closed, shutting down");
                    break;
                }
            }
            // Handle node task completion
            result = &mut node_handle => {
                match result {
                    Ok(_) => tracing::info!("Node task completed"),
                    Err(e) => tracing::error!("Node task failed: {}", e),
                }
                break;
            }
            // Handle shutdown signal
            _ = shutdown_rx.recv() => {
                tracing::info!("Shutdown signal received");
                break;
            }
        }
    }

    // Shutdown
    tracing::info!("Shutting down...");
    _ = shutdown.send(());

    // Stop RPC servers
    for handle in rpc_handles {
        handle.stop()?;
    }

    // Shutdown node
    requester.shutdown().await?;

    // Ensure node task is complete
    if !node_handle.is_finished() {
        node_handle.await??;
    }

    tracing::info!("Shutdown complete");
    Ok(())
}

pub fn parse_checkpoint(input: Option<&str>, network: Network) -> anyhow::Result<HeaderCheckpoint> {
    match input {
        Some(s) => {
            let (hash_str, height_str) = s
                .split_once(':')
                .ok_or_else(|| anyhow!("Invalid checkpoint format: expected <hash>:<height>"))?;

            let hash = BlockHash::from_str(hash_str)
                .map_err(|e| anyhow!("Invalid block hash '{}': {}", hash_str, e))?;

            let height = height_str
                .parse::<u32>()
                .map_err(|e| anyhow!("Invalid block height '{}': {}", height_str, e))?;

            Ok(HeaderCheckpoint { height, hash })
        }
        None => {
            match network {
                Network::Bitcoin => {
                    let hash = BlockHash::from_str("0000000000000000000152dd9d6059126e4e4dbc2732246bef2b8496ef1d971d")
                        .map_err(|e| anyhow!("Invalid hardcoded block hash: {}", e))?;
                    Ok(HeaderCheckpoint {
                        height: 870_000,
                        hash,
                    })
                }
                Network::Testnet4 => {
                    let hash = BlockHash::from_str("000000000000000c1a1fad82b0e133f4772802b6dff7a95990580ae2e15c634f")
                        .map_err(|e| anyhow!("Invalid hardcoded block hash: {}", e))?;
                    Ok(HeaderCheckpoint {
                        height: 40_000,
                        hash,
                    })
                }
                _ => Ok(HeaderCheckpoint::most_recent(network)),
            }
        }
    }
}


async fn start_rpc_listeners(
    bind_addresses: Vec<String>,
    port: u16,
    requester: Requester,
) -> anyhow::Result<Vec<ServerHandle>> {
    let mut handles = Vec::new();
    let rpc_server = RpcServerImpl {
        requester: requester.clone(),
    };

    for addr in bind_addresses {
        let bind_addr = format!("{}:{}", addr, port);
        let server = Server::builder()
            .build(&bind_addr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to bind RPC server on {}: {}", bind_addr, e))?;

        let server_handle = server.start(rpc_server.clone().into_rpc());
        handles.push(server_handle);
        tracing::info!("Started RPC server on {}", bind_addr);
    }

    Ok(handles)
}
