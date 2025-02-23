#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations, missing_docs, rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;
pub mod fetcher;
pub mod kv;
pub mod preimage;
pub mod server;
pub mod util;

pub use cli::{init_tracing_subscriber, HostCli};
use fetcher::Fetcher;
use server::PreimageServer;

use anyhow::{anyhow, Result};
use command_fds::{CommandFdExt, FdMapping};
use futures::FutureExt;
use kona_common::FileDescriptor;
use kona_derive::online::{OnlineBeaconClient, OnlineBlobProvider};
use kona_preimage::{HintReader, OracleServer, PipeHandle};
use kv::KeyValueStore;
use std::{
    io::{stderr, stdin, stdout},
    os::fd::{AsFd, AsRawFd},
    panic::AssertUnwindSafe,
    sync::Arc,
};
use tokio::{process::Command, sync::RwLock, task};
use tracing::{error, info};
use util::Pipe;

/// Starts the [PreimageServer] in the primary thread. In this mode, the host program has been
/// invoked by the Fault Proof VM and the client program is running in the parent process.
pub async fn start_server(cfg: HostCli) -> Result<()> {
    let (preimage_pipe, hint_pipe) = (
        PipeHandle::new(FileDescriptor::PreimageRead, FileDescriptor::PreimageWrite),
        PipeHandle::new(FileDescriptor::HintRead, FileDescriptor::HintWrite),
    );
    let oracle_server = OracleServer::new(preimage_pipe);
    let hint_reader = HintReader::new(hint_pipe);

    let kv_store = cfg.construct_kv_store();

    let fetcher = if !cfg.is_offline() {
        let beacon_client = OnlineBeaconClient::new_http(
            cfg.l1_beacon_address.clone().expect("Beacon API URL must be set"),
        );
        let mut blob_provider = OnlineBlobProvider::new(beacon_client, None, None);
        blob_provider
            .load_configs()
            .await
            .map_err(|e| anyhow!("Failed to load blob provider configuration: {e}"))?;
        let l1_provider = util::http_provider(&cfg.l1_node_address.expect("Provider must be set"));
        let l2_provider = util::http_provider(&cfg.l2_node_address.expect("Provider must be set"));
        Some(Arc::new(RwLock::new(Fetcher::new(
            kv_store.clone(),
            l1_provider,
            blob_provider,
            l2_provider,
            cfg.l2_head,
        ))))
    } else {
        None
    };

    // Start the server and wait for it to complete.
    info!("Starting preimage server.");
    let server = PreimageServer::new(oracle_server, hint_reader, kv_store, fetcher);
    server.start().await?;
    info!("Preimage server has exited.");

    Ok(())
}

/// Starts the [PreimageServer] and the client program in separate threads. The client program is
/// ran natively in this mode.
pub async fn start_server_and_native_client(cfg: HostCli) -> Result<()> {
    let hint_pipe = util::bidirectional_pipe()?;
    let preimage_pipe = util::bidirectional_pipe()?;

    let kv_store = cfg.construct_kv_store();

    let fetcher = if !cfg.is_offline() {
        let beacon_client = OnlineBeaconClient::new_http(
            cfg.l1_beacon_address.clone().expect("Beacon API URL must be set"),
        );
        let mut blob_provider = OnlineBlobProvider::new(beacon_client, None, None);
        blob_provider
            .load_configs()
            .await
            .map_err(|e| anyhow!("Failed to load blob provider configuration: {e}"))?;
        let l1_provider =
            util::http_provider(cfg.l1_node_address.as_ref().expect("Provider must be set"));
        let l2_provider =
            util::http_provider(cfg.l2_node_address.as_ref().expect("Provider must be set"));
        Some(Arc::new(RwLock::new(Fetcher::new(
            kv_store.clone(),
            l1_provider,
            blob_provider,
            l2_provider,
            cfg.l2_head,
        ))))
    } else {
        None
    };

    // Create the server and start it.
    let server_task = task::spawn(start_native_preimage_server(
        kv_store,
        fetcher,
        hint_pipe.host,
        preimage_pipe.host,
    ));

    // Start the client program in a separate child process.
    let program_task =
        task::spawn(start_native_client_program(cfg, hint_pipe.client, preimage_pipe.client));

    // Execute both tasks and wait for them to complete.
    info!("Starting preimage server and client program.");
    tokio::select!(
        r = util::flatten_join_result(server_task) => r?,
        r = util::flatten_join_result(program_task) => r?
    );
    info!(target: "kona_host", "Preimage server and client program have exited.");

    Ok(())
}

/// Starts the preimage server in a separate thread. The client program is ran natively in this
/// mode.
pub async fn start_native_preimage_server<KV>(
    kv_store: Arc<RwLock<KV>>,
    fetcher: Option<Arc<RwLock<Fetcher<KV>>>>,
    hint_pipe: Pipe,
    preimage_pipe: Pipe,
) -> Result<()>
where
    KV: KeyValueStore + Send + Sync + ?Sized + 'static,
{
    let hint_reader = HintReader::new(PipeHandle::new(
        FileDescriptor::Wildcard(hint_pipe.read.as_raw_fd() as usize),
        FileDescriptor::Wildcard(hint_pipe.write.as_raw_fd() as usize),
    ));
    let oracle_server = OracleServer::new(PipeHandle::new(
        FileDescriptor::Wildcard(preimage_pipe.read.as_raw_fd() as usize),
        FileDescriptor::Wildcard(preimage_pipe.write.as_raw_fd() as usize),
    ));

    let server = PreimageServer::new(oracle_server, hint_reader, kv_store, fetcher);
    AssertUnwindSafe(server.start())
        .catch_unwind()
        .await
        .map_err(|_| {
            error!(target: "preimage_server", "Preimage server panicked");
            anyhow!("Preimage server panicked")
        })?
        .map_err(|e| {
            error!(target: "preimage_server", "Preimage server exited with an error");
            anyhow!("Preimage server exited with an error: {:?}", e)
        })?;

    Ok(())
}

/// Starts the client program in a separate child process. The client program is ran natively in
/// this mode.
///
/// ## Takes
/// - `cfg`: The host configuration.
/// - `files`: The files that are used to communicate with the native client.
/// - `tx`: The sender to signal the preimage server to exit.
/// - `rx`: The receiver to wait for the preimage server to exit.
///
/// ## Returns
/// - `Ok(())` if the client program exits successfully.
/// - `Err(_)` if the client program exits with a non-zero status.
pub async fn start_native_client_program(
    cfg: HostCli,
    hint_pipe: Pipe,
    preimage_pipe: Pipe,
) -> Result<()> {
    // Map the file descriptors to the standard streams and the preimage oracle and hint
    // reader's special file descriptors.
    let mut command =
        Command::new(cfg.exec.ok_or_else(|| anyhow!("No client program binary path specified."))?);
    command
        .fd_mappings(vec![
            FdMapping {
                parent_fd: stdin().as_fd().try_clone_to_owned().unwrap(),
                child_fd: FileDescriptor::StdIn.into(),
            },
            FdMapping {
                parent_fd: stdout().as_fd().try_clone_to_owned().unwrap(),
                child_fd: FileDescriptor::StdOut.into(),
            },
            FdMapping {
                parent_fd: stderr().as_fd().try_clone_to_owned().unwrap(),
                child_fd: FileDescriptor::StdErr.into(),
            },
            FdMapping {
                parent_fd: hint_pipe.read.into(),
                child_fd: FileDescriptor::HintRead.into(),
            },
            FdMapping {
                parent_fd: hint_pipe.write.into(),
                child_fd: FileDescriptor::HintWrite.into(),
            },
            FdMapping {
                parent_fd: preimage_pipe.read.into(),
                child_fd: FileDescriptor::PreimageRead.into(),
            },
            FdMapping {
                parent_fd: preimage_pipe.write.into(),
                child_fd: FileDescriptor::PreimageWrite.into(),
            },
        ])
        .expect("No errors may occur when mapping file descriptors.");

    let status = command
        .status()
        .await
        .map_err(|e| {
            error!(target: "client_program", "Failed to execute client program: {:?}", e);
            anyhow!("Failed to execute client program: {:?}", e)
        })?
        .success();

    if !status {
        error!(target: "client_program", "Client program exited with a non-zero status.");
        return Err(anyhow!("Client program exited with a non-zero status."));
    }

    Ok(())
}
