use std::borrow::Borrow;
use std::hash::{Hash, Hasher};

pub mod buffer_poll_manager;
mod buffer_poll_manager1;
mod lru_k_replacer;
mod lru_replacer;

pub type FrameId = usize;

struct KeyRef<K> {
    k: *const K,
}

impl<K: Hash> Hash for KeyRef<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        unsafe { (*self.k).hash(state) }
    }
}

impl<K: PartialEq> PartialEq for KeyRef<K> {
    fn eq(&self, other: &Self) -> bool {
        unsafe { (*self.k).eq(&*other.k) }
    }
}

impl<K: Eq> Eq for KeyRef<K> {}

#[repr(transparent)]
struct KeyWrapper<K: ?Sized>(K);

impl<K: ?Sized> KeyWrapper<K> {
    fn from_ref(k: &K) -> &KeyWrapper<K> {
        unsafe { &*(k as *const K as *const KeyWrapper<K>) }
    }
}

impl<K: Hash> Hash for KeyWrapper<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl<K: PartialEq> PartialEq for KeyWrapper<K> {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl<K: Eq> Eq for KeyWrapper<K> {}

impl<K, Q> Borrow<KeyWrapper<Q>> for KeyRef<K>
where
    K: Borrow<Q>,
    Q: ?Sized,
{
    fn borrow(&self) -> &KeyWrapper<Q> {
        let key = unsafe { &*self.k }.borrow();
        KeyWrapper::from_ref(key)
    }
}
