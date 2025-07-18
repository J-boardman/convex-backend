use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    iter,
    ops::Bound as StdBound,
};

use common::{
    bootstrap_model::index::{
        database_index::{
            DatabaseIndexState,
            DeveloperDatabaseIndexConfig,
            IndexedFields,
        },
        text_index::DeveloperTextIndexConfig,
        DeveloperIndexConfig,
        IndexConfig,
        TabletIndexMetadata,
        INDEX_TABLE,
    },
    comparators::{
        tuple::two::TupleKey,
        AsComparator,
    },
    document::{
        PackedDocument,
        ParseDocument,
        ParsedDocument,
        ResolvedDocument,
    },
    index::{
        IndexKey,
        IndexKeyBytes,
    },
    query::FilterValue as SearchFilterValue,
    types::{
        DatabaseIndexUpdate,
        DatabaseIndexValue,
        GenericIndexName,
        IndexDescriptor,
        IndexId,
        IndexName,
        PersistenceVersion,
        TabletIndexName,
        INDEX_BY_CREATION_TIME_DESCRIPTOR,
        INDEX_BY_ID_DESCRIPTOR,
    },
};
use errors::ErrorMetadata;
use imbl::{
    OrdMap,
    OrdSet,
};
use itertools::Itertools;
use value::{
    heap_size::{
        HeapSize,
        WithHeapSize,
    },
    ConvexString,
    ConvexValue,
    FieldPath,
    InternalId,
    ResolvedDocumentId,
    TableMapping,
    TableNamespace,
    TableNumber,
    TabletId,
};

/// [`IndexRegistry`] maintains the metadata for indexes, indicating
/// which indexes exist in the system and which are ready to use. It is a
/// derived view of the `_index` system table,
///
/// This data structure *must* match a transaction's view of the underlying
/// table.
///
/// New indexes are registered via [`TransactionState::add_index_metadata`],
///
/// Index names are expected to be unique, except that database indexes can have
/// up to two entries per unique name, one pending and one enabled.
///
/// Enabled indexes are available to be queried and used by applications.
/// Pending indexes are backfilling or backfilled but not yet enabled indexes
/// that should not be used by applications.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexRegistry {
    index_table: TabletId,
    index_table_number: TableNumber,
    // Indexes that are enabled and ready to be queried against.
    enabled_indexes: OrdMap<TabletIndexName, Index>,
    // Indexes that are not yet enabled for queries, typically backfilling or waiting to be
    // committed.
    pending_indexes: OrdMap<TabletIndexName, Index>,
    indexes_by_table: OrdSet<(TabletId, IndexDescriptor)>,

    persistence_version: PersistenceVersion,
}

impl IndexRegistry {
    pub fn persistence_version(&self) -> PersistenceVersion {
        self.persistence_version
    }

    pub fn set_persistence_version(&mut self, persistence_version: PersistenceVersion) {
        self.persistence_version = persistence_version
    }

    pub fn index_table(&self) -> TabletId {
        self.index_table
    }

    pub fn index_table_number(&self) -> TableNumber {
        self.index_table_number
    }

    /// Fill out all of our index metadata given the latest version of each
    /// document in the `_index` table. After initializing each index, mark
    /// all of them as completed since we'll be streaming in all non
    /// `_index` documents later.
    #[fastrace::trace]
    pub fn bootstrap<'a, Doc: ParseDocument<TabletIndexMetadata>>(
        table_mapping: &TableMapping,
        index_documents: impl Iterator<Item = Doc>,
        persistence_version: PersistenceVersion,
    ) -> anyhow::Result<Self> {
        let index_table = table_mapping
            .namespace(TableNamespace::Global)
            .name_to_tablet()(INDEX_TABLE.clone())?;
        let index_table_number = table_mapping.tablet_number(index_table)?;
        let mut index = Self {
            index_table,
            index_table_number,
            enabled_indexes: OrdMap::new(),
            pending_indexes: OrdMap::new(),
            indexes_by_table: OrdSet::new(),
            persistence_version,
        };

        let meta_index_name = GenericIndexName::by_id(index_table);
        let mut meta_index = None;
        let mut regular_indexes = vec![];

        for document in index_documents {
            let metadata = document.parse()?;
            anyhow::ensure!(metadata.id().tablet_id == index_table);
            if metadata.name == meta_index_name {
                anyhow::ensure!(meta_index.is_none());
                meta_index = Some(metadata);
            } else {
                regular_indexes.push(metadata);
            }
        }

        let meta_index = meta_index
            .ok_or_else(|| anyhow::anyhow!("Missing `by_id` index for {}", *INDEX_TABLE))?;

        // First insert the `_index` table scan index.
        index.insert(Index::new(meta_index.id().internal_id(), meta_index));

        // Then insert the rest of the indexes.
        for metadata in regular_indexes {
            index.insert(Index::new(metadata.id().internal_id(), metadata));
        }

        Ok(index)
    }

    // Verifies and applies and update.
    pub fn update(
        &mut self,
        deletion: Option<&ResolvedDocument>,
        insertion: Option<&ResolvedDocument>,
    ) -> anyhow::Result<()> {
        self.verify_update(deletion, insertion)?;
        self.apply_verified_update(deletion, insertion);
        Ok(())
    }

    /// Returns the index keys for `document` for all the registered indexes on
    /// its table.
    ///
    /// N.B.: if `D` is a `ResolvedDocument` the returned keys are `IndexKey`s,
    /// but if it's a `PackedDocument` then this function returns
    /// `IndexKeyBytes` directly.
    pub(crate) fn index_keys<'a, D: IndexedDocument>(
        &'a self,
        document: &'a D,
    ) -> impl Iterator<Item = (&'a Index, D::IndexKey)> + 'a {
        iter::from_coroutine(
            #[coroutine]
            move || {
                for index in self.indexes_by_table(document.id().tablet_id) {
                    // Only yield fields from database indexes.
                    if let IndexConfig::Database {
                        developer_config: DeveloperDatabaseIndexConfig { fields },
                        on_disk_state: _,
                    } = &index.metadata.config
                    {
                        yield (
                            index,
                            document.index_key_bytes(&fields[..], self.persistence_version()),
                        );
                    }
                }
            },
        )
    }

    pub fn index_updates<'a>(
        &'a self,
        deletion: Option<&'a ResolvedDocument>,
        insertion: Option<&'a ResolvedDocument>,
    ) -> Vec<DatabaseIndexUpdate> {
        let mut updates = BTreeMap::new();
        if let Some(old_document) = deletion {
            for (index, index_key) in self.index_keys(old_document) {
                updates.insert(
                    (index.id(), index_key.clone()),
                    DatabaseIndexUpdate {
                        index_id: index.id(),
                        key: index_key,
                        value: DatabaseIndexValue::Deleted,
                        is_system_index: index.name().descriptor().is_reserved(),
                    },
                );
            }
        }
        if let Some(new_document) = insertion {
            for (index, index_key) in self.index_keys(new_document) {
                updates.insert(
                    (index.id(), index_key.clone()),
                    DatabaseIndexUpdate {
                        index_id: index.id(),
                        key: index_key,
                        value: DatabaseIndexValue::NonClustered(new_document.id()),
                        is_system_index: index.name().descriptor().is_reserved(),
                    },
                );
            }
        }
        updates.into_values().collect()
    }

    pub fn document_index_keys(&self, document: PackedDocument) -> DocumentIndexKeys {
        let map: BTreeMap<_, _> = self
            .indexes_by_table(document.id().tablet_id)
            .flat_map(|index| {
                let key = match &index.metadata.config {
                    IndexConfig::Database {
                        developer_config: DeveloperDatabaseIndexConfig { fields },
                        ..
                    } => Some(DocumentIndexKeyValue::Standard(
                        document.index_key_bytes(&fields[..], self.persistence_version()),
                    )),
                    IndexConfig::Text {
                        developer_config:
                            DeveloperTextIndexConfig {
                                search_field,
                                filter_fields,
                            },
                        ..
                    } => {
                        let filter_values = filter_fields
                            .iter()
                            .map(|field| {
                                let value = document.value().get_path(field);
                                let bytes = SearchFilterValue::from_search_value(value.as_ref());
                                (field.clone(), bytes)
                            })
                            .collect();

                        let search_field_value = match document.value().get_path(search_field) {
                            Some(ConvexValue::String(string)) => Some(string.clone()),
                            _ => None,
                        };

                        Some(DocumentIndexKeyValue::Search(SearchIndexKeyValue {
                            filter_values,
                            search_field: search_field.clone(),
                            search_field_value,
                        }))
                    },
                    IndexConfig::Vector { .. } => None,
                };

                key.map(|key| {
                    let name = index.metadata().name.clone();
                    (name, key)
                })
            })
            .collect();

        DocumentIndexKeys(map.into())
    }

    // Verifies if an update is valid.
    fn verify_update(
        &self,
        old_document: Option<&ResolvedDocument>,
        new_document: Option<&ResolvedDocument>,
    ) -> anyhow::Result<()> {
        // Checks performed when updating a document.
        if let (Some(old_document), Some(new_document)) = (&old_document, &new_document) {
            anyhow::ensure!(old_document.id() == new_document.id());
            anyhow::ensure!(old_document.table() == new_document.table());
            if old_document.id().tablet_id == self.index_table() {
                let old_metadata = TabletIndexMetadata::from_document((*old_document).clone())?;
                let new_metadata = TabletIndexMetadata::from_document((*new_document).clone())?;
                anyhow::ensure!(
                    old_metadata.name.table() == new_metadata.name.table(),
                    "Can't change indexed table"
                );
                if old_metadata.name.is_by_id_or_creation_time() {
                    anyhow::ensure!(
                        old_metadata.name == new_metadata.name,
                        "Can't rename system defined index {}",
                        old_metadata.name
                    );
                }
                anyhow::ensure!(
                    DeveloperIndexConfig::from(old_metadata.config.clone())
                        == DeveloperIndexConfig::from(new_metadata.config.clone()),
                    "Can't modify developer index config for existing indexes {}",
                    old_metadata.name
                );
            }
        }
        // Checks performed when updating or removing a document.
        if let Some(old_document) = old_document {
            if old_document.id().tablet_id == self.index_table() {
                let metadata = TabletIndexMetadata::from_document(old_document.clone())?;
                let index_name = metadata.name.clone();
                if !self.enabled_indexes.contains_key(&index_name)
                    && !self.pending_indexes.contains_key(&index_name)
                {
                    anyhow::bail!("Updating nonexistent index {}", metadata.name);
                }
            }
            let table_key = (&old_document.id().tablet_id, &*INDEX_BY_ID_DESCRIPTOR);
            if !self.indexes_by_table.contains(table_key.as_comparator()) {
                anyhow::bail!("Removing document that doesn't exist in index");
            }
        }
        // Checks performed when updating or adding a document.
        if let Some(new_document) = new_document {
            let tablet_id = new_document.id().tablet_id;
            anyhow::ensure!(
                self.enabled_indexes
                    .contains_key(&GenericIndexName::by_id(tablet_id)),
                "Missing `by_id` index for table {}",
                tablet_id,
            );
            if tablet_id == self.index_table() {
                let metadata = TabletIndexMetadata::from_document(new_document.clone())?;

                // Only the "by_id" index is allowed on the index table itself. The IndexWorker
                // cannot backfill the `_index` table itself, as it loads all index records into
                // memory first when bootstrapping. After loading these records at the latest
                // snapshot, it doesn't make sense to retraverse the `_index` table
                // historically.
                if metadata.name.table() == &self.index_table {
                    anyhow::ensure!(metadata.name.is_by_id());
                }

                if metadata.name.is_by_id() {
                    if let IndexConfig::Database { on_disk_state, .. } = &metadata.config {
                        anyhow::ensure!(
                            *on_disk_state == DatabaseIndexState::Enabled,
                            "All `by_id` indexes should be enabled: {:?}",
                            metadata
                        );
                    } else {
                        anyhow::bail!("`by_id` index must be a database index")
                    }
                }

                // An index cannot be created if another index exists with the same name
                // and same state. The existing index of the same name and state must be deleted
                // first. Note indexes can be edited, e.g. to change state from
                // Backfilling to Enabled.
                let enabled_index = self.enabled_indexes.get(&metadata.name);
                let pending_index = self.pending_indexes.get(&metadata.name);
                Self::verify_index_state(enabled_index, pending_index, &metadata)?;
            }
        }
        Ok(())
    }

    fn verify_index_state(
        enabled_index: Option<&Index>,
        pending_index: Option<&Index>,
        metadata: &ParsedDocument<TabletIndexMetadata>,
    ) -> anyhow::Result<()> {
        if metadata.config.is_enabled() {
            anyhow::ensure!(
                Self::existing_is_none_or_equal(enabled_index, metadata),
                "Cannot create a second enabled index with name {}, \ncurrent: {enabled_index:?}, \
                 \nnew: {metadata:?}",
                metadata.name,
            );
        } else {
            anyhow::ensure!(
                Self::existing_is_none_or_equal(pending_index, metadata),
                "Cannot create a second pending index with name {}, \n current: {pending_index:?} \
                 \nnew: {metadata:?}",
                metadata.name,
            )
        }
        Ok(())
    }

    fn existing_is_none_or_equal(
        existing: Option<&Index>,
        updated: &ParsedDocument<TabletIndexMetadata>,
    ) -> bool {
        existing.is_none() || existing.unwrap().id() == updated.id().internal_id()
    }

    // Applies an already verified update. Should call verify_update beforehand
    // or the function might panic. Returns if the index registry has been
    // modified.
    pub fn apply_verified_update(
        &mut self,
        deletion: Option<&ResolvedDocument>,
        insertion: Option<&ResolvedDocument>,
    ) -> bool {
        let mut modified = false;
        if let Some(old_document) = deletion {
            if old_document.id().tablet_id == self.index_table() {
                let index = TabletIndexMetadata::from_document(old_document.clone()).unwrap();
                self.remove(&index);
                modified = true;
            }
        }
        if let Some(new_document) = insertion {
            // The default index should exist for a table before adding
            // any documents.
            let table_id = new_document.id().tablet_id;
            if table_id == self.index_table() {
                let metadata = TabletIndexMetadata::from_document(new_document.clone()).unwrap();
                let index = Index::new(metadata.id().internal_id(), metadata.clone());
                self.insert(index);
                modified = true;
            }
        }

        modified
    }

    pub fn all_tables_with_indexes(&self) -> Vec<TabletId> {
        self.all_indexes()
            .map(|index| *index.name.table())
            .sorted()
            .dedup()
            .collect()
    }

    pub fn all_text_indexes(&self) -> Vec<ParsedDocument<TabletIndexMetadata>> {
        self.all_indexes()
            .filter(|index| index.is_text_index())
            .cloned()
            .collect()
    }

    pub fn all_vector_indexes(&self) -> Vec<ParsedDocument<TabletIndexMetadata>> {
        self.all_indexes()
            .filter(|index| index.is_vector_index())
            .cloned()
            .collect()
    }

    pub fn all_search_and_vector_indexes(&self) -> Vec<ParsedDocument<TabletIndexMetadata>> {
        self.all_indexes()
            .filter(|index| index.is_text_index() || index.is_vector_index())
            .cloned()
            .collect()
    }

    pub fn enabled_index_by_index_id(&self, index_id: &InternalId) -> Option<&Index> {
        self.enabled_indexes
            .values()
            .find(|index| *index_id == index.id)
    }

    pub fn all_indexes(&self) -> impl Iterator<Item = &ParsedDocument<TabletIndexMetadata>> {
        self.enabled_indexes
            .values()
            .chain(self.pending_indexes.values())
            .map(|index| index.metadata())
    }

    pub fn all_database_index_configs(
        &self,
    ) -> BTreeMap<IndexId, (TabletIndexName, IndexedFields)> {
        self.all_indexes()
            .filter_map(|index| {
                let index_id = index.id().internal_id();
                let index_name = index.name.clone();
                match &index.config {
                    IndexConfig::Database {
                        developer_config, ..
                    } => Some((index_id, (index_name, developer_config.fields.clone()))),
                    IndexConfig::Text { .. } | IndexConfig::Vector { .. } => None,
                }
            })
            .collect()
    }

    pub fn all_enabled_indexes(&self) -> Vec<ParsedDocument<TabletIndexMetadata>> {
        self.enabled_indexes
            .values()
            .map(|index| index.metadata())
            .cloned()
            .collect()
    }

    pub fn by_id_indexes(&self) -> BTreeMap<TabletId, IndexId> {
        self.all_enabled_indexes()
            .into_iter()
            .filter(|index| index.name.is_by_id())
            .map(|index| (*index.name.table(), index.id().internal_id()))
            .collect()
    }

    /// Returns true if there are neither pending nor enabled indexes for the
    /// given table.
    pub fn has_no_indexes(&self, tablet_id: TabletId) -> bool {
        let table_indexes: Vec<&Index> = self.indexes_by_table(tablet_id).collect();
        table_indexes.is_empty()
    }

    pub fn text_indexes_by_table(
        &self,
        tablet_id: TabletId,
    ) -> impl Iterator<Item = &'_ Index> + '_ {
        // We only support storing one search index with a given name at at time, so
        // unlike database indexes, we're not overly concerned with the state.
        self.indexes_by_table(tablet_id)
            .filter(|index| index.metadata.is_text_index())
    }

    pub fn vector_indexes_by_table(
        &self,
        tablet_id: TabletId,
    ) -> impl Iterator<Item = &'_ Index> + '_ {
        self.indexes_by_table(tablet_id)
            .filter(|index| index.metadata.is_vector_index())
    }

    /// Returns both enabled and pending indexes for the given table.
    ///
    /// Multiple Indexes with a given name will be returned if an index is
    /// mutated but the mutated version is not yet enabled.
    pub(crate) fn indexes_by_table(
        &self,
        tablet_id: TabletId,
    ) -> impl Iterator<Item = &'_ Index> + '_ {
        let s = (&tablet_id, &IndexDescriptor::MIN);
        let range = (StdBound::Included(s.as_comparator()), StdBound::Unbounded);
        self.indexes_by_table
            .range::<_, dyn TupleKey<TabletId, IndexDescriptor>>(range)
            .take_while(move |(t, _)| *t == tablet_id)
            .flat_map(move |(t, d)| {
                let index_name = if d == &*INDEX_BY_ID_DESCRIPTOR {
                    GenericIndexName::by_id(*t)
                } else if d == &*INDEX_BY_CREATION_TIME_DESCRIPTOR {
                    GenericIndexName::by_creation_time(*t)
                } else if d.is_reserved() {
                    GenericIndexName::new_reserved(*t, d.clone())
                        .expect("Invalid IndexName in index")
                } else {
                    GenericIndexName::new(*t, d.clone()).expect("Invalid IndexName in index")
                };
                let result: Vec<&Index> = {
                    let name = &index_name;
                    vec![&self.enabled_indexes, &self.pending_indexes]
                        .into_iter()
                        .filter_map(|indexes| indexes.get(name))
                        .collect()
                };
                if result.is_empty() {
                    panic!("indexes_by_table and indexes inconsistent");
                }
                result
            })
    }

    pub fn enabled_index_metadata(
        &self,
        name: &TabletIndexName,
    ) -> Option<ParsedDocument<TabletIndexMetadata>> {
        self.get_enabled(name).map(|index| index.metadata.clone())
    }

    pub fn require_enabled(
        &self,
        index_name: &TabletIndexName,
        printable_index_name: &IndexName,
    ) -> anyhow::Result<Index> {
        let enabled = self.get_enabled(index_name);
        if let Some(enabled) = enabled {
            return Ok(enabled.clone());
        }
        match self.get_pending(index_name) {
            Some(_) => anyhow::bail!(index_backfilling_error(printable_index_name)),
            None => {
                anyhow::bail!(index_not_found_error(printable_index_name))
            },
        }
    }

    pub fn get_enabled(&self, index_name: &TabletIndexName) -> Option<&Index> {
        self.enabled_indexes.get(index_name)
    }

    pub fn get_pending(&self, index_name: &TabletIndexName) -> Option<&Index> {
        self.pending_indexes.get(index_name)
    }

    pub fn must_get_by_id(&self, tablet_id: TabletId) -> anyhow::Result<&Index> {
        self.get_enabled(&TabletIndexName::by_id(tablet_id))
            .ok_or_else(|| anyhow::anyhow!("No `by_id` index for table {}", tablet_id))
    }

    fn insert(&mut self, index: Index) -> Option<Index> {
        let name = index.name();
        let indexes_to_modify = if index.metadata.config.is_enabled() {
            &mut self.enabled_indexes
        } else {
            &mut self.pending_indexes
        };
        self.indexes_by_table
            .insert((*name.table(), name.descriptor().clone()));
        indexes_to_modify.insert(name, index)
    }

    fn remove(&mut self, to_remove: &ParsedDocument<TabletIndexMetadata>) {
        let (remove_from, other) = if to_remove.config.is_enabled() {
            (&mut self.enabled_indexes, &self.pending_indexes)
        } else {
            (&mut self.pending_indexes, &self.enabled_indexes)
        };
        let removed = remove_from.remove(&to_remove.name);
        if let Some(removed) = removed {
            if removed.id() != to_remove.id().internal_id() {
                panic!("Tried to remove a different index with the same name");
            }
        } else {
            panic!("Tried to remove a non-existent index, or an index in the wrong state");
        }
        if !other.contains_key(&to_remove.name) {
            let key = (to_remove.name.table(), to_remove.name.descriptor());
            self.indexes_by_table.remove(key.as_comparator()).unwrap();
        }
    }

    pub fn index_ids(&self) -> BTreeSet<IndexId> {
        self.enabled_indexes
            .iter()
            .chain(self.pending_indexes.iter())
            .map(|(_name, index)| index.id)
            .collect()
    }

    /// Returns true if the same indexes are present in this registry and in
    /// `other`.
    ///
    /// Index state (ie pending/enabled) may not be identical even if this
    /// method returns true.
    pub fn same_indexes<'a>(&'a self, other: &'a Self) -> bool {
        // The implementation of this method assumes that index definitions cannot be
        // mutated. Updating or removing and re-adding an index must result in a
        // new index ID being created for this implementation to work correctly.
        vec![self, other]
            .into_iter()
            .map(|registry: &IndexRegistry| registry.index_ids())
            .all_equal()
    }
}

pub trait IndexedDocument {
    type IndexKey;
    fn id(&self) -> ResolvedDocumentId;
    fn index_key_bytes(
        &self,
        fields: &[FieldPath],
        persistence_version: PersistenceVersion,
    ) -> Self::IndexKey;
}

impl IndexedDocument for ResolvedDocument {
    type IndexKey = IndexKey;

    fn id(&self) -> ResolvedDocumentId {
        self.id()
    }

    fn index_key_bytes(
        &self,
        fields: &[FieldPath],
        persistence_version: PersistenceVersion,
    ) -> IndexKey {
        self.index_key(fields, persistence_version)
    }
}
impl IndexedDocument for PackedDocument {
    type IndexKey = IndexKeyBytes;

    fn id(&self) -> ResolvedDocumentId {
        self.id()
    }

    fn index_key_bytes(
        &self,
        fields: &[FieldPath],
        persistence_version: PersistenceVersion,
    ) -> IndexKeyBytes {
        self.index_key_owned(fields, persistence_version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub id: IndexId,
    pub metadata: ParsedDocument<TabletIndexMetadata>,
}

impl Index {
    fn new(id: IndexId, metadata: ParsedDocument<TabletIndexMetadata>) -> Self {
        Self { id, metadata }
    }

    pub fn id(&self) -> IndexId {
        self.id
    }

    pub fn name(&self) -> TabletIndexName {
        self.metadata.name.clone()
    }

    pub fn metadata(&self) -> &ParsedDocument<TabletIndexMetadata> {
        &self.metadata
    }
}

pub fn index_backfilling_error(name: &IndexName) -> ErrorMetadata {
    ErrorMetadata::bad_request(
        "IndexBackfillingError",
        format!("Index {name} is currently backfilling and not available to query yet.",),
    )
}

pub fn index_not_found_error(name: &IndexName) -> ErrorMetadata {
    ErrorMetadata::bad_request("IndexNotFoundError", format!("Index {name} not found."))
}

/// For a given document, contains all the index keys for the indexes on the
/// document’s table.
///
/// This is used in lieu of the full document in the write log. This is most of
/// the time more memory efficient (because we don’t need to store the full
/// document) and faster (because we don’t need to reconstruct the index keys
/// every time we need them).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DocumentIndexKeys(WithHeapSize<BTreeMap<TabletIndexName, DocumentIndexKeyValue>>);

impl DocumentIndexKeys {
    pub fn get(&self, index_name: &TabletIndexName) -> Option<&DocumentIndexKeyValue> {
        self.0.get(index_name)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn empty_for_test() -> Self {
        Self(Default::default())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn with_standard_index_for_test(
        index_name: TabletIndexName,
        index_value: IndexKey,
    ) -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(
            index_name,
            DocumentIndexKeyValue::Standard(index_value.to_bytes()),
        );
        Self(keys.into())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn with_search_index_for_test(
        index_name: TabletIndexName,
        search_field: FieldPath,
        search_field_value: ConvexString,
    ) -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(
            index_name,
            DocumentIndexKeyValue::Search(SearchIndexKeyValue {
                filter_values: Default::default(),
                search_field,
                search_field_value: Some(search_field_value),
            }),
        );
        Self(keys.into())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn with_search_index_for_test_with_filters(
        index_name: TabletIndexName,
        search_field: FieldPath,
        search_field_value: ConvexString,
        filter_values: BTreeMap<FieldPath, SearchFilterValue>,
    ) -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(
            index_name,
            DocumentIndexKeyValue::Search(SearchIndexKeyValue {
                filter_values: filter_values.into(),
                search_field,
                search_field_value: Some(search_field_value),
            }),
        );
        Self(keys.into())
    }
}

impl HeapSize for DocumentIndexKeys {
    fn heap_size(&self) -> usize {
        self.0.heap_size()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentIndexKeyValue {
    Standard(IndexKeyBytes),
    Search(SearchIndexKeyValue),
    // We don’t store index key values for vector indexes because they don’t
    // support subscriptions.
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchIndexKeyValue {
    /// These are values for the fields present in the must
    /// clauses of the search index.
    pub filter_values: WithHeapSize<BTreeMap<FieldPath, SearchFilterValue>>,
    pub search_field: FieldPath,
    pub search_field_value: Option<ConvexString>,
}

impl HeapSize for DocumentIndexKeyValue {
    fn heap_size(&self) -> usize {
        match self {
            DocumentIndexKeyValue::Standard(index_key) => index_key.heap_size(),
            DocumentIndexKeyValue::Search(SearchIndexKeyValue {
                filter_values,
                search_field,
                search_field_value,
            }) => {
                filter_values.heap_size()
                    + search_field.heap_size()
                    + search_field_value.heap_size()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use common::{
        bootstrap_model::index::{
            database_index::IndexedFields,
            text_index::{
                DeveloperTextIndexConfig,
                TextIndexSnapshot,
                TextIndexSnapshotData,
                TextIndexState,
                TextSnapshotVersion,
            },
            IndexMetadata,
        },
        document::CreationTime,
        testing::TestIdGenerator,
        types::{
            GenericIndexName,
            TableName,
            Timestamp,
        },
    };
    use maplit::btreemap;
    use value::{
        assert_obj,
        FieldPath,
    };

    use super::*;

    #[test]
    fn test_document_index_keys() -> anyhow::Result<()> {
        let mut id_generator = TestIdGenerator::new();
        let table_id = id_generator.user_table_id(&"messages".parse()?);

        // Create indexes
        let by_id = GenericIndexName::by_id(table_id.tablet_id);
        let by_name = GenericIndexName::new(table_id.tablet_id, IndexDescriptor::new("by_name")?)?;
        let by_content =
            GenericIndexName::new(table_id.tablet_id, IndexDescriptor::new("by_content")?)?;

        let indexes = vec![
            IndexMetadata::new_enabled(by_id.clone(), IndexedFields::by_id()),
            IndexMetadata::new_enabled(by_name.clone(), vec!["name".parse()?].try_into()?),
            IndexMetadata::new_text_index(
                by_content.clone(),
                DeveloperTextIndexConfig {
                    search_field: FieldPath::from_str("content")?,
                    filter_fields: vec![FieldPath::from_str("author")?].into_iter().collect(),
                },
                TextIndexState::SnapshottedAt(TextIndexSnapshot {
                    data: TextIndexSnapshotData::MultiSegment(vec![]),
                    ts: Timestamp::MIN,
                    version: TextSnapshotVersion::V2UseStringIds,
                }),
            ),
        ];

        let index_documents = index_documents(&mut id_generator, indexes)?;
        let index_registry = IndexRegistry::bootstrap(
            &id_generator,
            index_documents.values(),
            PersistenceVersion::default(),
        )?;

        let doc = ResolvedDocument::new(
            id_generator.user_generate(&TableName::from_str("messages")?),
            CreationTime::ONE,
            assert_obj!(
                "name" => "test",
                "content" => "hello world",
                "author" => "alice"
            ),
        )?;

        let index_keys = index_registry.document_index_keys(PackedDocument::pack(&doc));

        let expected = DocumentIndexKeys(btreemap! {
            by_name.clone() => DocumentIndexKeyValue::Standard(
                doc.index_key_bytes(&[FieldPath::from_str("name")?], PersistenceVersion::default()).to_bytes()
            ),
            by_content.clone() => DocumentIndexKeyValue::Search(SearchIndexKeyValue {
                filter_values: btreemap! {
                    FieldPath::from_str("author")? => SearchFilterValue::from_search_value(
                        doc.value().get_path(&FieldPath::from_str("author")?)
                    )
                }.into(),
                search_field: FieldPath::from_str("content")?,
                search_field_value: Some("hello world".try_into()?),
            }),
            by_id.clone() => DocumentIndexKeyValue::Standard(
                doc.index_key_bytes(&[], PersistenceVersion::default()).to_bytes()
            ),
        }.into());

        assert_eq!(index_keys, expected);
        Ok(())
    }

    fn index_documents(
        id_generator: &mut TestIdGenerator,
        mut indexes: Vec<TabletIndexMetadata>,
    ) -> anyhow::Result<BTreeMap<ResolvedDocumentId, ResolvedDocument>> {
        let mut index_documents = BTreeMap::new();
        let index_table = id_generator.system_table_id(&INDEX_TABLE);
        // Add the _index.by_id index.
        indexes.push(IndexMetadata::new_enabled(
            GenericIndexName::by_id(index_table.tablet_id),
            IndexedFields::by_id(),
        ));
        for metadata in indexes {
            let doc = gen_index_document(id_generator, metadata.clone())?;
            index_documents.insert(doc.id(), doc);
        }
        Ok(index_documents)
    }

    fn gen_index_document(
        id_generator: &mut TestIdGenerator,
        metadata: TabletIndexMetadata,
    ) -> anyhow::Result<ResolvedDocument> {
        let index_id = id_generator.system_generate(&INDEX_TABLE);
        ResolvedDocument::new(index_id, CreationTime::ONE, metadata.try_into()?)
    }
}
