//
// Copyright 2022 The Project Oak Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
#![no_std]

extern crate alloc;

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use hashbrown::HashMap;
use log::Level;
use oak_functions_abi::{proto::OakStatus, ExtensionHandle, StorageGetItemResponse};
use oak_functions_extension::{ExtensionFactory, OakApiNativeExtension};
use oak_logger::OakLogger;
use spinning_top::Spinlock;

pub struct LookupFactory<L: OakLogger> {
    manager: Arc<LookupDataManager<L>>,
}

impl<L> LookupFactory<L>
where
    L: OakLogger + 'static,
{
    pub fn new_boxed_extension_factory(
        manager: Arc<LookupDataManager<L>>,
    ) -> anyhow::Result<Box<dyn ExtensionFactory<L>>> {
        let lookup_factory = Self { manager };
        Ok(Box::new(lookup_factory))
    }
}

impl<L> ExtensionFactory<L> for LookupFactory<L>
where
    L: OakLogger + 'static,
{
    fn create(&self) -> anyhow::Result<Box<dyn OakApiNativeExtension>> {
        let extension = self.manager.create_lookup_data();
        Ok(Box::new(extension))
    }
}

impl<L: OakLogger> OakApiNativeExtension for LookupData<L> {
    fn invoke(&mut self, request: Vec<u8>) -> Result<Vec<u8>, OakStatus> {
        // The request is the key to lookup.
        let key = request;
        let key_to_log = key.clone().into_iter().take(512).collect::<Vec<_>>();
        self.log_debug(&format!(
            "storage_get_item(): key: {}",
            format_bytes(&key_to_log)
        ));
        let value = self.get(&key);

        // Log found value.
        value.clone().map_or_else(
            || {
                self.log_debug("storage_get_item(): value not found");
            },
            |value| {
                // Truncate value for logging.
                let value_to_log = value.into_iter().take(512).collect::<Vec<_>>();
                self.log_debug(&format!(
                    "storage_get_item(): value: {}",
                    format_bytes(&value_to_log)
                ));
            },
        );

        Ok(StorageGetItemResponse { value }.into())
    }

    fn terminate(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_handle(&self) -> ExtensionHandle {
        ExtensionHandle::LookupHandle
    }
}

// Data maintains the invariant on lookup data to have [at most one
// value](https://github.com/project-oak/oak/tree/main/oak_functions/lookup/README.md#invariant-at-most-one-value)
pub type Data = HashMap<Vec<u8>, Vec<u8>>;

#[derive(Default)]
enum BuilderState {
    #[default]
    Empty,
    Updating,
}

// Incrementally build data backing the lookup data keeping track of the state.
#[derive(Default)]
struct DataBuilder {
    data: Data,
    state: BuilderState,
}

impl DataBuilder {
    fn new() -> Self {
        Default::default()
    }

    /// Build data from the builder and set the builder back to the initial state.
    fn build(&mut self) -> Data {
        self.state = BuilderState::Empty;
        core::mem::take(&mut self.data)
    }

    /// Extends the DataBuilder with new data.
    ///
    /// Note, if new data contains a key already present in the existing data, calling extend
    /// overwrites the value.
    fn extend(&mut self, new_data: Data) {
        self.state = BuilderState::Updating;
        self.data.extend(new_data);
    }
}

/// Utility for managing lookup data.
///
/// `LookupDataManager` can be used to create `LookupData` instances that share the underlying data.
/// It can also update the underlying data. After updating the data, new `LookupData` instances will
/// use the new data, but earlier instances will still used the earlier data.
///
/// LookupDataManager maintains the invariants [consistent view on lookup
/// data](https://github.com/project-oak/oak/tree/main/oak_functions/lookup/README.md#invariant-consistent-view-on-lookup-data) , and [shared
/// lookup data](https://github.com/project-oak/oak/tree/main/oak_functions/lookup/README.md#invariant-shared-lookup-data)
///
/// Note that the data is never mutated in-place, but only ever replaced. So instead of the Rust
/// idiom `Arc<Spinlock<T>>` we have `Spinlock<Arc<T>>`.
///
/// In the future we may replace both the mutex and the hash map with something like RCU.
pub struct LookupDataManager<L: OakLogger + Clone> {
    data: Spinlock<Arc<Data>>,
    // Behind a lock, because we have multiple references to LookupDataManager and need to mutate
    // data builder.
    data_builder: Spinlock<DataBuilder>,
    logger: L,
}

#[derive(Clone)]
pub enum UpdateAction {
    StartAndFinish,
}

pub enum UpdateStatus {
    Started,
    Finished,
    Aborted,
}

impl<L> LookupDataManager<L>
where
    L: OakLogger + Clone,
{
    /// Creates a new instance with empty backing data.
    pub fn new_empty(logger: L) -> Self {
        Self {
            data: Spinlock::new(Arc::new(Data::new())),
            data_builder: Spinlock::new(DataBuilder::new()),
            logger,
        }
    }

    /// Creates an instance of LookupData populated with the given entries.
    pub fn for_test(data: Data, logger: L) -> Self {
        let test_manager = Self::new_empty(logger);
        *test_manager.data.lock() = Arc::new(data);
        test_manager
    }

    /// Updates the backing data that will be used by new `LookupData` instances.
    pub fn update_data(&self, action: UpdateAction, new_data: Data) -> UpdateStatus {
        let mut data_builder = self.data_builder.lock();

        match (&data_builder.state, &action) {
            (BuilderState::Empty, UpdateAction::StartAndFinish) => {
                data_builder.extend(new_data);
                let next_data = data_builder.build();
                let mut data = self.data.lock();
                *data = Arc::new(next_data);
                UpdateStatus::Finished
            }
            (BuilderState::Updating, UpdateAction::StartAndFinish) => {
                // Clear the builder throwing away the intermediate result.
                let _ = data_builder.build();
                UpdateStatus::Aborted
            }
        }
    }

    /// Creates a new `LookupData` instance with a reference to the current backing data.
    pub fn create_lookup_data(&self) -> LookupData<L> {
        let data = self.data.lock().clone();
        LookupData::new(data, self.logger.clone())
    }
}

/// Provides access to shared lookup data.
pub struct LookupData<L: OakLogger + Clone> {
    data: Arc<Data>,
    logger: L,
}

impl<L> LookupData<L>
where
    L: OakLogger + Clone,
{
    fn new(data: Arc<Data>, logger: L) -> Self {
        Self { data, logger }
    }

    /// Gets an individual entry from the backing data.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.data.get(key).cloned()
    }

    /// Gets the number of entries in the backing data.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the backing data is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Logs an error message.
    ///
    /// The code assumes the message might contain sensitive information.
    pub fn log_error(&self, message: &str) {
        self.logger.log_sensitive(Level::Error, message)
    }

    /// Logs a debug message.
    ///
    /// The code assumes the message might contain sensitive information.
    pub fn log_debug(&self, message: &str) {
        self.logger.log_sensitive(Level::Debug, message)
    }
}

/// Converts a binary sequence to a string if it is a valid UTF-8 string, or formats it as a numeric
/// vector of bytes otherwise.
pub fn format_bytes(v: &[u8]) -> String {
    alloc::str::from_utf8(v)
        .map(|s| s.to_string())
        .unwrap_or_else(|_| format!("{:?}", v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct TestLogger {}
    impl OakLogger for TestLogger {
        fn log_sensitive(&self, _level: Level, _message: &str) {}
        fn log_public(&self, _level: Level, _message: &str) {}
    }

    #[test]
    fn test_lookup_data_instance_consistency() {
        // Ensure that the data for a specific lookup data instance remains consistent even if the
        // data in the manager has been updated.
        let manager = LookupDataManager::new_empty(TestLogger {});
        let lookup_data_0 = manager.create_lookup_data();
        assert_eq!(lookup_data_0.len(), 0);

        manager.update_data(
            UpdateAction::StartAndFinish,
            HashMap::from_iter([(b"key1".to_vec(), b"value1".to_vec())].into_iter()),
        );
        let lookup_data_1 = manager.create_lookup_data();
        assert_eq!(lookup_data_0.len(), 0);
        assert_eq!(lookup_data_1.len(), 1);

        manager.update_data(
            UpdateAction::StartAndFinish,
            HashMap::from_iter(
                [
                    (b"key1".to_vec(), b"value1".to_vec()),
                    (b"key2".to_vec(), b"value2".to_vec()),
                ]
                .into_iter(),
            ),
        );
        let lookup_data_2 = manager.create_lookup_data();
        assert_eq!(lookup_data_0.len(), 0);
        assert_eq!(lookup_data_1.len(), 1);
        assert_eq!(lookup_data_2.len(), 2);
    }

    #[test]
    fn test_format_bytes() {
        // Valid UTF-8 string.
        assert_eq!("🚀oak⭐", format_bytes("🚀oak⭐".as_bytes()));
        // Incorrect UTF-8 bytes, as per https://doc.rust-lang.org/std/string/struct.String.html#examples-3.
        assert_eq!("[0, 159, 146, 150]", format_bytes(&[0, 159, 146, 150]));
    }
}
