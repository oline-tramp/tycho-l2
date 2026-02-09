use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::future::BoxFuture;
use proof_api_l2::api::ApiConfig;
use proof_api_l2::storage::{ProofStorage, ProofStorageConfig};
use proof_api_util::api::Api;
use serde::{Deserialize, Serialize};
use tycho_block_util::archive::ArchiveData;
use tycho_block_util::block::BlockStuff;
use tycho_core::block_strider::{
    BlockProviderExt, BlockSubscriber, BlockSubscriberContext, MetricsSubscriber,
};
use tycho_core::node::{LightNodeConfig, LightNodeContext, NodeBaseConfig, NodeBootArgs};
use tycho_core::storage::{BlockConnection, BlockHandle, CoreStorage, NewBlockMeta};
use tycho_types::dict::Dict;
use tycho_types::models::BlockId;
use tycho_util::cli;
use tycho_util::cli::config::ThreadPoolConfig;
use tycho_util::cli::logger::LoggerConfig;
use tycho_util::cli::metrics::MetricsConfig;
use tycho_util::config::PartialConfig;
use tycho_util::futures::JoinTask;

/// Run the Tycho node.
#[derive(Parser)]
pub struct Cmd {
    #[clap(flatten)]
    args: tycho_core::node::CmdRunArgs,
}

impl Cmd {
    pub fn run(self) -> Result<()> {
        self.args.init_config_or_run_light_node(async move |ctx| {
            let LightNodeContext::<NodeConfig> {
                node,
                config,
                boot_args,
                ..
            } = ctx;

            // Open proofs storage.
            let proofs =
                ProofStorage::new(node.core_storage.context().root_dir(), config.proof_storage)
                    .await
                    .context("failed to create proof storage")?;
            tracing::info!("created proofs storage");

            // Bind API.
            let api = Api::bind(
                config.api.listen_addr,
                proof_api_l2::api::build_api(&config.api, proofs.clone()),
            )
            .await
            .context("failed to bind API service")?;
            tracing::info!("created api");

            // Sync node.
            let init_block_id = node
                .init_ext(NodeBootArgs {
                    ignore_states: true,
                    ..boot_args
                })
                .await?;

            // Init proofs storage.
            proofs
                .init(&node.core_storage, &init_block_id)
                .await
                .context("failed to init proofs storage")?;

            // Start API
            let api_fut = JoinTask::new(api.serve());

            // Build strider.
            let archive_block_provider = node.build_archive_block_provider();
            let storage_block_provider = node.build_storage_block_provider();
            let blockchain_block_provider = node
                .build_blockchain_block_provider()
                .with_fallback(archive_block_provider.clone());

            let block_strider = node.build_strider(
                archive_block_provider.chain((blockchain_block_provider, storage_block_provider)),
                (
                    LightSubscriber {
                        storage: node.core_storage.clone(),
                        proofs,
                    },
                    MetricsSubscriber,
                ),
            );

            // Run block strider
            tracing::info!("block strider started");
            tokio::select! {
                res = block_strider.run() => res?,
                res = api_fut => res?
            }
            tracing::info!("block strider finished");

            // Done
            Ok(())
        })
    }
}

pub struct LightSubscriber {
    storage: CoreStorage,
    proofs: ProofStorage,
}

impl LightSubscriber {
    async fn get_block_handle(
        &self,
        mc_block_id: &BlockId,
        block: &BlockStuff,
        archive_data: &ArchiveData,
    ) -> Result<BlockHandle> {
        let block_storage = self.storage.block_storage();

        let info = block.load_info()?;
        let res = block_storage
            .store_block_data(block, archive_data, NewBlockMeta {
                is_key_block: info.key_block,
                gen_utime: info.gen_utime,
                ref_by_mc_seqno: mc_block_id.seqno,
            })
            .await?;

        Ok(res.handle)
    }

    async fn prepare_block_impl(&self, cx: &BlockSubscriberContext) -> Result<BlockHandle> {
        tracing::info!(
            mc_block_id = %cx.mc_block_id.as_short_id(),
            id = %cx.block.id(),
            "preparing block",
        );

        // Load handle
        let handle = self
            .get_block_handle(&cx.mc_block_id, &cx.block, &cx.archive_data)
            .await?;

        let (prev_id, prev_id_alt) = cx
            .block
            .construct_prev_id()
            .context("failed to construct prev id")?;

        // Update block connections
        let block_handles = self.storage.block_handle_storage();
        let connections = self.storage.block_connection_storage();

        let block_id = cx.block.id();

        let prev_handle = block_handles.load_handle(&prev_id);

        match prev_id_alt {
            None => {
                if let Some(handle) = prev_handle {
                    let direction = if block_id.shard != prev_id.shard
                        && prev_id.shard.split().unwrap().1 == block_id.shard
                    {
                        // Special case for the right child after split
                        BlockConnection::Next2
                    } else {
                        BlockConnection::Next1
                    };
                    connections.store_connection(&handle, direction, block_id);
                }
                connections.store_connection(&handle, BlockConnection::Prev1, &prev_id);
            }
            Some(ref prev_id_alt) => {
                if let Some(handle) = prev_handle {
                    connections.store_connection(&handle, BlockConnection::Next1, block_id);
                }
                if let Some(handle) = block_handles.load_handle(prev_id_alt) {
                    connections.store_connection(&handle, BlockConnection::Next1, block_id);
                }
                connections.store_connection(&handle, BlockConnection::Prev1, &prev_id);
                connections.store_connection(&handle, BlockConnection::Prev2, prev_id_alt);
            }
        }

        // Get block signatures for masterchain block.
        let signatures = if cx.block.id().is_masterchain() {
            let proof = self
                .storage
                .block_storage()
                .load_block_proof(&handle)
                .await?;
            let Some(signatures) = &proof.as_ref().signatures else {
                anyhow::bail!("masterchain block proof without signatures: {block_id}");
            };
            signatures.signatures.clone()
        } else {
            Dict::new()
        };

        // Store proof.
        self.proofs
            .store_block(cx.block.clone(), signatures, cx.mc_block_id.seqno)
            .await?;

        Ok(handle)
    }

    async fn handle_block_impl(
        &self,
        cx: &BlockSubscriberContext,
        handle: BlockHandle,
    ) -> Result<()> {
        tracing::info!(
            block_id = %cx.block.id(),
            mc_block_id = %cx.mc_block_id,
            "handling block"
        );

        // Save block to archive.
        if self.storage.config().archives_gc.is_some() {
            tracing::debug!(block_id = %handle.id(), "saving block into archive");
            self.storage
                .block_storage()
                .move_into_archive(&handle, cx.mc_is_key_block)
                .await?;
        }

        // Update proofs storage snapshot on masterchain blocks.
        if cx.block.id().is_masterchain() {
            self.proofs.update_snapshot();
        }

        // Update current vset on key blocks.
        if cx.is_key_block {
            let custom = cx.block.load_custom()?;
            let config = custom.config.as_ref().context("key block without config")?;

            let current_vset = config
                .get_current_validator_set()
                .context("failed to get current validator set")
                .map(Arc::new)?;

            self.proofs.set_current_vset(current_vset);
        }

        // Done
        Ok(())
    }
}

impl BlockSubscriber for LightSubscriber {
    type Prepared = BlockHandle;

    type PrepareBlockFut<'a> = BoxFuture<'a, Result<Self::Prepared>>;
    type HandleBlockFut<'a> = BoxFuture<'a, Result<()>>;

    fn prepare_block<'a>(&'a self, cx: &'a BlockSubscriberContext) -> Self::PrepareBlockFut<'a> {
        Box::pin(self.prepare_block_impl(cx))
    }

    fn handle_block<'a>(
        &'a self,
        cx: &'a BlockSubscriberContext,
        handle: Self::Prepared,
    ) -> Self::HandleBlockFut<'a> {
        Box::pin(self.handle_block_impl(cx, handle))
    }
}

#[allow(unused)]
#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialConfig)]
#[serde(default)]
struct NodeConfig {
    #[partial]
    #[serde(flatten)]
    base: NodeBaseConfig,
    #[important]
    threads: ThreadPoolConfig,
    #[important]
    logger_config: LoggerConfig,
    #[important]
    metrics: Option<MetricsConfig>,
    #[important]
    api: ApiConfig,
    #[important]
    proof_storage: ProofStorageConfig,
}

impl LightNodeConfig for NodeConfig {
    fn base(&self) -> &NodeBaseConfig {
        &self.base
    }

    fn threads(&self) -> &cli::config::ThreadPoolConfig {
        &self.threads
    }

    fn metrics(&self) -> Option<&cli::metrics::MetricsConfig> {
        self.metrics.as_ref()
    }

    fn logger(&self) -> Option<&cli::logger::LoggerConfig> {
        Some(&self.logger_config)
    }
}
