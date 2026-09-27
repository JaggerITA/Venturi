//! Maps of project entities keyed by ids that never change: an undo puts
//! back an entity under the id it had, so commands, selections and ids
//! handed to other programs keep pointing at it.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::legacy_slotmap;

pub trait Id: Copy + Ord + std::hash::Hash + fmt::Debug {
    fn from_raw(raw: u64) -> Self;
    fn raw(self) -> u64;
}

macro_rules! id_types {
    ($($(#[$attr:meta])* $name:ident;)*) => {$(
        $(#[$attr])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(u64);

        impl Id for $name {
            fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            fn raw(self) -> u64 {
                self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_u64(self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_any(IdVisitor(PhantomData))
            }
        }
    )*};
}

id_types! {
    MediaId;
    TimelineId;
    FolderId;
}

struct IdVisitor<K>(PhantomData<K>);

impl<'de, K: Id> Visitor<'de> for IdVisitor<K> {
    type Value = K;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an id")
    }

    fn visit_u64<E: de::Error>(self, raw: u64) -> Result<K, E> {
        Ok(K::from_raw(raw))
    }

    fn visit_i64<E: de::Error>(self, raw: i64) -> Result<K, E> {
        u64::try_from(raw)
            .map(K::from_raw)
            .map_err(|_| E::custom("negative id"))
    }

    // LEGACY_SLOTMAP
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<K, A::Error> {
        legacy_slotmap::key(map).map(K::from_raw)
    }
}

/// Iterates in id order, i.e. in insertion order.
#[derive(Clone, Debug)]
pub struct IdMap<K, V> {
    items: BTreeMap<K, V>,
    next: u64,
}

impl<K, V> Default for IdMap<K, V> {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            next: 1,
        }
    }
}

impl<K: Id, V> IdMap<K, V> {
    pub fn insert(&mut self, value: V) -> K {
        let id = self.alloc();
        self.items.insert(id, value);
        id
    }

    /// An id no entity had or will have: for a command that inserts later.
    pub fn alloc(&mut self) -> K {
        let id = K::from_raw(self.next);
        self.next += 1;
        id
    }

    /// Puts back an entity under an id from `alloc`/`insert` of this map.
    pub fn insert_at(&mut self, id: K, value: V) {
        debug_assert!(id.raw() < self.next, "{id:?} was not allocated here");
        let previous = self.items.insert(id, value);
        debug_assert!(previous.is_none(), "{id:?} already present");
    }

    pub fn remove(&mut self, id: K) -> Option<V> {
        self.items.remove(&id)
    }

    pub fn get(&self, id: K) -> Option<&V> {
        self.items.get(&id)
    }

    pub fn get_mut(&mut self, id: K) -> Option<&mut V> {
        self.items.get_mut(&id)
    }

    pub fn contains_key(&self, id: K) -> bool {
        self.items.contains_key(&id)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (K, &V)> + Clone {
        self.items.iter().map(|(&id, value)| (id, value))
    }

    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = (K, &mut V)> {
        self.items.iter_mut().map(|(&id, value)| (id, value))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = K> + Clone + '_ {
        self.items.keys().copied()
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> + Clone {
        self.items.values()
    }

    pub fn values_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut V> {
        self.items.values_mut()
    }
}

impl<K: Id, V> std::ops::Index<K> for IdMap<K, V> {
    type Output = V;

    fn index(&self, id: K) -> &V {
        self.get(id)
            .unwrap_or_else(|| panic!("{id:?} not in the map"))
    }
}

impl<K: Id, V> std::ops::IndexMut<K> for IdMap<K, V> {
    fn index_mut(&mut self, id: K) -> &mut V {
        self.get_mut(id)
            .unwrap_or_else(|| panic!("{id:?} not in the map"))
    }
}

impl<K: Id, V> IntoIterator for IdMap<K, V> {
    type Item = (K, V);
    type IntoIter = std::collections::btree_map::IntoIter<K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl<'a, K: Id, V> IntoIterator for &'a IdMap<K, V> {
    type Item = (K, &'a V);
    type IntoIter = std::iter::Map<
        std::collections::btree_map::Iter<'a, K, V>,
        fn((&'a K, &'a V)) -> (K, &'a V),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter().map(|(&id, value)| (id, value))
    }
}

/// `(next: N, items: {id: value, ...})`. `next` is saved because ids of
/// deleted entities may still be referenced (offline clips).
#[derive(Serialize)]
struct Stored<'a, K: Ord, V> {
    next: u64,
    items: &'a BTreeMap<K, V>,
}

impl<K: Id + Serialize, V: Serialize> Serialize for IdMap<K, V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Stored {
            next: self.next,
            items: &self.items,
        }
        .serialize(serializer)
    }
}

impl<'de, K, V> Deserialize<'de> for IdMap<K, V>
where
    K: Id + Deserialize<'de>,
    V: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(IdMapVisitor(PhantomData))
    }
}

struct IdMapVisitor<K, V>(PhantomData<(K, V)>);

impl<'de, K, V> Visitor<'de> for IdMapVisitor<K, V>
where
    K: Id + Deserialize<'de>,
    V: Deserialize<'de>,
{
    type Value = IdMap<K, V>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a map of ids")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut next = None;
        let mut items: Option<BTreeMap<K, V>> = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "next" => next = Some(map.next_value()?),
                "items" => items = Some(map.next_value()?),
                other => return Err(de::Error::unknown_field(other, &["next", "items"])),
            }
        }
        let items = items.ok_or_else(|| de::Error::missing_field("items"))?;
        let next = next.ok_or_else(|| de::Error::missing_field("next"))?;
        if items.keys().next_back().is_some_and(|id| id.raw() >= next) {
            return Err(de::Error::custom("id not below `next`"));
        }
        Ok(IdMap { items, next })
    }

    // LEGACY_SLOTMAP
    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        let (items, next) = legacy_slotmap::map_from_slots(seq)?;
        Ok(IdMap {
            items: items
                .into_iter()
                .map(|(raw, value)| (K::from_raw(raw), value))
                .collect(),
            next,
        })
    }
}

#[cfg(test)]
#[path = "tests/id_map.rs"]
mod tests;
