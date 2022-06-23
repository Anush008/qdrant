use std::collections::HashMap;
use std::fs::{create_dir_all, read_dir, remove_dir_all};
use std::num::NonZeroU32;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::sync::{RwLock, RwLockReadGuard};

use collection::config::{CollectionConfig, CollectionParams};
use collection::operations::config_diff::DiffConfig;
use collection::operations::types::{
    BatchSearchRequest, PointRequest, RecommendRequest, Record, ScrollRequest, ScrollResult,
    SearchRequest, UpdateResult,
};
use collection::operations::CollectionUpdateOperations;
use collection::{ChannelService, Collection, CollectionShardDistribution};
use segment::types::ScoredPoint;

use super::collection_meta_ops::CreateCollectionOperation;
use super::consensus_state;
use crate::content_manager::shard_distribution::ShardDistributionProposal;
use crate::content_manager::{
    alias_mapping::AliasPersistence,
    collection_meta_ops::{
        AliasOperations, ChangeAliasesOperation, CollectionMetaOperations, CreateAlias,
        CreateAliasOperation, CreateCollection, DeleteAlias, DeleteAliasOperation, RenameAlias,
        RenameAliasOperation, UpdateCollection,
    },
    collections_ops::{Checker, Collections},
    errors::StorageError,
};
use crate::types::{PeerAddressById, StorageConfig};
use collection::collection_manager::collection_managers::CollectionSearcher;
use collection::collection_manager::simple_collection_searcher::SimpleCollectionSearcher;
use collection::shard::ShardId;
use collection::PeerId;

const COLLECTIONS_DIR: &str = "collections";

/// The main object of the service. It holds all objects, required for proper functioning.
/// In most cases only one `TableOfContent` is enough for service. It is created only once during
/// the launch of the service.
pub struct TableOfContent {
    collections: Arc<RwLock<Collections>>,
    storage_config: StorageConfig,
    search_runtime: Runtime,
    collection_management_runtime: Runtime,
    alias_persistence: RwLock<AliasPersistence>,
    segment_searcher: Box<dyn CollectionSearcher + Sync + Send>,
    this_peer_id: PeerId,
    channel_service: ChannelService,
}

impl TableOfContent {
    /// PeerId does not change during execution so it is ok to copy it here.
    pub fn new(
        storage_config: &StorageConfig,
        search_runtime: Runtime,
        channel_service: ChannelService,
        this_peer_id: PeerId,
    ) -> Self {
        let collections_path = Path::new(&storage_config.storage_path).join(&COLLECTIONS_DIR);
        let collection_management_runtime = Runtime::new().unwrap();
        create_dir_all(&collections_path).expect("Can't create Collections directory");
        let collection_paths =
            read_dir(&collections_path).expect("Can't read Collections directory");
        let mut collections: HashMap<String, Collection> = Default::default();
        for entry in collection_paths {
            let collection_path = entry
                .expect("Can't access of one of the collection files")
                .path();
            let collection_name = collection_path
                .file_name()
                .expect("Can't resolve a filename of one of the collection files")
                .to_str()
                .expect("A filename of one of the collection files is not a valid UTF-8")
                .to_string();
            log::info!("Loading collection: {}", collection_name);
            let collection = collection_management_runtime.block_on(Collection::load(
                collection_name.clone(),
                &collection_path,
                channel_service.clone(),
            ));

            collections.insert(collection_name, collection);
        }
        let alias_path = Path::new(&storage_config.storage_path).join("aliases");
        let alias_persistence =
            AliasPersistence::open(alias_path).expect("Can't open database by the provided config");
        TableOfContent {
            collections: Arc::new(RwLock::new(collections)),
            storage_config: storage_config.clone(),
            search_runtime,
            alias_persistence: RwLock::new(alias_persistence),
            segment_searcher: Box::new(SimpleCollectionSearcher::new()),
            collection_management_runtime,
            this_peer_id,
            channel_service,
        }
    }

    fn get_collection_path(&self, collection_name: &str) -> PathBuf {
        Path::new(&self.storage_config.storage_path)
            .join(&COLLECTIONS_DIR)
            .join(collection_name)
    }

    pub fn storage_path(&self) -> &str {
        &self.storage_config.storage_path
    }

    async fn create_collection_path(&self, collection_name: &str) -> Result<PathBuf, StorageError> {
        let path = self.get_collection_path(collection_name);

        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|err| StorageError::ServiceError {
                description: format!(
                    "Can't create directory for collection {}. Error: {}",
                    collection_name, err
                ),
            })?;

        Ok(path)
    }

    /// Finds the original name of the collection
    ///
    /// # Arguments
    ///
    /// * `collection_name` - Name of the collection or alias to resolve
    ///
    /// # Result
    ///
    /// If the collection exists - return its name
    /// If alias exists - returns the original collection name
    /// If neither exists - returns [`StorageError`]
    async fn resolve_name(&self, collection_name: &str) -> Result<String, StorageError> {
        let alias_collection_name = self.alias_persistence.read().await.get(collection_name);

        let resolved_name = match alias_collection_name {
            None => collection_name.to_string(),
            Some(resolved_alias) => resolved_alias,
        };
        self.collections
            .read()
            .await
            .validate_collection_exists(&resolved_name)
            .await?;
        Ok(resolved_name)
    }

    async fn create_collection(
        &self,
        collection_name: &str,
        operation: CreateCollection,
        collection_shard_distribution: CollectionShardDistribution,
    ) -> Result<bool, StorageError> {
        let CreateCollection {
            vector_size,
            distance,
            shard_number,
            on_disk_payload,
            hnsw_config: hnsw_config_diff,
            wal_config: wal_config_diff,
            optimizers_config: optimizers_config_diff,
        } = operation;

        self.collections
            .read()
            .await
            .validate_collection_not_exists(collection_name)
            .await?;

        let collection_path = self.create_collection_path(collection_name).await?;

        let collection_params = CollectionParams {
            vector_size,
            distance,
            shard_number: NonZeroU32::new(shard_number.unwrap_or(1)).ok_or(
                StorageError::BadInput {
                    description: "`shard_number` cannot be 0".to_string(),
                },
            )?,
            on_disk_payload: on_disk_payload.unwrap_or(self.storage_config.on_disk_payload),
        };
        let wal_config = match wal_config_diff {
            None => self.storage_config.wal.clone(),
            Some(diff) => diff.update(&self.storage_config.wal)?,
        };

        let optimizers_config = match optimizers_config_diff {
            None => self.storage_config.optimizers.clone(),
            Some(diff) => diff.update(&self.storage_config.optimizers)?,
        };

        let hnsw_config = match hnsw_config_diff {
            None => self.storage_config.hnsw_index,
            Some(diff) => diff.update(&self.storage_config.hnsw_index)?,
        };

        let collection_config = CollectionConfig {
            wal_config,
            params: collection_params,
            optimizer_config: optimizers_config,
            hnsw_config,
        };
        let collection = Collection::new(
            collection_name.to_string(),
            Path::new(&collection_path),
            &collection_config,
            collection_shard_distribution,
            self.channel_service.clone(),
        )
        .await?;

        let mut write_collections = self.collections.write().await;
        write_collections
            .validate_collection_not_exists(collection_name)
            .await?;
        write_collections.insert(collection_name.to_string(), collection);
        Ok(true)
    }

    async fn update_collection(
        &self,
        collection_name: &str,
        operation: UpdateCollection,
    ) -> Result<bool, StorageError> {
        match operation.optimizers_config {
            None => {}
            Some(new_optimizers_config) => {
                let collection = self.get_collection(collection_name).await?;
                collection
                    .update_optimizer_params_from_diff(new_optimizers_config)
                    .await?
            }
        }
        Ok(true)
    }

    async fn delete_collection(&self, collection_name: &str) -> Result<bool, StorageError> {
        if let Some(mut removed) = self.collections.write().await.remove(collection_name) {
            removed.before_drop().await;
            let path = self.get_collection_path(collection_name);
            drop(removed);
            remove_dir_all(path).map_err(|err| StorageError::ServiceError {
                description: format!(
                    "Can't delete collection {}, error: {}",
                    collection_name, err
                ),
            })?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// performs several alias changes in an atomic fashion
    async fn update_aliases(
        &self,
        operation: ChangeAliasesOperation,
    ) -> Result<bool, StorageError> {
        // Lock all collections for alias changes
        // Prevent search on partially switched collections
        let collection_lock = self.collections.write().await;
        let mut alias_lock = self.alias_persistence.write().await;
        for action in operation.actions {
            match action {
                AliasOperations::CreateAlias(CreateAliasOperation {
                    create_alias:
                        CreateAlias {
                            collection_name,
                            alias_name,
                        },
                }) => {
                    collection_lock
                        .validate_collection_exists(&collection_name)
                        .await?;
                    collection_lock
                        .validate_collection_not_exists(&alias_name)
                        .await?;

                    alias_lock.insert(alias_name, collection_name)?;
                }
                AliasOperations::DeleteAlias(DeleteAliasOperation {
                    delete_alias: DeleteAlias { alias_name },
                }) => {
                    alias_lock.remove(&alias_name)?;
                }
                AliasOperations::RenameAlias(RenameAliasOperation {
                    rename_alias:
                        RenameAlias {
                            old_alias_name,
                            new_alias_name,
                        },
                }) => {
                    alias_lock.rename_alias(&old_alias_name, new_alias_name)?;
                }
            };
        }
        Ok(true)
    }

    pub fn perform_collection_meta_op_sync(
        &self,
        operation: CollectionMetaOperations,
    ) -> Result<bool, StorageError> {
        self.collection_management_runtime
            .block_on(self.perform_collection_meta_op(operation))
    }

    pub async fn perform_collection_meta_op(
        &self,
        operation: CollectionMetaOperations,
    ) -> Result<bool, StorageError> {
        match operation {
            CollectionMetaOperations::CreateCollectionDistributed(operation, distribution) => {
                let local = distribution.local_shards_for(self.this_peer_id);
                let remote = distribution.remote_shards_for(self.this_peer_id);
                let collection_shard_distribution =
                    CollectionShardDistribution::Distribution { local, remote };
                self.create_collection(
                    &operation.collection_name,
                    operation.create_collection,
                    collection_shard_distribution,
                )
                .await
            }
            CollectionMetaOperations::CreateCollection(operation) => {
                self.create_collection(
                    &operation.collection_name,
                    operation.create_collection,
                    CollectionShardDistribution::AllLocal,
                )
                .await
            }
            CollectionMetaOperations::UpdateCollection(operation) => {
                self.update_collection(&operation.collection_name, operation.update_collection)
                    .await
            }
            CollectionMetaOperations::DeleteCollection(operation) => {
                self.delete_collection(&operation.0).await
            }
            CollectionMetaOperations::ChangeAliases(operation) => {
                self.update_aliases(operation).await
            }
        }
    }

    pub async fn get_collection<'a>(
        &'a self,
        collection_name: &str,
    ) -> Result<RwLockReadGuard<'a, Collection>, StorageError> {
        let read_collection = self.collections.read().await;
        let real_collection_name = self.resolve_name(collection_name).await?;
        // resolve_name already checked collection existence, unwrap is safe here
        Ok(RwLockReadGuard::map(read_collection, |collection| {
            collection.get(&real_collection_name).unwrap()
        }))
    }

    /// Recommend points using positive and negative example from the request
    ///
    /// # Arguments
    ///
    /// * `collection_name` - for what collection do we recommend
    /// * `request` - [`RecommendRequest`]
    ///
    /// # Result
    ///
    /// Points with recommendation score
    pub async fn recommend(
        &self,
        collection_name: &str,
        request: RecommendRequest,
        shard_selection: Option<ShardId>,
    ) -> Result<Vec<ScoredPoint>, StorageError> {
        let collection = self.get_collection(collection_name).await?;
        collection
            .recommend_by(
                request,
                self.segment_searcher.deref(),
                self.search_runtime.handle(),
                shard_selection,
            )
            .await
            .map_err(|err| err.into())
    }

    /// Search for the closest points using vector similarity with given restrictions defined
    /// in the request
    ///
    /// # Arguments
    ///
    /// * `collection_name` - in what collection do we search
    /// * `request` - [`SearchRequest`]
    /// * `shard_selection` - which local shard to use
    /// # Result
    ///
    /// Points with search score
    pub async fn search(
        &self,
        collection_name: &str,
        request: SearchRequest,
        shard_selection: Option<ShardId>,
    ) -> Result<Vec<ScoredPoint>, StorageError> {
        let collection = self.get_collection(collection_name).await?;
        collection
            .search(
                request,
                self.segment_searcher.as_ref(),
                self.search_runtime.handle(),
                shard_selection,
            )
            .await
            .map_err(|err| err.into())
    }

    /// Search for the closest points using vector similarity with given restrictions defined
    /// in the request
    ///
    /// # Arguments
    ///
    /// * `collection_name` - in what collection do we search
    /// * `request` - [`SearchRequest`]
    /// * `shard_selection` - which local shard to use
    /// # Result
    ///
    /// Points with search score
    pub async fn search_batch(
        &self,
        collection_name: &str,
        request: BatchSearchRequest,
        shard_selection: Option<ShardId>,
    ) -> Result<Vec<Vec<ScoredPoint>>, StorageError> {
        let collection = self.get_collection(collection_name).await?;
        collection
            .batch_search(
                request,
                self.segment_searcher.as_ref(),
                self.search_runtime.handle(),
                shard_selection,
            )
            .await
            .map_err(|err| err.into())
    }

    /// Return specific points by IDs
    ///
    /// # Arguments
    ///
    /// * `collection_name` - select from this collection
    /// * `request` - [`PointRequest`]
    /// * `shard_selection` - which local shard to use
    ///
    /// # Result
    ///
    /// List of points with specified information included
    pub async fn retrieve(
        &self,
        collection_name: &str,
        request: PointRequest,
        shard_selection: Option<ShardId>,
    ) -> Result<Vec<Record>, StorageError> {
        let collection = self.get_collection(collection_name).await?;
        collection
            .retrieve(request, self.segment_searcher.as_ref(), shard_selection)
            .await
            .map_err(|err| err.into())
    }

    /// List of all collections
    pub async fn all_collections(&self) -> Vec<String> {
        self.collections.read().await.keys().cloned().collect()
    }

    /// List of all collections
    pub fn all_collections_sync(&self) -> Vec<String> {
        self.collection_management_runtime
            .block_on(self.collections.read())
            .keys()
            .cloned()
            .collect()
    }

    /// List of all aliases for a given collection
    pub async fn collection_aliases(
        &self,
        collection_name: &str,
    ) -> Result<Vec<String>, StorageError> {
        let result = self
            .alias_persistence
            .read()
            .await
            .collection_aliases(collection_name);
        Ok(result)
    }

    /// Paginate over all stored points with given filtering conditions
    ///
    /// # Arguments
    ///
    /// * `collection_name` - which collection to use
    /// * `request` - [`ScrollRequest`]
    /// * `shard_selection` - which local shard to use
    ///
    /// # Result
    ///
    /// List of points with specified information included
    pub async fn scroll(
        &self,
        collection_name: &str,
        request: ScrollRequest,
        shard_selection: Option<ShardId>,
    ) -> Result<ScrollResult, StorageError> {
        let collection = self.get_collection(collection_name).await?;
        collection
            .scroll_by(request, self.segment_searcher.deref(), shard_selection)
            .await
            .map_err(|err| err.into())
    }

    pub async fn update(
        &self,
        collection_name: &str,
        operation: CollectionUpdateOperations,
        shard_selection: Option<ShardId>,
        wait: bool,
    ) -> Result<UpdateResult, StorageError> {
        let collection = self.get_collection(collection_name).await?;
        let result = match shard_selection {
            Some(shard_selection) => {
                collection
                    .update_from_peer(operation, shard_selection, wait)
                    .await
            }
            None => collection.update_from_client(operation, wait).await,
        };
        result.map_err(|err| err.into())
    }

    fn this_peer_id(&self) -> PeerId {
        self.this_peer_id
    }

    pub fn peer_address_by_id(&self) -> PeerAddressById {
        self.channel_service.id_to_address.read().clone()
    }

    pub fn collections_snapshot_sync(&self) -> consensus_state::CollectionsSnapshot {
        self.collection_management_runtime
            .block_on(self.collections_snapshot())
    }

    pub async fn collections_snapshot(&self) -> consensus_state::CollectionsSnapshot {
        let mut collections: HashMap<collection::CollectionId, collection::State> = HashMap::new();
        for (id, collection) in self.collections.read().await.iter() {
            collections.insert(id.clone(), collection.state(self.this_peer_id()).await);
        }
        consensus_state::CollectionsSnapshot {
            collections,
            aliases: self.alias_persistence.read().await.state().clone(),
        }
    }

    pub fn apply_collections_snapshot(
        &self,
        data: consensus_state::CollectionsSnapshot,
    ) -> Result<(), StorageError> {
        self.collection_management_runtime.block_on(async {
            let mut collections = self.collections.write().await;
            for (id, state) in &data.collections {
                let collection = collections.get_mut(id);
                match collection {
                    // Update state if collection present locally
                    Some(collection) => {
                        if &collection.state(self.this_peer_id()).await != state {
                            collection
                                .apply_state(
                                    state.clone(),
                                    self.this_peer_id(),
                                    &self.get_collection_path(&collection.name()),
                                    self.channel_service.clone(),
                                )
                                .await?;
                        }
                    }
                    // Create collection if not present locally
                    None => {
                        let collection_path = self.create_collection_path(id).await?;
                        let shard_distribution = CollectionShardDistribution::from_shard_to_peer(
                            self.this_peer_id,
                            &state.shard_to_peer,
                        );
                        let collection = Collection::new(
                            id.to_string(),
                            Path::new(&collection_path),
                            &state.config,
                            shard_distribution,
                            self.channel_service.clone(),
                        )
                        .await?;
                        collections.validate_collection_not_exists(id).await?;
                        collections.insert(id.to_string(), collection);
                    }
                }
            }

            // Remove collections that are present locally but are not in the snapshot state
            for collection_name in collections.keys() {
                if !data.collections.contains_key(collection_name) {
                    log::debug!(
                        "Deleting collection {} because it is not part of the consensus snapshot",
                        collection_name
                    );
                    self.delete_collection(collection_name).await?;
                }
            }

            // Apply alias mapping
            self.alias_persistence
                .write()
                .await
                .apply_state(data.aliases)?;
            Ok(())
        })
    }

    pub async fn suggest_shard_distribution(
        &self,
        op: &CreateCollectionOperation,
    ) -> ShardDistributionProposal {
        let shard_number = op.create_collection.shard_number.unwrap_or(1);
        let known_peers: Vec<_> = self
            .channel_service
            .id_to_address
            .read()
            .keys()
            .copied()
            .collect();
        let known_collections = self.collections.read().await;
        let known_shards: Vec<_> = known_collections
            .iter()
            .flat_map(|(_, col)| col.all_shards())
            .collect();
        let shard_distribution = ShardDistributionProposal::new(
            shard_number,
            self.this_peer_id(),
            &known_peers,
            known_shards,
        );
        log::debug!(
            "Suggesting distribution for {} shards for collection '{}' among {} peers {:?}",
            shard_number,
            op.collection_name,
            known_peers.len(),
            shard_distribution.distribution
        );
        shard_distribution
    }
}

// `TableOfContent` should not be dropped from async context.
impl Drop for TableOfContent {
    fn drop(&mut self) {
        self.collection_management_runtime.block_on(async {
            for (_, mut collection) in self.collections.write().await.drain() {
                collection.before_drop().await;
            }
        });
    }
}
