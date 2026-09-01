use std::{collections::HashMap, fmt::Debug, hash::Hash};

use reth_network::cache::LruCache;

/// A naive implementation of a bi-directional LRU cache.
#[derive(Debug)]
pub struct LruBiMap<K: Hash + Eq + Clone + Debug, V: Hash + Eq + Clone + Debug> {
    left_to_right: HashMap<K, V>,
    right_to_left: HashMap<V, K>,
    lru_keys: LruCache<K>,
}

impl<K: Hash + Eq + Clone + Debug, V: Hash + Eq + Clone + Debug> LruBiMap<K, V> {
    pub fn new(limit: u32) -> Self {
        Self {
            left_to_right: HashMap::new(),
            right_to_left: HashMap::new(),
            lru_keys: LruCache::new(limit),
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        if let Some(old_value) = self.left_to_right.get(&key).cloned()
            && old_value != value
        {
            self.right_to_left.remove(&old_value);
        }
        if let Some(old_key) = self.right_to_left.get(&value).cloned()
            && old_key != key
        {
            self.left_to_right.remove(&old_key);
        }
        if let (true, Some(evicted)) = self.lru_keys.insert_and_get_evicted(key.clone()) {
            self.evict(&evicted);
        }
        self.left_to_right.insert(key.clone(), value.clone());
        self.right_to_left.insert(value.clone(), key.clone());
    }

    pub fn get_by_left(&self, key: &K) -> Option<&V> {
        self.left_to_right.get(key)
    }

    pub fn get_by_right(&self, value: &V) -> Option<&K> {
        self.right_to_left.get(value)
    }

    pub fn remove_by_left(&mut self, key: &K) {
        self.evict(key);
    }

    fn evict(&mut self, key: &K) {
        if let Some(value) = self.left_to_right.remove(key) {
            self.right_to_left.remove(&value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LruBiMap;

    #[test]
    fn replacing_a_value_removes_the_old_key() {
        let mut map = LruBiMap::new(4);
        map.insert("old", 1);
        map.insert("new", 1);

        assert_eq!(map.get_by_left(&"old"), None);
        assert_eq!(map.get_by_left(&"new"), Some(&1));
        assert_eq!(map.get_by_right(&1), Some(&"new"));
    }

    #[test]
    fn replacing_a_key_removes_the_old_value() {
        let mut map = LruBiMap::new(4);
        map.insert("block", 1);
        map.insert("block", 2);

        assert_eq!(map.get_by_right(&1), None);
        assert_eq!(map.get_by_right(&2), Some(&"block"));
    }
}
