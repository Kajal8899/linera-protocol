// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::large_futures)]

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{anyhow, bail, ensure, Result};
use async_trait::async_trait;
use futures::{FutureExt as _, SinkExt, StreamExt};
use linera_base::listen_for_shutdown_signals;
use linera_client::config::{GenesisConfig, ValidatorServerConfig};
use linera_core::{node::NodeError, JoinSetExt as _};
use linera_rpc::{
    config::{
        NetworkProtocol, ShardConfig, ValidatorInternalNetworkPreConfig,
        ValidatorPublicNetworkPreConfig,
    },
    simple::{MessageHandler, TransportProtocol},
    RpcMessage,
};
use linera_sdk::linera_base_types::Blob;
#[cfg(with_metrics)]
use linera_service::prometheus_server;
use linera_service::{
    storage::{Runnable, StorageConfigNamespace},
    util,
};
use linera_storage::Storage;
use linera_views::{lru_caching::StorageCacheConfig, store::CommonStoreConfig};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument};

mod grpc;
use grpc::GrpcProxy;

/// Options for running the proxy.
#[derive(clap::Parser, Debug, Clone)]
#[command(
    name = "Linera Proxy",
    about = "A proxy to redirect incoming requests to Linera Server shards",
    version = linera_version::VersionInfo::default_clap_str(),
)]
pub struct ProxyOptions {
    /// Path to server configuration.
    config_path: PathBuf,

    /// Timeout for sending queries (ms)
    #[arg(long = "send-timeout-ms",
          default_value = "4000",
          value_parser = util::parse_millis,
          env = "LINERA_PROXY_SEND_TIMEOUT")]
    send_timeout: Duration,

    /// Timeout for receiving responses (ms)
    #[arg(long = "recv-timeout-ms",
          default_value = "4000",
          value_parser = util::parse_millis,
          env = "LINERA_PROXY_RECV_TIMEOUT")]
    recv_timeout: Duration,

    /// The number of Tokio worker threads to use.
    #[arg(long, env = "LINERA_PROXY_TOKIO_THREADS")]
    tokio_threads: Option<usize>,

    /// The number of Tokio blocking threads to use.
    #[arg(long, env = "LINERA_PROXY_TOKIO_BLOCKING_THREADS")]
    tokio_blocking_threads: Option<usize>,

    /// Storage configuration for the blockchain history, chain states and binary blobs.
    #[arg(long = "storage")]
    storage_config: StorageConfigNamespace,

    /// The maximal number of simultaneous queries to the database
    #[arg(long)]
    max_concurrent_queries: Option<usize>,

    /// The maximal number of stream queries to the database
    #[arg(long, default_value = "10")]
    max_stream_queries: usize,

    /// The maximal memory used in the storage cache.
    #[arg(long, default_value = "10000000")]
    pub max_cache_size: usize,

    /// The maximal size of an entry in the storage cache.
    #[arg(long, default_value = "1000000")]
    pub max_entry_size: usize,

    /// The maximal number of entries in the storage cache.
    #[arg(long, default_value = "1000")]
    pub max_cache_entries: usize,

    /// Path to the file describing the initial user chains (aka genesis state)
    #[arg(long = "genesis")]
    genesis_config_path: PathBuf,
}

/// A Linera Proxy, either gRPC or over 'Simple Transport', meaning TCP or UDP.
/// The proxy can be configured to have a gRPC ingress and egress, or a combination
/// of TCP / UDP ingress and egress.
enum Proxy<S>
where
    S: Storage + Clone + Send + Sync + 'static,
{
    Simple(Box<SimpleProxy<S>>),
    Grpc(GrpcProxy<S>),
}

struct ProxyContext {
    config: ValidatorServerConfig,
    send_timeout: Duration,
    recv_timeout: Duration,
}

impl ProxyContext {
    pub fn from_options(options: &ProxyOptions) -> Result<Self> {
        let config = util::read_json(&options.config_path)?;
        Ok(Self {
            config,
            send_timeout: options.send_timeout,
            recv_timeout: options.recv_timeout,
        })
    }
}

#[async_trait]
impl Runnable for ProxyContext {
    type Output = Result<(), anyhow::Error>;

    async fn run<S>(self, storage: S) -> Result<(), anyhow::Error>
    where
        S: Storage + Clone + Send + Sync + 'static,
    {
        let shutdown_notifier = CancellationToken::new();
        tokio::spawn(listen_for_shutdown_signals(shutdown_notifier.clone()));
        let proxy = Proxy::from_context(self, storage)?;
        match proxy {
            Proxy::Simple(simple_proxy) => simple_proxy.run(shutdown_notifier).await,
            Proxy::Grpc(grpc_proxy) => grpc_proxy.run(shutdown_notifier).await,
        }
    }
}

impl<S> Proxy<S>
where
    S: Storage + Clone + Send + Sync + 'static,
{
    /// Constructs and configures the [`Proxy`] given [`ProxyContext`].
    fn from_context(context: ProxyContext, storage: S) -> Result<Self> {
        let internal_protocol = context.config.internal_network.protocol;
        let external_protocol = context.config.validator.network.protocol;
        let proxy = match (internal_protocol, external_protocol) {
            (NetworkProtocol::Grpc { .. }, NetworkProtocol::Grpc(tls)) => {
                Self::Grpc(GrpcProxy::new(
                    context.config.validator.network,
                    context.config.internal_network,
                    context.send_timeout,
                    context.recv_timeout,
                    tls,
                    storage,
                ))
            }
            (
                NetworkProtocol::Simple(internal_transport),
                NetworkProtocol::Simple(public_transport),
            ) => Self::Simple(Box::new(SimpleProxy {
                internal_config: context
                    .config
                    .internal_network
                    .clone_with_protocol(internal_transport),
                public_config: context
                    .config
                    .validator
                    .network
                    .clone_with_protocol(public_transport),
                send_timeout: context.send_timeout,
                recv_timeout: context.recv_timeout,
                storage,
            })),
            _ => {
                bail!(
                    "network protocol mismatch: cannot have {} and {} ",
                    internal_protocol,
                    external_protocol,
                );
            }
        };

        Ok(proxy)
    }
}

#[derive(Debug, Clone)]
pub struct SimpleProxy<S>
where
    S: Storage + Clone + Send + Sync + 'static,
{
    public_config: ValidatorPublicNetworkPreConfig<TransportProtocol>,
    internal_config: ValidatorInternalNetworkPreConfig<TransportProtocol>,
    send_timeout: Duration,
    recv_timeout: Duration,
    storage: S,
}

#[async_trait]
impl<S> MessageHandler for SimpleProxy<S>
where
    S: Storage + Clone + Send + Sync + 'static,
{
    #[instrument(skip_all, fields(chain_id = ?message.target_chain_id()))]
    async fn handle_message(&mut self, message: RpcMessage) -> Option<RpcMessage> {
        if message.is_local_message() {
            match self.try_local_message(message).await {
                Ok(maybe_response) => {
                    return maybe_response;
                }
                Err(error) => {
                    error!(error = %error, "Failed to handle local message");
                    return None;
                }
            }
        }

        let Some(chain_id) = message.target_chain_id() else {
            error!("Can't proxy message without chain ID");
            return None;
        };

        let shard = self.internal_config.get_shard_for(chain_id).clone();
        let protocol = self.internal_config.protocol;

        match Self::try_proxy_message(
            message,
            shard.clone(),
            protocol,
            self.send_timeout,
            self.recv_timeout,
        )
        .await
        {
            Ok(maybe_response) => maybe_response,
            Err(error) => {
                error!(error = %error, "Failed to proxy message to {}", shard.address());
                None
            }
        }
    }
}

impl<S> SimpleProxy<S>
where
    S: Storage + Clone + Send + Sync + 'static,
{
    #[instrument(name = "SimpleProxy::run", skip_all, fields(port = self.public_config.port, metrics_port = self.internal_config.metrics_port), err)]
    async fn run(self, shutdown_signal: CancellationToken) -> Result<()> {
        info!("Starting simple server");
        let mut join_set = JoinSet::new();
        let address = self.get_listen_address(self.public_config.port);

        #[cfg(with_metrics)]
        Self::start_metrics(
            self.get_listen_address(self.internal_config.metrics_port),
            shutdown_signal.clone(),
        );

        self.public_config
            .protocol
            .spawn_server(address, self, shutdown_signal, &mut join_set)
            .join()
            .await?;

        join_set.await_all_tasks().await;

        Ok(())
    }

    #[cfg(with_metrics)]
    pub fn start_metrics(address: SocketAddr, shutdown_signal: CancellationToken) {
        prometheus_server::start_metrics(address, shutdown_signal)
    }

    fn get_listen_address(&self, port: u16) -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], port))
    }

    async fn try_proxy_message(
        message: RpcMessage,
        shard: ShardConfig,
        protocol: TransportProtocol,
        send_timeout: Duration,
        recv_timeout: Duration,
    ) -> Result<Option<RpcMessage>> {
        let mut connection = protocol.connect((shard.host, shard.port)).await?;
        linera_base::time::timer::timeout(send_timeout, connection.send(message)).await??;
        let message = linera_base::time::timer::timeout(recv_timeout, connection.next())
            .await?
            .transpose()?;
        Ok(message)
    }

    async fn try_local_message(&self, message: RpcMessage) -> Result<Option<RpcMessage>> {
        use RpcMessage::*;

        match message {
            VersionInfoQuery => {
                // We assume each shard is running the same version as the proxy
                Ok(Some(RpcMessage::VersionInfoResponse(
                    linera_version::VersionInfo::default().into(),
                )))
            }
            NetworkDescriptionQuery => {
                let description = self
                    .storage
                    .read_network_description()
                    .await?
                    .ok_or(anyhow!("Cannot find network description in the database"))?;
                Ok(Some(RpcMessage::NetworkDescriptionResponse(Box::new(
                    description,
                ))))
            }
            UploadBlob(content) => {
                let blob = Blob::new(*content);
                let id = blob.id();
                ensure!(
                    self.storage.maybe_write_blobs(&[blob]).await?[0],
                    "Blob not found"
                );
                Ok(Some(RpcMessage::UploadBlobResponse(Box::new(id))))
            }
            DownloadBlob(blob_id) => {
                let content = self.storage.read_blob(*blob_id).await?.into_content();
                Ok(Some(RpcMessage::DownloadBlobResponse(Box::new(content))))
            }
            DownloadConfirmedBlock(hash) => Ok(Some(RpcMessage::DownloadConfirmedBlockResponse(
                Box::new(self.storage.read_confirmed_block(*hash).await?),
            ))),
            DownloadCertificates(hashes) => {
                let certificates = self.storage.read_certificates(hashes).await?;
                Ok(Some(RpcMessage::DownloadCertificatesResponse(certificates)))
            }
            BlobLastUsedBy(blob_id) => Ok(Some(RpcMessage::BlobLastUsedByResponse(Box::new(
                self.storage.read_blob_state(*blob_id).await?.last_used_by,
            )))),
            MissingBlobIds(blob_ids) => Ok(Some(RpcMessage::MissingBlobIdsResponse(
                self.storage.missing_blobs(&blob_ids).await?,
            ))),
            BlockProposal(_)
            | LiteCertificate(_)
            | TimeoutCertificate(_)
            | ConfirmedCertificate(_)
            | ValidatedCertificate(_)
            | ChainInfoQuery(_)
            | CrossChainRequest(_)
            | Vote(_)
            | Error(_)
            | ChainInfoResponse(_)
            | VersionInfoResponse(_)
            | NetworkDescriptionResponse(_)
            | DownloadBlobResponse(_)
            | DownloadPendingBlob(_)
            | DownloadPendingBlobResponse(_)
            | HandlePendingBlob(_)
            | BlobLastUsedByResponse(_)
            | MissingBlobIdsResponse(_)
            | DownloadConfirmedBlockResponse(_)
            | DownloadCertificatesResponse(_)
            | UploadBlobResponse(_) => Err(anyhow::Error::from(NodeError::UnexpectedMessage)),
        }
    }
}

fn main() -> Result<()> {
    let options = <ProxyOptions as clap::Parser>::parse();
    let server_config: ValidatorServerConfig =
        util::read_json(&options.config_path).expect("Fail to read server config");
    let public_key = &server_config.validator.public_key;

    linera_base::tracing::init(&format!("validator-{public_key}-proxy"));

    let mut runtime = if options.tokio_threads == Some(1) {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();

        if let Some(threads) = options.tokio_threads {
            builder.worker_threads(threads);
        }

        builder
    };

    if let Some(blocking_threads) = options.tokio_blocking_threads {
        runtime.max_blocking_threads(blocking_threads);
    }

    runtime.enable_all().build()?.block_on(options.run())
}

impl ProxyOptions {
    async fn run(&self) -> Result<()> {
        let storage_cache_config = StorageCacheConfig {
            max_cache_size: self.max_cache_size,
            max_entry_size: self.max_entry_size,
            max_cache_entries: self.max_cache_entries,
        };
        let common_config = CommonStoreConfig {
            max_concurrent_queries: self.max_concurrent_queries,
            max_stream_queries: self.max_stream_queries,
            storage_cache_config,
        };
        let genesis_config: GenesisConfig = util::read_json(&self.genesis_config_path)?;
        let store_config = self.storage_config.add_common_config(common_config).await?;
        store_config
            .run_with_storage(&genesis_config, None, ProxyContext::from_options(self)?)
            .boxed()
            .await?
    }
}
