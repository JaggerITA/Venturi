use super::*;

#[test]
fn ids_are_never_reused() {
    let mut map: IdMap<MediaId, &str> = IdMap::default();
    let a = map.insert("a");
    map.remove(a);
    let b = map.insert("b");

    assert_ne!(a, b);
    assert!(!map.contains_key(a));
}

#[test]
fn insert_at_puts_an_entity_back_under_its_id() {
    let mut map: IdMap<MediaId, &str> = IdMap::default();
    let a = map.insert("a");
    let b = map.insert("b");
    let removed = map.remove(a).unwrap();

    map.insert_at(a, removed);

    assert_eq!(map[a], "a");
    assert_eq!(map.keys().collect::<Vec<_>>(), vec![a, b]);
}

#[test]
fn alloc_reserves_an_id_that_insert_skips() {
    let mut map: IdMap<FolderId, &str> = IdMap::default();
    let reserved = map.alloc();
    let other = map.insert("other");
    map.insert_at(reserved, "reserved");

    assert_ne!(reserved, other);
    assert_eq!(map[reserved], "reserved");
}

#[test]
fn a_saved_map_keeps_the_ids_of_deleted_entities_unused() {
    let mut map: IdMap<MediaId, String> = IdMap::default();
    map.insert("a".into());
    let deleted = map.insert("b".into());
    map.remove(deleted);

    let text = ron::to_string(&map).unwrap();
    let mut loaded: IdMap<MediaId, String> = ron::from_str(&text).unwrap();

    assert_eq!(loaded.len(), 1);
    assert_ne!(loaded.insert("c".into()), deleted);
}

#[test]
fn an_id_at_or_above_next_is_refused() {
    let text = "(next: 2, items: {2: \"x\"})";
    assert!(ron::from_str::<IdMap<MediaId, String>>(text).is_err());
}
