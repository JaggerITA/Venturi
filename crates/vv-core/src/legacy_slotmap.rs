//! LEGACY_SLOTMAP: reads projects saved before `IdMap` (2026-09), whose
//! maps were `slotmap::SlotMap`s: a map is a list of `(value, version)`
//! slots, an id is `(idx, version)`.
//!
//! Phase-out: once those projects no longer need to open, delete this
//! module, its tests and fixture, and the `visit_map`/`visit_seq` hooks
//! marked LEGACY_SLOTMAP in `id_map.rs`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess};

/// Keeps the version: a key to a deleted entity whose slot was reused must
/// not resolve to the new occupant. Version 1 (never reused) gives `idx`.
fn id_of(idx: u32, version: u32) -> u64 {
    u64::from(idx) | (u64::from(version >> 1) << 32)
}

#[derive(Deserialize)]
struct Key {
    idx: u32,
    version: u32,
}

pub(crate) fn key<'de, A: MapAccess<'de>>(map: A) -> Result<u64, A::Error> {
    let key = Key::deserialize(de::value::MapAccessDeserializer::new(map))?;
    // `slotmap` stores odd versions only for live keys.
    Ok(id_of(key.idx, key.version | 1))
}

#[derive(Deserialize)]
struct Slot<V> {
    value: Option<V>,
    version: u32,
}

/// The items and the `next` of an `IdMap`. `next` is above every id a key
/// in the file can have, including keys to deleted entities.
pub(crate) fn map_from_slots<'de, V: Deserialize<'de>, A: SeqAccess<'de>>(
    mut seq: A,
) -> Result<(BTreeMap<u64, V>, u64), A::Error> {
    let mut items = BTreeMap::new();
    let mut next = 1;
    let mut idx: u32 = 0;
    while let Some(slot) = seq.next_element::<Slot<V>>()? {
        // Slot 0 is `slotmap`'s sentinel, never occupied.
        if idx > 0 {
            let id = id_of(idx, slot.version | 1);
            next = next.max(id + 1);
            if let Some(value) = slot.value {
                items.insert(id, value);
            }
        }
        idx = idx
            .checked_add(1)
            .ok_or_else(|| de::Error::custom("too many slots"))?;
    }
    Ok((items, next))
}

#[cfg(test)]
#[path = "tests/legacy_slotmap.rs"]
mod tests;
