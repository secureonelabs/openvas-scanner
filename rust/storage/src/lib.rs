// SPDX-FileCopyrightText: 2023 Greenbone AG
//
// SPDX-License-Identifier: GPL-2.0-or-later

#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

pub mod item;
mod retrieve;
pub use retrieve::*;
pub mod time;
pub mod types;
use std::{
    fmt::Display,
    io,
    marker::PhantomData,
    sync::{Arc, Mutex, PoisonError},
};

use item::NVTField;
use types::Primitive;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde_support",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]

/// Structure to hold a knowledge base item
pub struct Kb {
    /// Key of the knowledge base entry
    pub key: String,
    /// Value of the knowledge base entry
    pub value: Primitive,
    /// If set it is the unix timestamp the KB entry will expire
    ///
    /// When an entry expires `get_kb` will not find that entry anymore.
    /// When it is Null the KB entry will stay the whole run.
    pub expire: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde_support",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]

/// Structure to hold a knowledge base item
pub struct Notus {
    /// Notus advisory metadata
    pub value: Primitive,
}

/// Describes various Fields of supported items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Field {
    /// Metadata of the NASL script.
    NVT(NVTField),
    /// Knowledge Base item
    KB(Kb),
    /// Notus advisories
    NOTUS(Notus),
}

impl From<NVTField> for Field {
    fn from(value: NVTField) -> Self {
        Self::NVT(value)
    }
}

impl From<Kb> for Field {
    fn from(value: Kb) -> Self {
        Self::KB(value)
    }
}
impl From<Notus> for Field {
    fn from(value: Notus) -> Self {
        Self::NOTUS(value)
    }
}

/// Defines abstract error cases
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    /// Informs the caller to retry the call
    Retry(String),
    /// The connection to a DB was lost.
    ///
    /// The default solution in those cases are most of the times to try a reconnect.
    ConnectionLost(String),
    /// Did expected a different kind of data and is unable to fulfil the request.
    ///
    /// This is usually a usage error.
    UnexpectedData(String),
    /// There is a deeper problem with the underlying DataBase
    ///
    /// An example would be that there is no free db left on redis and that it needs to be cleaned up.
    Dirty(String),
}

impl<S> From<PoisonError<S>> for StorageError {
    fn from(value: PoisonError<S>) -> Self {
        Self::Dirty(format!("{value:?}"))
    }
}

impl From<io::Error> for StorageError {
    fn from(value: io::Error) -> Self {
        let msg = format!("{:?}", value.kind());
        match value.kind() {
            io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::AlreadyExists
            | io::ErrorKind::AddrInUse
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Unsupported => StorageError::UnexpectedData(msg),
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted => StorageError::Retry(msg),
            _ => StorageError::Dirty(msg),
        }
    }
}

impl Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Retry(p) => write!(f, "There was a temporary issue while reading {p}."),
            StorageError::ConnectionLost(p) => write!(f, "Connection lost {p}."),
            StorageError::UnexpectedData(p) => write!(f, "Unexpected data {p}"),
            StorageError::Dirty(p) => write!(f, "Unexpected issue {p}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// Defines the Dispatcher interface to distribute fields
pub trait Dispatcher<K>: Sync + Send {
    /// Distributes given field under a key
    ///
    /// A key is usually a OID that was given when starting a script but in description run it is the filename.
    fn dispatch(&self, key: &K, scope: Field) -> Result<(), StorageError>;

    /// On exit is called when a script exit
    ///
    /// Some database require a cleanup therefore this method is called when a script finishes.
    fn on_exit(&self) -> Result<(), StorageError>;

    /// Retries a dispatch for the amount of retries when a retrieable error occurs.
    fn retry_dispatch(&self, retries: usize, key: &K, scope: Field) -> Result<(), StorageError> {
        match self.dispatch(key, scope.clone()) {
            Ok(r) => Ok(r),
            Err(e) => {
                if retries > 0 && matches!(e, StorageError::Retry(_)) {
                    self.retry_dispatch(retries - 1, key, scope)
                } else {
                    Err(e)
                }
            }
        }
    }
}

impl<K, T> Dispatcher<K> for Arc<T>
where
    T: Dispatcher<K>,
{
    fn dispatch(&self, key: &K, scope: Field) -> Result<(), StorageError> {
        self.as_ref().dispatch(key, scope)
    }

    fn on_exit(&self) -> Result<(), StorageError> {
        self.as_ref().on_exit()
    }
}

/// Convenience trait to use a dispatcher and retriever implementation
pub trait Storage<K>: Dispatcher<K> + Retriever<K> {
    /// Returns a reference to the retriever
    fn as_retriever(&self) -> &dyn Retriever<K>;
    /// Returns a reference to the dispatcher
    fn as_dispatcher(&self) -> &dyn Dispatcher<K>;
}

impl<K, T> Storage<K> for T
where
    T: Dispatcher<K> + Retriever<K>,
{
    fn as_retriever(&self) -> &dyn Retriever<K> {
        self
    }

    fn as_dispatcher(&self) -> &dyn Dispatcher<K> {
        self
    }
}

/// Contains a Vector of all stored items.
///
/// The first String statement is the used key while the Vector of Scope are the values.
type StoreItem = Vec<(String, Vec<Field>)>;

/// Is a in-memory dispatcher that behaves like a Storage.
#[derive(Default)]
pub struct DefaultDispatcher<K> {
    /// If dirty it will not clean the data on_exit
    dirty: bool,
    key: PhantomData<K>,
    /// The data storage
    ///
    /// The memory access is managed via an Arc while the Mutex ensures that only one consumer at a time is accessing it.
    data: Arc<Mutex<StoreItem>>,
}

impl<K> DefaultDispatcher<K> {
    /// Creates a new DefaultDispatcher
    pub fn new(dirty: bool) -> Self {
        Self {
            dirty,
            data: Default::default(),
            key: PhantomData,
        }
    }

    /// Cleanses stored data.
    pub fn cleanse(&self) -> Result<(), StorageError> {
        let mut data = Arc::as_ref(&self.data).lock()?;
        data.clear();
        data.shrink_to_fit();
        Ok(())
    }
}

impl<K> Dispatcher<K> for DefaultDispatcher<K>
where
    K: AsRef<str> + Display + Default + From<String> + Send + Sync,
{
    fn dispatch(&self, key: &K, scope: Field) -> Result<(), StorageError> {
        let mut data = Arc::as_ref(&self.data).lock()?;
        match data.iter_mut().find(|(k, _)| k.as_str() == key.as_ref()) {
            Some((_, v)) => v.push(scope),
            None => data.push((key.as_ref().to_owned(), vec![scope])),
        }
        tracing::trace!("Keys: {}", data.len());
        Ok(())
    }

    fn on_exit(&self) -> Result<(), StorageError> {
        if !self.dirty {
            self.cleanse()?;
        }

        Ok(())
    }
}

impl<K> Retriever<K> for DefaultDispatcher<K>
where
    K: AsRef<str> + Display + Default + From<String>,
{
    fn retrieve(&self, key: &K, scope: &Retrieve) -> Result<Vec<Field>, StorageError> {
        let data = Arc::as_ref(&self.data).lock()?;
        let skey = key.to_string();
        let result = data
            .iter()
            .filter(|(k, _)| k == &skey)
            .flat_map(|(_, v)| v.clone())
            .filter(|v| scope.for_field(v))
            .collect::<Vec<Field>>();
        Ok(result)
    }

    fn retrieve_by_field(
        &self,
        field: &Field,
        scope: &Retrieve,
    ) -> Result<Vec<(K, Vec<Field>)>, StorageError> {
        let data = Arc::as_ref(&self.data).lock()?;
        tracing::debug!("Entries: {:?}", data.len());
        let result = data
            .iter()
            .filter(|(_, v)| v.contains(field))
            .map(|(k, v)| {
                (
                    k.clone().into(),
                    v.iter().filter(|v| scope.for_field(v)).cloned().collect(),
                )
            })
            .collect::<Vec<(K, Vec<Field>)>>();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::item::NVTKey;

    use super::Field::*;
    use super::NVTField::*;
    use super::*;

    #[test]
    pub fn default_storage() -> Result<(), StorageError> {
        let storage = DefaultDispatcher::default();
        let key: String = Default::default();
        storage.dispatch(&key, NVT(Oid("moep".to_owned())))?;
        assert_eq!(
            storage.retrieve(&key, &Retrieve::NVT(None))?,
            vec![NVT(Oid("moep".to_owned()))]
        );
        assert_eq!(
            storage.retrieve(&key, &Retrieve::NVT(Some(NVTKey::Oid)))?,
            vec![NVT(Oid("moep".to_owned()))]
        );
        assert_eq!(
            storage.retrieve(&key, &Retrieve::NVT(Some(NVTKey::Family)))?,
            vec![]
        );
        assert_eq!(
            storage.retrieve_by_field(&NVT(Oid("moep".to_owned())), &Retrieve::NVT(None))?,
            vec![(key.clone(), vec![NVT(Oid("moep".to_owned()))])]
        );
        Ok(())
    }
}
