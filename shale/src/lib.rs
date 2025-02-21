// Copyright (C) 2023, Ava Labs, Inc. All rights reserved.
// See the file LICENSE.md for licensing terms.

use disk_address::DiskAddress;
use std::any::type_name;
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug, Formatter};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, RwLock, RwLockWriteGuard};

use thiserror::Error;

pub mod cached;
pub mod compact;
pub mod disk_address;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ShaleError {
    #[error("obj invalid: {addr:?} obj: {obj_type:?} error: {error:?}")]
    InvalidObj {
        addr: usize,
        obj_type: &'static str,
        error: &'static str,
    },
    #[error("invalid address length expected: {expected:?} found: {found:?})")]
    InvalidAddressLength { expected: DiskAddress, found: u64 },
    #[error("invalid node type")]
    InvalidNodeType,
    #[error("failed to create view: offset: {offset:?} size: {size:?}")]
    InvalidCacheView { offset: usize, size: u64 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// TODO:
// this could probably included with ShaleError,
// but keeping it separate for now as Obj/ObjRef might change in the near future
#[derive(Debug, Error)]
#[error("write error")]
pub struct ObjWriteError;

pub type SpaceId = u8;
pub const INVALID_SPACE_ID: SpaceId = 0xff;

pub struct DiskWrite {
    pub space_id: SpaceId,
    pub space_off: u64,
    pub data: Box<[u8]>,
}

impl std::fmt::Debug for DiskWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "[Disk space=0x{:02x} offset=0x{:04x} data=0x{}",
            self.space_id,
            self.space_off,
            hex::encode(&self.data)
        )
    }
}

/// A handle that pins and provides a readable access to a portion of the linear memory image.
pub trait CachedView {
    type DerefReturn: Deref<Target = [u8]>;
    fn as_deref(&self) -> Self::DerefReturn;
}

pub trait SendSyncDerefMut: DerefMut + Send + Sync {}

impl<T: Send + Sync + DerefMut> SendSyncDerefMut for T {}

/// In-memory store that offers access to intervals from a linear byte space, which is usually
/// backed by a cached/memory-mapped pool of the accessed intervals from the underlying linear
/// persistent store. Reads could trigger disk reads to bring data into memory, but writes will
/// *only* be visible in memory (it does not write back to the disk).
pub trait CachedStore: Debug + Send + Sync {
    /// Returns a handle that pins the `length` of bytes starting from `offset` and makes them
    /// directly accessible.
    fn get_view(
        &self,
        offset: usize,
        length: u64,
    ) -> Option<Box<dyn CachedView<DerefReturn = Vec<u8>>>>;
    /// Returns a handle that allows shared access to the store.
    fn get_shared(&self) -> Box<dyn SendSyncDerefMut<Target = dyn CachedStore>>;
    /// Write the `change` to the portion of the linear space starting at `offset`. The change
    /// should be immediately visible to all `CachedView` associated to this linear space.
    fn write(&mut self, offset: usize, change: &[u8]);
    /// Returns the identifier of this storage space.
    fn id(&self) -> SpaceId;
}

/// A addressed, typed, and read-writable handle for the stored item in [ShaleStore]. The object
/// represents the decoded/mapped data. The implementation of [ShaleStore] could use [ObjCache] to
/// turn a `TypedView` into an [ObjRef].
pub trait TypedView<T: ?Sized + Send + Sync>:
    std::fmt::Debug + Deref<Target = T> + Send + Sync
{
    /// Get the offset of the initial byte in the linear space.
    fn get_offset(&self) -> usize;
    /// Access it as a [CachedStore] object.
    fn get_mem_store(&self) -> &dyn CachedStore;
    /// Access it as a mutable CachedStore object
    fn get_mut_mem_store(&mut self) -> &mut dyn CachedStore;
    /// Estimate the serialized length of the current type content. It should not be smaller than
    /// the actually length.
    fn estimate_mem_image(&self) -> Option<u64>;
    /// Serialize the type content to the memory image. It defines how the current in-memory object
    /// of `T` should be represented in the linear storage space.
    fn write_mem_image(&self, mem_image: &mut [u8]) -> Result<(), ShaleError>;
    /// Gain mutable access to the typed content. By changing it, its serialized bytes (and length)
    /// could change.
    fn write(&mut self) -> &mut T;
    /// Returns if the typed content is memory-mapped (i.e., all changes through `write` are auto
    /// reflected in the underlying [CachedStore]).
    fn is_mem_mapped(&self) -> bool;
}

/// A wrapper of `TypedView` to enable writes. The direct construction (by [Obj::from_typed_view]
/// or [StoredView::ptr_to_obj]) could be useful for some unsafe access to a low-level item (e.g.
/// headers/metadata at bootstrap or part of [ShaleStore] implementation) stored at a given [DiskAddress]
/// . Users of [ShaleStore] implementation, however, will only use [ObjRef] for safeguarded access.
#[derive(Debug)]
pub struct Obj<T: ?Sized + Send + Sync> {
    value: Box<dyn TypedView<T>>,
    dirty: Option<u64>,
}

impl<T: ?Sized + Send + Sync> Obj<T> {
    #[inline(always)]
    pub fn as_ptr(&self) -> DiskAddress {
        DiskAddress(NonZeroUsize::new(self.value.get_offset()))
    }

    /// Write to the underlying object. Returns `Some(())` on success.
    #[inline]
    pub fn write(&mut self, modify: impl FnOnce(&mut T)) -> Result<(), ObjWriteError> {
        modify(self.value.write());

        // if `estimate_mem_image` gives overflow, the object will not be written
        self.dirty = match self.value.estimate_mem_image() {
            Some(len) => Some(len),
            None => return Err(ObjWriteError),
        };

        Ok(())
    }

    #[inline(always)]
    pub fn get_space_id(&self) -> SpaceId {
        self.value.get_mem_store().id()
    }

    #[inline(always)]
    pub fn from_typed_view(value: Box<dyn TypedView<T>>) -> Self {
        Obj { value, dirty: None }
    }

    pub fn flush_dirty(&mut self) {
        if !self.value.is_mem_mapped() {
            if let Some(new_value_len) = self.dirty.take() {
                let mut new_value = vec![0; new_value_len as usize];
                // TODO: log error
                self.value.write_mem_image(&mut new_value).unwrap();
                let offset = self.value.get_offset();
                let bx: &mut dyn CachedStore = self.value.get_mut_mem_store();
                bx.write(offset, &new_value);
            }
        }
    }
}

impl<T: ?Sized + Send + Sync> Drop for Obj<T> {
    fn drop(&mut self) {
        self.flush_dirty()
    }
}

impl<T: ?Sized + Send + Sync> Deref for Obj<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

/// User handle that offers read & write access to the stored [ShaleStore] item.
pub struct ObjRef<'a, T: Send + Sync> {
    inner: Option<Obj<T>>,
    cache: ObjCache<T>,
    _life: PhantomData<&'a mut ()>,
}

impl<'a, T: Send + Sync> ObjRef<'a, T> {
    pub fn to_longlive(mut self) -> ObjRef<'static, T> {
        ObjRef {
            inner: self.inner.take(),
            cache: ObjCache(self.cache.0.clone()),
            _life: PhantomData,
        }
    }

    #[inline]
    pub fn write(&mut self, modify: impl FnOnce(&mut T)) -> Result<(), ObjWriteError> {
        let inner = self.inner.as_mut().unwrap();
        inner.write(modify)?;

        self.cache.lock().dirty.insert(inner.as_ptr());

        Ok(())
    }
}

impl<'a, T: Send + Sync> Deref for ObjRef<'a, T> {
    type Target = Obj<T>;
    fn deref(&self) -> &Obj<T> {
        self.inner.as_ref().unwrap()
    }
}

impl<'a, T: Send + Sync> Drop for ObjRef<'a, T> {
    fn drop(&mut self) {
        let mut inner = self.inner.take().unwrap();
        let ptr = inner.as_ptr();
        let mut cache = self.cache.lock();
        match cache.pinned.remove(&ptr) {
            Some(true) => {
                inner.dirty = None;
            }
            _ => {
                cache.cached.put(ptr, inner);
            }
        }
    }
}

/// A persistent item storage backed by linear logical space. New items can be created and old
/// items could be retrieved or dropped.
pub trait ShaleStore<T: Send + Sync> {
    /// Dereference [DiskAddress] to a unique handle that allows direct access to the item in memory.
    fn get_item(&'_ self, ptr: DiskAddress) -> Result<ObjRef<'_, T>, ShaleError>;
    /// Allocate a new item.
    fn put_item(&'_ self, item: T, extra: u64) -> Result<ObjRef<'_, T>, ShaleError>;
    /// Free an item and recycle its space when applicable.
    fn free_item(&mut self, item: DiskAddress) -> Result<(), ShaleError>;
    /// Flush all dirty writes.
    fn flush_dirty(&self) -> Option<()>;
}

/// A stored item type that can be decoded from or encoded to on-disk raw bytes. An efficient
/// implementation could be directly transmuting to/from a POD struct. But sometimes necessary
/// compression/decompression is needed to reduce disk I/O and facilitate faster in-memory access.
pub trait Storable {
    fn dehydrated_len(&self) -> u64;
    fn dehydrate(&self, to: &mut [u8]) -> Result<(), ShaleError>;
    fn hydrate<T: CachedStore>(addr: usize, mem: &T) -> Result<Self, ShaleError>
    where
        Self: Sized;
    fn is_mem_mapped(&self) -> bool {
        false
    }
}

pub fn to_dehydrated(item: &dyn Storable) -> Result<Vec<u8>, ShaleError> {
    let mut buff = vec![0; item.dehydrated_len() as usize];
    item.dehydrate(&mut buff)?;
    Ok(buff)
}

/// Reference implementation of [TypedView]. It takes any type that implements [Storable]
pub struct StoredView<T> {
    decoded: T,
    mem: Box<dyn SendSyncDerefMut<Target = dyn CachedStore>>,
    offset: usize,
    len_limit: u64,
}

impl<T: Storable + Debug> Debug for StoredView<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let StoredView {
            decoded,
            offset,
            len_limit,
            mem: _,
        } = self;
        f.debug_struct("StoredView")
            .field("decoded", decoded)
            .field("offset", offset)
            .field("len_limit", len_limit)
            .finish()
    }
}

impl<T: Storable> Deref for StoredView<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.decoded
    }
}

impl<T: Storable + Debug + Send + Sync> TypedView<T> for StoredView<T> {
    fn get_offset(&self) -> usize {
        self.offset
    }

    fn get_mem_store(&self) -> &dyn CachedStore {
        &**self.mem
    }

    fn get_mut_mem_store(&mut self) -> &mut dyn CachedStore {
        &mut **self.mem
    }

    fn estimate_mem_image(&self) -> Option<u64> {
        let len = self.decoded.dehydrated_len();
        if len > self.len_limit {
            None
        } else {
            Some(len)
        }
    }

    fn write_mem_image(&self, mem_image: &mut [u8]) -> Result<(), ShaleError> {
        self.decoded.dehydrate(mem_image)
    }

    fn write(&mut self) -> &mut T {
        &mut self.decoded
    }
    fn is_mem_mapped(&self) -> bool {
        self.decoded.is_mem_mapped()
    }
}

impl<T: Storable + Debug + Send + Sync + 'static> StoredView<T> {
    #[inline(always)]
    fn new<U: CachedStore>(offset: usize, len_limit: u64, space: &U) -> Result<Self, ShaleError> {
        let decoded = T::hydrate(offset, space)?;
        Ok(Self {
            offset,
            decoded,
            mem: space.get_shared(),
            len_limit,
        })
    }

    #[inline(always)]
    fn from_hydrated(
        offset: usize,
        len_limit: u64,
        decoded: T,
        space: &dyn CachedStore,
    ) -> Result<Self, ShaleError> {
        Ok(Self {
            offset,
            decoded,
            mem: space.get_shared(),
            len_limit,
        })
    }

    #[inline(always)]
    pub fn ptr_to_obj<U: CachedStore>(
        store: &U,
        ptr: DiskAddress,
        len_limit: u64,
    ) -> Result<Obj<T>, ShaleError> {
        Ok(Obj::from_typed_view(Box::new(Self::new(
            ptr.get(),
            len_limit,
            store,
        )?)))
    }

    #[inline(always)]
    pub fn item_to_obj(
        store: &dyn CachedStore,
        addr: usize,
        len_limit: u64,
        decoded: T,
    ) -> Result<Obj<T>, ShaleError> {
        Ok(Obj::from_typed_view(Box::new(Self::from_hydrated(
            addr, len_limit, decoded, store,
        )?)))
    }
}

impl<T: Storable + Send + Sync> StoredView<T> {
    fn new_from_slice(
        offset: usize,
        len_limit: u64,
        decoded: T,
        space: &dyn CachedStore,
    ) -> Result<Self, ShaleError> {
        Ok(Self {
            offset,
            decoded,
            mem: space.get_shared(),
            len_limit,
        })
    }

    pub fn slice<U: Storable + Debug + Send + Sync + 'static>(
        s: &Obj<T>,
        offset: usize,
        length: u64,
        decoded: U,
    ) -> Result<Obj<U>, ShaleError> {
        let addr_ = s.value.get_offset() + offset;
        if s.dirty.is_some() {
            return Err(ShaleError::InvalidObj {
                addr: offset,
                obj_type: type_name::<T>(),
                error: "dirty write",
            });
        }
        let r = Box::new(StoredView::new_from_slice(
            addr_,
            length,
            decoded,
            s.value.get_mem_store(),
        )?);
        Ok(Obj {
            value: r,
            dirty: None,
        })
    }
}

#[derive(Debug)]
pub struct ObjCacheInner<T: Send + Sync> {
    cached: lru::LruCache<DiskAddress, Obj<T>>,
    pinned: HashMap<DiskAddress, bool>,
    dirty: HashSet<DiskAddress>,
}

/// [ObjRef] pool that is used by [ShaleStore] implementation to construct [ObjRef]s.
#[derive(Debug)]
pub struct ObjCache<T: Send + Sync>(Arc<RwLock<ObjCacheInner<T>>>);

impl<T: Send + Sync> ObjCache<T> {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(RwLock::new(ObjCacheInner {
            cached: lru::LruCache::new(NonZeroUsize::new(capacity).expect("non-zero cache size")),
            pinned: HashMap::new(),
            dirty: HashSet::new(),
        })))
    }

    fn lock(&self) -> RwLockWriteGuard<ObjCacheInner<T>> {
        self.0.write().unwrap()
    }

    #[inline(always)]
    pub fn get<'a>(
        &self,
        inner: &mut ObjCacheInner<T>,
        ptr: DiskAddress,
    ) -> Result<Option<ObjRef<'a, T>>, ShaleError> {
        if let Some(r) = inner.cached.pop(&ptr) {
            // insert and set to `false` if you can
            // When using `get` in parallel, one should not `write` to the same address
            inner
                .pinned
                .entry(ptr)
                .and_modify(|is_pinned| *is_pinned = false)
                .or_insert(false);

            // return if inner.pinned.insert(ptr, false).is_some() {
            //     Err(ShaleError::InvalidObj {
            //         addr: ptr.addr(),
            //         obj_type: type_name::<T>(),
            //         error: "address already in use",
            //     })
            // } else {
            //     Ok(Some(ObjRef {
            //         inner: Some(r),
            //         cache: Self(self.0.clone()),
            //         _life: PhantomData,
            //     }))
            // };

            // always return instead of the code above
            return Ok(Some(ObjRef {
                inner: Some(r),
                cache: Self(self.0.clone()),
                _life: PhantomData,
            }));
        }

        Ok(None)
    }

    #[inline(always)]
    pub fn put<'a>(&self, inner: Obj<T>) -> ObjRef<'a, T> {
        let ptr = inner.as_ptr();
        self.lock().pinned.insert(ptr, false);
        ObjRef {
            inner: Some(inner),
            cache: Self(self.0.clone()),
            _life: PhantomData,
        }
    }

    #[inline(always)]
    pub fn pop(&self, ptr: DiskAddress) {
        let mut inner = self.lock();
        if let Some(f) = inner.pinned.get_mut(&ptr) {
            *f = true
        }
        if let Some(mut r) = inner.cached.pop(&ptr) {
            r.dirty = None
        }
        inner.dirty.remove(&ptr);
    }

    pub fn flush_dirty(&self) -> Option<()> {
        let mut inner = self.lock();
        if !inner.pinned.is_empty() {
            return None;
        }
        for ptr in std::mem::take(&mut inner.dirty) {
            if let Some(r) = inner.cached.peek_mut(&ptr) {
                r.flush_dirty()
            }
        }
        Some(())
    }
}
