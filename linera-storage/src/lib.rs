// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! This module defines the storage abstractions for individual chains and certificates.

#![deny(clippy::large_futures)]

mod db_storage;

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::{mapref::entry::Entry, DashMap};
use linera_base::{
    crypto::CryptoHash,
    data_types::{
        ApplicationDescription, Blob, ChainDescription, CompressedBytecode, Epoch, TimeDelta,
        Timestamp,
    },
    identifiers::{ApplicationId, BlobId, ChainId, EventId},
    vm::VmRuntime,
};
use linera_chain::{
    types::{ConfirmedBlock, ConfirmedBlockCertificate},
    ChainError, ChainStateView,
};
#[cfg(with_revm)]
use linera_execution::{
    evm::revm::{EvmContractModule, EvmServiceModule},
    EvmRuntime,
};
use linera_execution::{
    BlobState, ExecutionError, ExecutionRuntimeConfig, ExecutionRuntimeContext, UserContractCode,
    UserServiceCode, WasmRuntime,
};
#[cfg(with_wasm_runtime)]
use linera_execution::{WasmContractModule, WasmServiceModule};
use linera_views::{
    context::Context,
    views::{RootView, ViewError},
};
use serde::{Deserialize, Serialize};

#[cfg(with_testing)]
pub use crate::db_storage::TestClock;
pub use crate::db_storage::{ChainStatesFirstAssignment, DbStorage, WallClock};
#[cfg(with_metrics)]
pub use crate::db_storage::{
    READ_CERTIFICATE_COUNTER, READ_CONFIRMED_BLOCK_COUNTER, WRITE_CERTIFICATE_COUNTER,
};

/// The default namespace to be used when none is specified
pub const DEFAULT_NAMESPACE: &str = "table_linera";

/// Communicate with a persistent storage using the "views" abstraction.
#[cfg_attr(not(web), async_trait)]
#[cfg_attr(web, async_trait(?Send))]
pub trait Storage: Sized {
    /// The low-level storage implementation in use by the core protocol (chain workers etc).
    type Context: Context<Extra = ChainRuntimeContext<Self>> + Clone + Send + Sync + 'static;

    /// The clock type being used.
    type Clock: Clock;

    /// The low-level storage implementation in use by the block exporter.
    type BlockExporterContext: Context<Extra = u32> + Clone + Send + Sync + 'static;

    /// Returns the current wall clock time.
    fn clock(&self) -> &Self::Clock;

    /// Loads the view of a chain state.
    ///
    /// # Notes
    ///
    /// Each time this method is called, a new [`ChainStateView`] is created. If there are multiple
    /// instances of the same chain active at any given moment, they will race to access persistent
    /// storage. This can lead to invalid states and data corruption.
    async fn load_chain(&self, id: ChainId) -> Result<ChainStateView<Self::Context>, ViewError>;

    /// Tests the existence of a blob with the given blob ID.
    async fn contains_blob(&self, blob_id: BlobId) -> Result<bool, ViewError>;

    /// Returns what blobs from the input are missing from storage.
    async fn missing_blobs(&self, blob_ids: &[BlobId]) -> Result<Vec<BlobId>, ViewError>;

    /// Tests existence of a blob state with the given blob ID.
    async fn contains_blob_state(&self, blob_id: BlobId) -> Result<bool, ViewError>;

    /// Reads the hashed certificate value with the given hash.
    async fn read_confirmed_block(&self, hash: CryptoHash) -> Result<ConfirmedBlock, ViewError>;

    /// Reads the blob with the given blob ID.
    async fn read_blob(&self, blob_id: BlobId) -> Result<Blob, ViewError>;

    /// Reads the blobs with the given blob IDs.
    async fn read_blobs(&self, blob_ids: &[BlobId]) -> Result<Vec<Option<Blob>>, ViewError>;

    /// Reads the blob state with the given blob ID.
    async fn read_blob_state(&self, blob_id: BlobId) -> Result<BlobState, ViewError>;

    /// Reads the blob states with the given blob IDs.
    async fn read_blob_states(&self, blob_ids: &[BlobId]) -> Result<Vec<BlobState>, ViewError>;

    /// Reads the hashed certificate values in descending order from the given hash.
    async fn read_confirmed_blocks_downward(
        &self,
        from: CryptoHash,
        limit: u32,
    ) -> Result<Vec<ConfirmedBlock>, ViewError>;

    /// Writes the given blob.
    async fn write_blob(&self, blob: &Blob) -> Result<(), ViewError>;

    /// Writes blobs and certificate
    async fn write_blobs_and_certificate(
        &self,
        blobs: &[Blob],
        certificate: &ConfirmedBlockCertificate,
    ) -> Result<(), ViewError>;

    /// Writes the given blob state.
    async fn write_blob_state(
        &self,
        blob_id: BlobId,
        blob_state: &BlobState,
    ) -> Result<(), ViewError>;

    /// Writes the given blobs, but only if they already have a blob state. Returns `true` for the
    /// blobs that were written.
    async fn maybe_write_blobs(&self, blobs: &[Blob]) -> Result<Vec<bool>, ViewError>;

    /// Attempts to write the given blob state. Returns the latest `Epoch` to have used this blob.
    async fn maybe_write_blob_state(
        &self,
        blob_id: BlobId,
        blob_state: BlobState,
    ) -> Result<Epoch, ViewError>;

    /// Attempts to write the given blob state. Returns the latest `Epoch` to have used this blob.
    async fn maybe_write_blob_states(
        &self,
        blob_ids: &[BlobId],
        blob_state: BlobState,
        overwrite: bool,
    ) -> Result<Vec<Epoch>, ViewError>;

    /// Writes several blobs.
    async fn write_blobs(&self, blobs: &[Blob]) -> Result<(), ViewError>;

    /// Tests existence of the certificate with the given hash.
    async fn contains_certificate(&self, hash: CryptoHash) -> Result<bool, ViewError>;

    /// Reads the certificate with the given hash.
    async fn read_certificate(
        &self,
        hash: CryptoHash,
    ) -> Result<ConfirmedBlockCertificate, ViewError>;

    /// Reads a number of certificates
    async fn read_certificates<I: IntoIterator<Item = CryptoHash> + Send>(
        &self,
        hashes: I,
    ) -> Result<Vec<ConfirmedBlockCertificate>, ViewError>;

    /// Reads the event with the given ID.
    async fn read_event(&self, id: EventId) -> Result<Vec<u8>, ViewError>;

    /// Tests existence of the event with the given ID.
    async fn contains_event(&self, id: EventId) -> Result<bool, ViewError>;

    /// Writes a vector of events.
    async fn write_events(
        &self,
        events: impl IntoIterator<Item = (EventId, Vec<u8>)> + Send,
    ) -> Result<(), ViewError>;

    /// Reads the network description.
    async fn read_network_description(&self) -> Result<Option<NetworkDescription>, ViewError>;

    /// Writes the network description.
    async fn write_network_description(
        &self,
        information: &NetworkDescription,
    ) -> Result<(), ViewError>;

    /// Initializes a chain in a simple way (used for testing and to create a genesis state).
    ///
    /// # Notes
    ///
    /// This method creates a new [`ChainStateView`] instance. If there are multiple instances of
    /// the same chain active at any given moment, they will race to access persistent storage.
    /// This can lead to invalid states and data corruption.
    async fn create_chain(&self, description: ChainDescription) -> Result<(), ChainError>
    where
        ChainRuntimeContext<Self>: ExecutionRuntimeContext,
    {
        let id = description.id();
        // Store the description blob.
        self.write_blob(&Blob::new_chain_description(&description))
            .await?;
        let mut chain = self.load_chain(id).await?;
        assert!(!chain.is_active(), "Attempting to create a chain twice");
        let current_time = self.clock().current_time();
        chain.ensure_is_active(current_time).await?;
        chain.save().await?;
        Ok(())
    }

    /// Selects the WebAssembly runtime to use for applications (if any).
    fn wasm_runtime(&self) -> Option<WasmRuntime>;

    /// Creates a [`UserContractCode`] instance using the bytecode in storage referenced
    /// by the `application_description`.
    async fn load_contract(
        &self,
        application_description: &ApplicationDescription,
    ) -> Result<UserContractCode, ExecutionError> {
        let contract_bytecode_blob_id = application_description.contract_bytecode_blob_id();
        let contract_blob = self.read_blob(contract_bytecode_blob_id).await?;
        let compressed_contract_bytecode = CompressedBytecode {
            compressed_bytes: contract_blob.into_bytes().to_vec(),
        };
        #[cfg_attr(not(any(with_wasm_runtime, with_revm)), allow(unused_variables))]
        let contract_bytecode =
            linera_base::task::Blocking::<linera_base::task::NoInput, _>::spawn(
                move |_| async move { compressed_contract_bytecode.decompress() },
            )
            .await
            .join()
            .await?;
        match application_description.module_id.vm_runtime {
            VmRuntime::Wasm => {
                cfg_if::cfg_if! {
                    if #[cfg(with_wasm_runtime)] {
                        let Some(wasm_runtime) = self.wasm_runtime() else {
                            panic!("A Wasm runtime is required to load user applications.");
                        };
                        Ok(WasmContractModule::new(contract_bytecode, wasm_runtime)
                           .await?
                           .into())
                    } else {
                        panic!(
                            "A Wasm runtime is required to load user applications. \
                             Please enable the `wasmer` or the `wasmtime` feature flags \
                             when compiling `linera-storage`."
                        );
                    }
                }
            }
            VmRuntime::Evm => {
                cfg_if::cfg_if! {
                    if #[cfg(with_revm)] {
                        let evm_runtime = EvmRuntime::Revm;
                        Ok(EvmContractModule::new(contract_bytecode, evm_runtime)
                           .await?
                           .into())
                    } else {
                        panic!(
                            "An Evm runtime is required to load user applications. \
                             Please enable the `revm` feature flag \
                             when compiling `linera-storage`."
                        );
                    }
                }
            }
        }
    }

    /// Creates a [`linera-sdk::UserContract`] instance using the bytecode in storage referenced
    /// by the `application_description`.
    async fn load_service(
        &self,
        application_description: &ApplicationDescription,
    ) -> Result<UserServiceCode, ExecutionError> {
        let service_bytecode_blob_id = application_description.service_bytecode_blob_id();
        let service_blob = self.read_blob(service_bytecode_blob_id).await?;
        let compressed_service_bytecode = CompressedBytecode {
            compressed_bytes: service_blob.into_bytes().to_vec(),
        };
        #[cfg_attr(not(any(with_wasm_runtime, with_revm)), allow(unused_variables))]
        let service_bytecode = linera_base::task::Blocking::<linera_base::task::NoInput, _>::spawn(
            move |_| async move { compressed_service_bytecode.decompress() },
        )
        .await
        .join()
        .await?;
        match application_description.module_id.vm_runtime {
            VmRuntime::Wasm => {
                cfg_if::cfg_if! {
                    if #[cfg(with_wasm_runtime)] {
                        let Some(wasm_runtime) = self.wasm_runtime() else {
                            panic!("A Wasm runtime is required to load user applications.");
                        };
                        Ok(WasmServiceModule::new(service_bytecode, wasm_runtime)
                           .await?
                           .into())
                    } else {
                        panic!(
                            "A Wasm runtime is required to load user applications. \
                             Please enable the `wasmer` or the `wasmtime` feature flags \
                             when compiling `linera-storage`."
                        );
                    }
                }
            }
            VmRuntime::Evm => {
                cfg_if::cfg_if! {
                    if #[cfg(with_revm)] {
                        let evm_runtime = EvmRuntime::Revm;
                        Ok(EvmServiceModule::new(service_bytecode, evm_runtime)
                           .await?
                           .into())
                    } else {
                        panic!(
                            "An Evm runtime is required to load user applications. \
                             Please enable the `revm` feature flag \
                             when compiling `linera-storage`."
                        );
                    }
                }
            }
        }
    }

    async fn block_exporter_context(
        &self,
        block_exporter_id: u32,
    ) -> Result<Self::BlockExporterContext, ViewError>;
}

/// A description of the current Linera network to be stored in every node's database.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(with_testing, derive(Eq, PartialEq))]
pub struct NetworkDescription {
    pub name: String,
    pub genesis_config_hash: CryptoHash,
    pub genesis_timestamp: Timestamp,
}

/// An implementation of `ExecutionRuntimeContext` suitable for the core protocol.
#[derive(Clone)]
pub struct ChainRuntimeContext<S> {
    storage: S,
    chain_id: ChainId,
    execution_runtime_config: ExecutionRuntimeConfig,
    user_contracts: Arc<DashMap<ApplicationId, UserContractCode>>,
    user_services: Arc<DashMap<ApplicationId, UserServiceCode>>,
}

#[cfg_attr(not(web), async_trait)]
#[cfg_attr(web, async_trait(?Send))]
impl<S> ExecutionRuntimeContext for ChainRuntimeContext<S>
where
    S: Storage + Send + Sync,
{
    fn chain_id(&self) -> ChainId {
        self.chain_id
    }

    fn execution_runtime_config(&self) -> linera_execution::ExecutionRuntimeConfig {
        self.execution_runtime_config
    }

    fn user_contracts(&self) -> &Arc<DashMap<ApplicationId, UserContractCode>> {
        &self.user_contracts
    }

    fn user_services(&self) -> &Arc<DashMap<ApplicationId, UserServiceCode>> {
        &self.user_services
    }

    async fn get_user_contract(
        &self,
        description: &ApplicationDescription,
    ) -> Result<UserContractCode, ExecutionError> {
        match self.user_contracts.entry(description.into()) {
            Entry::Occupied(entry) => Ok(entry.get().clone()),
            Entry::Vacant(entry) => {
                let contract = self.storage.load_contract(description).await?;
                entry.insert(contract.clone());
                Ok(contract)
            }
        }
    }

    async fn get_user_service(
        &self,
        description: &ApplicationDescription,
    ) -> Result<UserServiceCode, ExecutionError> {
        match self.user_services.entry(description.into()) {
            Entry::Occupied(entry) => Ok(entry.get().clone()),
            Entry::Vacant(entry) => {
                let service = self.storage.load_service(description).await?;
                entry.insert(service.clone());
                Ok(service)
            }
        }
    }

    async fn get_blob(&self, blob_id: BlobId) -> Result<Blob, ViewError> {
        self.storage.read_blob(blob_id).await
    }

    async fn get_event(&self, event_id: EventId) -> Result<Vec<u8>, ViewError> {
        self.storage.read_event(event_id).await
    }

    async fn contains_blob(&self, blob_id: BlobId) -> Result<bool, ViewError> {
        self.storage.contains_blob(blob_id).await
    }

    async fn contains_event(&self, event_id: EventId) -> Result<bool, ViewError> {
        self.storage.contains_event(event_id).await
    }

    #[cfg(with_testing)]
    async fn add_blobs(
        &self,
        blobs: impl IntoIterator<Item = Blob> + Send,
    ) -> Result<(), ViewError> {
        let blobs = Vec::from_iter(blobs);
        self.storage.write_blobs(&blobs).await
    }

    #[cfg(with_testing)]
    async fn add_events(
        &self,
        events: impl IntoIterator<Item = (EventId, Vec<u8>)> + Send,
    ) -> Result<(), ViewError> {
        self.storage.write_events(events).await
    }
}

/// A clock that can be used to get the current `Timestamp`.
#[cfg_attr(not(web), async_trait)]
#[cfg_attr(web, async_trait(?Send))]
pub trait Clock {
    fn current_time(&self) -> Timestamp;

    async fn sleep(&self, delta: TimeDelta);

    async fn sleep_until(&self, timestamp: Timestamp);
}
