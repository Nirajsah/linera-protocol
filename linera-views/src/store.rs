// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! This provides the trait definitions for the stores.

use std::{fmt::Debug, future::Future};

use serde::de::DeserializeOwned;

#[cfg(with_testing)]
use crate::random::generate_test_namespace;
use crate::{batch::Batch, common::from_bytes_option, ViewError};

/// The error type for the key-value stores.
pub trait KeyValueStoreError:
    std::error::Error + From<bcs::Error> + Debug + Send + Sync + 'static
{
    /// The name of the backend.
    const BACKEND: &'static str;
}

impl<E: KeyValueStoreError> From<E> for ViewError {
    fn from(error: E) -> Self {
        Self::StoreError {
            backend: E::BACKEND,
            error: Box::new(error),
        }
    }
}

/// Define an associated [`KeyValueStoreError`].
pub trait WithError {
    /// The error type.
    type Error: KeyValueStoreError;
}

/// Low-level, asynchronous read key-value operations. Useful for storage APIs not based on views.
#[cfg_attr(not(web), trait_variant::make(Send + Sync))]
pub trait ReadableKeyValueStore: WithError {
    /// The maximal size of keys that can be stored.
    const MAX_KEY_SIZE: usize;

    /// Retrieve the number of stream queries.
    fn max_stream_queries(&self) -> usize;

    /// Retrieves a `Vec<u8>` from the database using the provided `key`.
    async fn read_value_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Tests whether a key exists in the database
    async fn contains_key(&self, key: &[u8]) -> Result<bool, Self::Error>;

    /// Tests whether a list of keys exist in the database
    async fn contains_keys(&self, keys: Vec<Vec<u8>>) -> Result<Vec<bool>, Self::Error>;

    /// Retrieves multiple `Vec<u8>` from the database using the provided `keys`.
    async fn read_multi_values_bytes(
        &self,
        keys: Vec<Vec<u8>>,
    ) -> Result<Vec<Option<Vec<u8>>>, Self::Error>;

    /// Finds the `key` matching the prefix. The prefix is not included in the returned keys.
    async fn find_keys_by_prefix(&self, key_prefix: &[u8]) -> Result<Vec<Vec<u8>>, Self::Error>;

    /// Finds the `(key,value)` pairs matching the prefix. The prefix is not included in the returned keys.
    async fn find_key_values_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Self::Error>;

    // We can't use `async fn` here in the below implementations due to
    // https://github.com/rust-lang/impl-trait-utils/issues/17, but once that bug is fixed
    // we can revert them to `async fn` syntax, which is neater.

    /// Reads a single `key` and deserializes the result if present.
    fn read_value<V: DeserializeOwned>(
        &self,
        key: &[u8],
    ) -> impl Future<Output = Result<Option<V>, Self::Error>> {
        async { Ok(from_bytes_option(&self.read_value_bytes(key).await?)?) }
    }

    /// Reads multiple `keys` and deserializes the results if present.
    fn read_multi_values<V: DeserializeOwned + Send + Sync>(
        &self,
        keys: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<Option<V>>, Self::Error>> {
        async {
            let mut values = Vec::with_capacity(keys.len());
            for entry in self.read_multi_values_bytes(keys).await? {
                values.push(from_bytes_option(&entry)?);
            }
            Ok(values)
        }
    }
}

/// Low-level, asynchronous write key-value operations. Useful for storage APIs not based on views.
#[cfg_attr(not(web), trait_variant::make(Send + Sync))]
pub trait WritableKeyValueStore: WithError {
    /// The maximal size of values that can be stored.
    const MAX_VALUE_SIZE: usize;

    /// Writes the `batch` in the database.
    async fn write_batch(&self, batch: Batch) -> Result<(), Self::Error>;

    /// Clears any journal entry that may remain.
    /// The journal is located at the `root_key`.
    async fn clear_journal(&self) -> Result<(), Self::Error>;
}

/// Low-level trait for the administration of stores and their namespaces.
#[cfg_attr(not(web), trait_variant::make(Send + Sync))]
pub trait AdminKeyValueStore: WithError + Sized {
    /// The configuration needed to interact with a new store.
    type Config: Send + Sync;
    /// The name of this class of stores
    fn get_name() -> String;

    /// Connects to an existing namespace using the given configuration.
    async fn connect(config: &Self::Config, namespace: &str) -> Result<Self, Self::Error>;

    /// Opens the key partition starting at `root_key` and returns a clone of the
    /// connection to work in this partition.
    ///
    /// IMPORTANT: It is assumed that the returned connection is the only user of the
    /// partition (for both read and write) and will remain so until it is ended. Future
    /// implementations of this method may fail if this is not the case.
    fn open_exclusive(&self, root_key: &[u8]) -> Result<Self, Self::Error>;

    /// Obtains the list of existing namespaces.
    async fn list_all(config: &Self::Config) -> Result<Vec<String>, Self::Error>;

    /// Lists the root keys of the namespace.
    /// It is possible that some root keys have no keys.
    async fn list_root_keys(
        config: &Self::Config,
        namespace: &str,
    ) -> Result<Vec<Vec<u8>>, Self::Error>;

    /// Deletes all the existing namespaces.
    fn delete_all(config: &Self::Config) -> impl Future<Output = Result<(), Self::Error>> {
        async {
            let namespaces = Self::list_all(config).await?;
            for namespace in namespaces {
                Self::delete(config, &namespace).await?;
            }
            Ok(())
        }
    }

    /// Tests if a given namespace exists.
    async fn exists(config: &Self::Config, namespace: &str) -> Result<bool, Self::Error>;

    /// Creates a namespace. Returns an error if the namespace exists.
    async fn create(config: &Self::Config, namespace: &str) -> Result<(), Self::Error>;

    /// Deletes the given namespace.
    async fn delete(config: &Self::Config, namespace: &str) -> Result<(), Self::Error>;

    /// Initializes a storage if missing and provides it.
    fn maybe_create_and_connect(
        config: &Self::Config,
        namespace: &str,
    ) -> impl Future<Output = Result<Self, Self::Error>> {
        async {
            if !Self::exists(config, namespace).await? {
                Self::create(config, namespace).await?;
            }
            Self::connect(config, namespace).await
        }
    }

    /// Creates a new storage. Overwrites it if this namespace already exists.
    fn recreate_and_connect(
        config: &Self::Config,
        namespace: &str,
    ) -> impl Future<Output = Result<Self, Self::Error>> {
        async {
            if Self::exists(config, namespace).await? {
                Self::delete(config, namespace).await?;
            }
            Self::create(config, namespace).await?;
            Self::connect(config, namespace).await
        }
    }
}

/// Low-level, asynchronous write and read key-value operations. Useful for storage APIs not based on views.
pub trait RestrictedKeyValueStore: ReadableKeyValueStore + WritableKeyValueStore {}

impl<T> RestrictedKeyValueStore for T where T: ReadableKeyValueStore + WritableKeyValueStore {}

/// Low-level, asynchronous write and read key-value operations. Useful for storage APIs not based on views.
pub trait KeyValueStore:
    ReadableKeyValueStore + WritableKeyValueStore + AdminKeyValueStore
{
}

impl<T> KeyValueStore for T where
    T: ReadableKeyValueStore + WritableKeyValueStore + AdminKeyValueStore
{
}

/// The functions needed for testing purposes
#[cfg(with_testing)]
pub trait TestKeyValueStore: KeyValueStore {
    /// Obtains a test config
    async fn new_test_config() -> Result<Self::Config, Self::Error>;

    /// Creates a store for testing purposes
    async fn new_test_store() -> Result<Self, Self::Error> {
        let config = Self::new_test_config().await?;
        let namespace = generate_test_namespace();
        Self::recreate_and_connect(&config, &namespace).await
    }
}
