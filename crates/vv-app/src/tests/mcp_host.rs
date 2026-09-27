use super::*;
use tokio::sync::oneshot::error::TryRecvError;
use vv_mcp::{ImportMediaArgs, McpHandle, SetClipPropertiesArgs, TimelineArgs, ToolResult};

fn attached() -> (VenturiApp, McpHandle) {
    let mut app = VenturiApp::default();
    let (handle, inbox) = vv_mcp::channel(|| {});
    app.mcp = Some(McpHost::for_test(inbox));
    (app, handle)
}

/// Submits `call` and gives the host one frame to answer it.
fn call(app: &mut VenturiApp, handle: &McpHandle, call: ToolCall) -> ToolResult {
    let mut reply = handle.submit(call);
    app.poll_mcp(&egui::Context::default());
    reply.try_recv().expect("answered in the same frame")
}

/// Keeps polling until the call is answered, like the event loop.
fn call_and_wait(app: &mut VenturiApp, handle: &McpHandle, call: ToolCall) -> ToolResult {
    let ctx = egui::Context::default();
    let mut reply = handle.submit(call);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        app.poll_session(&ctx);
        app.poll_mcp(&ctx);
        match reply.try_recv() {
            Ok(result) => return result,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Closed) => panic!("the host dropped the call"),
        }
        assert!(std::time::Instant::now() < deadline, "never answered");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn opacity(timeline_id: TimelineId, clip: ClipId, value: f32) -> ToolCall {
    ToolCall::SetClipProperties(SetClipPropertiesArgs {
        timeline_id: timeline_id_string(timeline_id),
        clip_ids: vec![clip.0.to_string()],
        opacity: Some(value),
        position: None,
        scale: None,
        rotation: None,
        gain_db: None,
        disabled: None,
        fade_in: None,
        fade_out: None,
        fill_color: None,
    })
}

fn timeline_id_string(id: TimelineId) -> String {
    use slotmap::Key;
    id.data().as_ffi().to_string()
}

fn solid_clip(app: &mut VenturiApp) -> (TimelineId, ClipId) {
    let timeline = app.ensure_timeline();
    let clip = vv_core::edit::insert_generator(
        &mut app.session.project,
        &mut app.session.history,
        timeline,
        vv_core::edit::Generator::SolidColor,
        0,
        0,
        Some(50),
    );
    (timeline, clip)
}

#[test]
fn get_state_reports_what_the_user_sees() {
    let (mut app, handle) = attached();
    let (timeline, clip) = solid_clip(&mut app);
    app.timeline_state.playhead = 12;
    app.timeline_state
        .set_selection(BTreeSet::from([(0, clip)]), Some((0, clip)));

    let state = call(&mut app, &handle, ToolCall::GetState).unwrap().value;

    assert_eq!(state["active_timeline"]["id"], timeline_id_string(timeline));
    assert_eq!(state["playhead"], 12);
    assert_eq!(
        state["selected_clips"],
        serde_json::json!([clip.0.to_string()])
    );
}

#[test]
fn edits_wait_for_the_users_gesture_and_stay_a_separate_undo_step() {
    let (mut app, handle) = attached();
    let (timeline, clip) = solid_clip(&mut app);
    let ctx = egui::Context::default();
    // The user is dragging a slider: their edits so far form one group.
    app.edit_drag_group = Some(app.session.history.begin_group());
    let before = app.session.history.position();

    let mut edit = handle.submit(opacity(timeline, clip, 40.0));
    app.poll_mcp(&ctx);
    assert!(matches!(edit.try_recv(), Err(TryRecvError::Empty)));
    assert!(
        call(&mut app, &handle, ToolCall::GetProject).is_ok(),
        "reading is not held"
    );

    let mark = app.edit_drag_group.take().unwrap();
    app.session.history.end_group(mark);
    app.poll_mcp(&ctx);
    assert!(edit.try_recv().unwrap().is_ok());
    assert_eq!(app.session.history.position(), before + 1);
    let clip = app.session.project.timelines[timeline]
        .clip(0, clip)
        .unwrap();
    assert_eq!(
        clip.effects
            .transform
            .track(vv_core::TransformParam::Opacity)
            .default,
        40.0
    );
}

#[test]
fn edits_are_refused_while_a_dialog_waits_for_the_user() {
    let (mut app, handle) = attached();
    let (timeline, clip) = solid_clip(&mut app);
    app.pending_project_switch = Some(ProjectSwitch::Quit);

    let refused = call(&mut app, &handle, opacity(timeline, clip, 40.0)).unwrap_err();
    assert!(refused.0.contains("dialog open"), "{refused}");
    assert!(call(&mut app, &handle, ToolCall::GetProject).is_ok());
}

#[test]
fn the_agent_cannot_replace_a_project_with_unsaved_changes() {
    let (mut app, handle) = attached();
    solid_clip(&mut app);

    let refused = call(&mut app, &handle, ToolCall::NewProject).unwrap_err();
    assert!(refused.0.contains("unsaved changes"), "{refused}");
    assert!(app.timeline_id.is_some());

    app.session.mark_saved();
    assert!(call(&mut app, &handle, ToolCall::NewProject).is_ok());
    assert_eq!(app.timeline_id, None, "the editor follows the new project");
}

#[test]
fn an_agent_import_does_not_move_the_users_view() {
    let dir = std::env::temp_dir().join("vv-app-mcp-import");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.mp4");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=25:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ],
        &path,
    );
    let (mut app, handle) = attached();
    app.import_warnings = vec!["the user's".into()];

    let result = call_and_wait(
        &mut app,
        &handle,
        ToolCall::ImportMedia(ImportMediaArgs {
            paths: vec![path.display().to_string()],
        }),
    )
    .unwrap();

    assert_eq!(result.value["media"][0]["name"], "a.mp4");
    assert_eq!(app.timeline_id, None, "no timeline created for the agent");
    assert_eq!(app.browsing_media, None);
    assert!(app.media_pool_state.selected.is_empty());
    assert_eq!(app.import_warnings, ["the user's"]);
}

#[test]
fn set_active_timeline_opens_it_in_the_editor() {
    let (mut app, handle) = attached();
    let first = app.ensure_timeline();
    let second = app.create_timeline("Second".into(), vv_core::Rational::new(25, 1), (64, 48));
    assert_eq!(app.timeline_id, Some(first));

    let args = TimelineArgs {
        timeline_id: timeline_id_string(second),
    };
    call(&mut app, &handle, ToolCall::SetActiveTimeline(args)).unwrap();

    assert_eq!(app.timeline_id, Some(second));
}

#[test]
fn screenshot_answers_with_the_next_captured_frame() {
    let (mut app, handle) = attached();
    let mut reply = handle.submit(ToolCall::ScreenshotUi);
    app.poll_mcp(&egui::Context::default());
    assert!(matches!(reply.try_recv(), Err(TryRecvError::Empty)));

    let image = egui::ColorImage::new([4, 2], vec![egui::Color32::RED; 8]);
    app.mcp_screenshot(&image);

    let output = reply.try_recv().unwrap().unwrap();
    assert_eq!(output.value["width"], 4);
    assert!(output.image_png.is_some());
}

#[test]
fn an_agent_export_shows_in_the_progress_window() {
    let dir = std::env::temp_dir().join("vv-app-mcp-export");
    std::fs::create_dir_all(&dir).unwrap();
    let (mut app, handle) = attached();
    let (timeline, _) = solid_clip(&mut app);

    let started = call(
        &mut app,
        &handle,
        ToolCall::Export(vv_mcp::ExportArgs {
            timeline_id: timeline_id_string(timeline),
            path: dir.join("out.mp4").display().to_string(),
            range: Some([0, 5]),
            scale_percent: Some(10),
            audio: false,
        }),
    )
    .unwrap();

    let job: vv_session::JobId = started.value["job_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(app.export.as_ref().map(|e| e.job), Some(job));
}
