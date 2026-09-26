use super::*;

fn timeline_item(app: &VenturiApp, timeline_id: vv_core::TimelineId) -> MediaId {
    app.project
        .media_pool
        .iter()
        .find(|(_, item)| item.compound == Some(timeline_id))
        .map(|(id, _)| id)
        .expect("every timeline has a pool item")
}

#[test]
fn duplicating_a_timeline_twice_numbers_the_copies() {
    let mut app = VenturiApp::default();
    let timeline_id = app.ensure_timeline();
    let source = timeline_item(&app, timeline_id);
    let base = format!(
        "{} {}",
        file_label(&app.project.media_pool[source].path),
        t!("pool.copy_suffix")
    );

    app.duplicate_timeline(source);
    app.duplicate_timeline(source);

    let mut names: Vec<String> = app
        .project
        .media_pool
        .values()
        .filter(|item| item.compound.is_some())
        .map(|item| file_label(&item.path))
        .collect();
    names.sort();
    assert_eq!(names[1..], [base.clone(), format!("{base} 2")]);
    assert_eq!(app.project.timelines.len(), 3);
    assert!(app.has_unsaved_changes());
}

#[test]
fn renaming_to_the_same_name_changes_nothing() {
    let mut app = VenturiApp::default();
    let timeline_id = app.ensure_timeline();
    let item = timeline_item(&app, timeline_id);
    app.unsaved_media = false;
    let name = file_label(&app.project.media_pool[item].path);
    app.rename_timeline(item, name);
    assert!(!app.unsaved_media);
    app.rename_timeline(item, "Edit".into());
    assert_eq!(app.project.timelines[timeline_id].name, "Edit");
    assert!(app.unsaved_media);
}
