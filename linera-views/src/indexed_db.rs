// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::Debug;

use futures::future;
use indexed_db_futures::{js_sys, prelude::*, web_sys};
use thiserror::Error;

use crate::{
    batch::{Batch, WriteOperation},
    common::{
        get_upper_bound_option, CommonStoreConfig, ContextFromStore, LocalAdminKeyValueStore,
        LocalKeyValueStore, LocalReadableKeyValueStore, LocalWritableKeyValueStore,
    },
    value_splitting::DatabaseConsistencyError,
    views::ViewError,
};

/// The initial configuration of the system
#[derive(Debug)]
pub struct IndexedDbStoreConfig {
    /// The common configuration of the key value store
    pub common_config: CommonStoreConfig,
}

impl IndexedDbStoreConfig {
    /// Creates a `IndexedDbStoreConfig`. `max_concurrent_queries` and `cache_size` are not used.
    pub fn new(max_stream_queries: usize) -> Self {
        let common_config = CommonStoreConfig {
            max_concurrent_queries: None,
            max_stream_queries,
            cache_size: 1000,
        };
        Self { common_config }
    }
}

/// The number of streams for the test
#[cfg(with_testing)]
pub const TEST_INDEX_DB_MAX_STREAM_QUERIES: usize = 10;

const DATABASE_NAME: &str = "linera";

/// A browser implementation of a key-value store using the [IndexedDB
/// API](https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API#:~:text=IndexedDB%20is%20a%20low%2Dlevel,larger%20amounts%20of%20structured%20data.).
pub struct IndexedDbStore {
    /// The database used for storing the data.
    pub database: IdbDatabase,
    /// The object store name used for storing the data.
    pub object_store_name: String,
    /// The maximum number of queries used for the stream.
    pub max_stream_queries: usize,
}

impl IndexedDbStore {
    fn with_object_store<R>(
        &self,
        f: impl FnOnce(IdbObjectStore) -> R,
    ) -> Result<R, IndexedDbStoreError> {
        let transaction = self.database.transaction_on_one(&self.object_store_name)?;
        let object_store = transaction.object_store(&self.object_store_name)?;
        Ok(f(object_store))
    }
}

fn prefix_to_range(prefix: &[u8]) -> Result<web_sys::IdbKeyRange, wasm_bindgen::JsValue> {
    let lower = js_sys::Uint8Array::from(prefix);
    if let Some(upper) = get_upper_bound_option(prefix) {
        let upper = js_sys::Uint8Array::from(&upper[..]);
        web_sys::IdbKeyRange::bound_with_lower_open_and_upper_open(
            &lower.into(),
            &upper.into(),
            false,
            true,
        )
    } else {
        web_sys::IdbKeyRange::lower_bound(&lower.into())
    }
}

impl LocalReadableKeyValueStore<IndexedDbStoreError> for IndexedDbStore {
    const MAX_KEY_SIZE: usize = usize::MAX;
    type Keys = Vec<Vec<u8>>;
    type KeyValues = Vec<(Vec<u8>, Vec<u8>)>;

    fn max_stream_queries(&self) -> usize {
        self.max_stream_queries
    }

    async fn read_value_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, IndexedDbStoreError> {
        let key = js_sys::Uint8Array::from(key);
        let value = self.with_object_store(|o| o.get(&key))??.await?;
        Ok(value.map(|v| js_sys::Uint8Array::new(&v).to_vec()))
    }

    async fn contains_key(&self, key: &[u8]) -> Result<bool, IndexedDbStoreError> {
        let key = js_sys::Uint8Array::from(key);
        let count = self.with_object_store(|o| o.count_with_key(&key))??.await?;
        assert!(count < 2);
        Ok(count == 1)
    }

    async fn read_multi_values_bytes(
        &self,
        keys: Vec<Vec<u8>>,
    ) -> Result<Vec<Option<Vec<u8>>>, IndexedDbStoreError> {
        future::try_join_all(
            keys.into_iter()
                .map(|key| async move { self.read_value_bytes(&key).await }),
        )
        .await
    }

    async fn find_keys_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Vec<Vec<u8>>, IndexedDbStoreError> {
        let range = prefix_to_range(key_prefix)?;
        Ok(self
            .with_object_store(|o| o.get_all_keys_with_key(&range))??
            .await?
            .into_iter()
            .map(|key| {
                let key = js_sys::Uint8Array::new(&key);
                key.subarray(key_prefix.len() as u32, key.length()).to_vec()
            })
            .collect())
    }

    async fn find_key_values_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, IndexedDbStoreError> {
        let mut key_values = vec![];
        let range = prefix_to_range(key_prefix)?;
        let transaction = self.database.transaction_on_one(&self.object_store_name)?;
        let object_store = transaction.object_store(&self.object_store_name)?;
        let Some(cursor) = object_store.open_cursor_with_range_owned(range)?.await? else {
            return Ok(key_values);
        };

        loop {
            let Some(key) = cursor.primary_key() else {
                break;
            };
            let key = js_sys::Uint8Array::new(&key);
            key_values.push((
                key.subarray(key_prefix.len() as u32, key.length()).to_vec(),
                js_sys::Uint8Array::new(&cursor.value()).to_vec(),
            ));
            if !cursor.continue_cursor()?.await? {
                break;
            }
        }

        Ok(key_values)
    }
}

impl LocalWritableKeyValueStore<IndexedDbStoreError> for IndexedDbStore {
    const MAX_VALUE_SIZE: usize = usize::MAX;

    async fn write_batch(&self, batch: Batch, _base_key: &[u8]) -> Result<(), IndexedDbStoreError> {
        let transaction = self
            .database
            .transaction_on_one_with_mode(&self.object_store_name, IdbTransactionMode::Readwrite)?;
        let object_store = transaction.object_store(&self.object_store_name)?;

        for ent in batch.operations {
            match ent {
                WriteOperation::Put { key, value } => {
                    object_store
                        .put_key_val_owned(
                            js_sys::Uint8Array::from(&key[..]),
                            &js_sys::Uint8Array::from(&value[..]),
                        )?
                        .await?;
                }
                WriteOperation::Delete { key } => {
                    object_store
                        .delete_owned(js_sys::Uint8Array::from(&key[..]))?
                        .await?;
                }
                WriteOperation::DeletePrefix { key_prefix } => {
                    object_store
                        .delete_owned(prefix_to_range(&key_prefix[..])?)?
                        .await?;
                }
            }
        }

        Ok(())
    }

    async fn clear_journal(&self, _base_key: &[u8]) -> Result<(), IndexedDbStoreError> {
        Ok(())
    }
}

impl LocalAdminKeyValueStore for IndexedDbStore {
    type Error = IndexedDbStoreError;
    type Config = IndexedDbStoreConfig;

    async fn connect(config: &Self::Config, namespace: &str) -> Result<Self, IndexedDbStoreError> {
        let namespace = namespace.to_string();
        let object_store_name = namespace.clone();
        let mut database = IdbDatabase::open(DATABASE_NAME)?.await?;

        if !database.object_store_names().any(|n| n == namespace) {
            let version = database.version();
            database.close();
            let mut db_req = IdbDatabase::open_f64(DATABASE_NAME, version + 1.0)?;
            db_req.set_on_upgrade_needed(Some(move |event: &IdbVersionChangeEvent| {
                event.db().create_object_store(&namespace)?;
                Ok(())
            }));
            database = db_req.await?;
        }

        Ok(IndexedDbStore {
            database,
            object_store_name,
            max_stream_queries: config.common_config.max_stream_queries,
        })
    }

    async fn list_all(config: &Self::Config) -> Result<Vec<String>, IndexedDbStoreError> {
        Ok(Self::connect(config, "")
            .await?
            .database
            .object_store_names()
            .collect())
    }

    async fn exists(config: &Self::Config, namespace: &str) -> Result<bool, IndexedDbStoreError> {
        Ok(Self::connect(config, "")
            .await?
            .database
            .object_store_names()
            .any(|x| x == namespace))
    }

    async fn create(config: &Self::Config, namespace: &str) -> Result<(), IndexedDbStoreError> {
        Self::connect(config, "")
            .await?
            .database
            .create_object_store(namespace)?;
        Ok(())
    }

    async fn delete(config: &Self::Config, namespace: &str) -> Result<(), IndexedDbStoreError> {
        Ok(Self::connect(config, "")
            .await?
            .database
            .delete_object_store(namespace)?)
    }
}

impl LocalKeyValueStore for IndexedDbStore {
    type Error = IndexedDbStoreError;
}

/// An implementation of [`crate::common::Context`] that stores all values in an IndexedDB
/// database.
pub type IndexedDbContext<E> = ContextFromStore<E, IndexedDbStore>;

impl<E> IndexedDbContext<E> {
    /// Creates a [`IndexedDbContext`].
    pub fn new(store: IndexedDbStore, extra: E) -> Self {
        Self {
            store,
            base_key: vec![],
            extra,
        }
    }
}

#[cfg(with_testing)]
mod testing {
    use super::*;
    use crate::test_utils::generate_test_namespace;

    /// Provides a `IndexedDbContext<()>` that can be used for tests.
    pub async fn create_indexed_db_test_context() -> IndexedDbContext<()> {
        IndexedDbContext::new(
            create_indexed_db_store_stream_queries(TEST_INDEX_DB_MAX_STREAM_QUERIES).await,
            (),
        )
    }

    /// Creates a test IndexedDB client for working.
    pub async fn create_indexed_db_store_stream_queries(
        max_stream_queries: usize,
    ) -> IndexedDbStore {
        let config = IndexedDbStoreConfig::new(max_stream_queries);
        let namespace = generate_test_namespace();
        IndexedDbStore::connect(&config, &namespace).await.unwrap()
    }

    /// Creates a test IndexedDB store for working.
    #[cfg(with_testing)]
    pub async fn create_indexed_db_test_store() -> IndexedDbStore {
        create_indexed_db_store_stream_queries(TEST_INDEX_DB_MAX_STREAM_QUERIES).await
    }
}

#[cfg(with_testing)]
pub use testing::*;

/// The error type for [`IndexedDbContext`].
#[derive(Error, Debug)]
pub enum IndexedDbStoreError {
    /// Serialization error with BCS.
    #[error("BCS error: {0}")]
    Bcs(#[from] bcs::Error),

    /// The value is too large for the IndexedDbStore
    #[error("The value is too large for the IndexedDbStore")]
    TooLargeValue,

    /// The database is not consistent
    #[error(transparent)]
    DatabaseConsistencyError(#[from] DatabaseConsistencyError),

    /// A DOM exception occurred in the IndexedDB operations
    #[error("DOM exception: {}", self.to_string())]
    Dom(web_sys::DomException),

    /// JavaScript threw an exception whilst handling IndexedDB operations
    #[error("JavaScript exception: {}", self.to_string())]
    Js(wasm_bindgen::JsValue),
}

impl From<web_sys::DomException> for IndexedDbStoreError {
    fn from(dom_exception: web_sys::DomException) -> Self {
        Self::Dom(dom_exception)
    }
}

impl From<wasm_bindgen::JsValue> for IndexedDbStoreError {
    fn from(js_value: wasm_bindgen::JsValue) -> Self {
        Self::Js(js_value)
    }
}

impl From<IndexedDbStoreError> for ViewError {
    fn from(error: IndexedDbStoreError) -> Self {
        Self::StoreError {
            backend: "indexed_db".to_string(),
            error: error.to_string(),
        }
    }
}
