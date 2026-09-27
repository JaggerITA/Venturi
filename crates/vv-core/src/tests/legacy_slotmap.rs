use crate::model::*;

/// Saved with `slotmap`: media `a` was deleted and its slot reused by `c`;
/// the Main timeline has clips of a, b, c and of the compound Nested.
const PROJECT: &str = include_str!("fixtures/legacy_slotmap_project.ron");

fn media_by_path(project: &Project, path: &str) -> MediaId {
    project
        .media_pool
        .iter()
        .find(|(_, item)| item.path.to_str() == Some(path))
        .map(|(id, _)| id)
        .unwrap()
}

fn main_clip_sources(project: &Project) -> Vec<MediaId> {
    let main = project
        .timelines
        .values()
        .find(|t| t.name == "Main")
        .unwrap();
    main.tracks[0]
        .clips
        .iter()
        .map(|clip| match clip.source {
            ClipSource::Media(id) => id,
            _ => panic!("media clip expected"),
        })
        .collect()
}

#[test]
fn a_slotmap_project_keeps_its_references() {
    let project: Project = ron::from_str(PROJECT).unwrap();
    let b = media_by_path(&project, "/m/b.mp4");
    let c = media_by_path(&project, "/m/c.mp4");
    let nested = media_by_path(&project, "Nested");

    let sources = main_clip_sources(&project);
    assert_eq!(sources[1..], [b, c, nested]);
    let nested_timeline = project.media_pool[nested].compound.unwrap();
    assert_eq!(project.timelines[nested_timeline].name, "Nested");
    let main_item = media_by_path(&project, "Main");
    let main_timeline = project.media_pool[main_item].compound.unwrap();
    assert_eq!(project.timelines[main_timeline].name, "Main");

    let folder_of = |id: MediaId| project.folders[project.media_pool[id].folder.unwrap()].clone();
    assert_eq!(folder_of(c).name, "F1");
    assert_eq!(folder_of(b).name, "F2");
    let f1 = project.media_pool[c].folder.unwrap();
    assert_eq!(folder_of(b).parent, Some(f1));
}

#[test]
fn a_clip_of_a_media_deleted_before_saving_stays_offline() {
    let mut project: Project = ron::from_str(PROJECT).unwrap();
    let deleted = main_clip_sources(&project)[0];
    assert!(!project.media_pool.contains_key(deleted));

    let item = project.media_pool.values().next().unwrap().clone();
    let new_media = project.media_pool.insert(item);

    assert_ne!(new_media, deleted);
    assert!(!project.media_pool.contains_key(deleted));
}

#[test]
fn a_migrated_project_is_saved_in_the_current_format() {
    let project: Project = ron::from_str(PROJECT).unwrap();
    let text = ron::ser::to_string_pretty(&project, ron::ser::PrettyConfig::default()).unwrap();
    assert!(!text.contains("version"));

    let reloaded: Project = ron::from_str(&text).unwrap();
    assert_eq!(main_clip_sources(&reloaded), main_clip_sources(&project));
    assert_eq!(
        reloaded.media_pool.keys().collect::<Vec<_>>(),
        project.media_pool.keys().collect::<Vec<_>>()
    );
}
