use super::*;
use std::path::PathBuf;
use vv_core::{Clip, ClipId, Interpolation, MediaItem, MediaMeta, Rational, Track, TrackKind};

const RATE: u32 = 100;

fn clip_at(media: vv_core::MediaId, start: FrameIdx, source_in: FrameIdx, len: FrameIdx) -> Clip {
    Clip::from_source_range(
        ClipId(0),
        ClipSource::Media(media),
        source_in,
        source_in + len,
        start,
        Rational::one(),
    )
}

/// Project at 10 fps with two media (`a.wav`, `b.wav`): at `RATE` = 100
/// every timeline frame is 10 audio frames.
fn project() -> (Project, vv_core::MediaId, vv_core::MediaId) {
    let mut project = Project::default();
    let meta = MediaMeta {
        duration_frames: 100,
        fps: Rational::new(10, 1),
        width: 0,
        height: 0,
        has_video: true,
        has_audio: true,
        sample_rate: RATE,
        channels: 1,
        audio_streams: 1,
        file: Default::default(),
    };
    let a = project.media_pool.insert(MediaItem {
        path: PathBuf::from("a.wav"),
        meta: meta.clone(),
        content_hash: 1,
        compound: None,
        folder: None,
    });
    let b = project.media_pool.insert(MediaItem {
        path: PathBuf::from("b.wav"),
        meta,
        content_hash: 2,
        compound: None,
        folder: None,
    });
    (project, a, b)
}

fn timeline(tracks: Vec<Track>) -> Timeline {
    Timeline {
        name: "t".into(),
        fps: Rational::new(10, 1),
        resolution: (4, 2),
        tracks,
        markers: Vec::new(),
        master: Default::default(),
    }
}

fn audio_track(clips: Vec<Clip>) -> Track {
    Track {
        kind: TrackKind::Audio,
        clips,
        muted: false,
        solo: false,
        locked: false,
        crossings: Vec::new(),
        mix: Default::default(),
        armed: Default::default(),
    }
}

/// Mono: `a` is always 0.5, `b` is a ramp (sample i = i/1000);
/// stream 1 of `a` is 0.25.
fn buffers(path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
    match (path.to_str()?, stream) {
        ("a.wav", 0) => Some(Arc::new(vec![0.5; 1000])),
        ("a.wav", 1) => Some(Arc::new(vec![0.25; 1000])),
        ("b.wav", 0) => Some(Arc::new((0..1000).map(|i| i as f32 / 1000.0).collect())),
        _ => None,
    }
}

fn render(project: &Project, tl: &Timeline, start: u64, frames: usize) -> Vec<f32> {
    let snap = MixSnapshot::from_timeline(project, tl, RATE, 1, &mut buffers);
    let mut out = vec![9.0; frames];
    mix_range(&snap, start, &mut out);
    out
}

#[test]
fn gap_is_silence() {
    let (project, a, _) = project();
    let tl = timeline(vec![audio_track(vec![clip_at(a, 5, 0, 2)])]);
    let out = render(&project, &tl, 0, 50);
    assert!(out.iter().all(|&s| s == 0.0));
    let empty = timeline(vec![]);
    assert!(render(&project, &empty, 0, 10).iter().all(|&s| s == 0.0));
}

#[test]
fn offset_clip_plays_its_source_range_at_its_timeline_position() {
    let (project, _, b) = project();
    // Timeline frame 2 (= audio 20) plays the source from frame 3 (= audio 30).
    let tl = timeline(vec![audio_track(vec![clip_at(b, 2, 3, 1)])]);
    let out = render(&project, &tl, 15, 20);
    assert!(out[..5].iter().all(|&s| s == 0.0));
    for i in 0..10 {
        assert_eq!(out[5 + i], (30 + i) as f32 / 1000.0);
    }
    assert!(out[15..].iter().all(|&s| s == 0.0));
}

#[test]
fn overlapping_clips_on_different_tracks_are_summed() {
    let (project, a, b) = project();
    let tl = timeline(vec![
        audio_track(vec![clip_at(a, 0, 0, 5)]),
        audio_track(vec![clip_at(b, 0, 0, 5)]),
    ]);
    let out = render(&project, &tl, 10, 5);
    for (i, s) in out.iter().enumerate() {
        assert_eq!(*s, 0.5 + (10 + i) as f32 / 1000.0);
    }
}

#[test]
fn muted_track_is_excluded() {
    let (project, a, b) = project();
    let mut muted = audio_track(vec![clip_at(a, 0, 0, 5)]);
    muted.muted = true;
    let tl = timeline(vec![muted, audio_track(vec![clip_at(b, 0, 0, 5)])]);
    let out = render(&project, &tl, 0, 5);
    for (i, s) in out.iter().enumerate() {
        assert_eq!(*s, i as f32 / 1000.0);
    }
}

#[test]
fn only_solo_tracks_play_when_any_is_solo() {
    let (project, a, b) = project();
    let mut solo = audio_track(vec![clip_at(b, 0, 0, 5)]);
    solo.solo = true;
    let tl = timeline(vec![audio_track(vec![clip_at(a, 0, 0, 5)]), solo]);
    let out = render(&project, &tl, 0, 5);
    for (i, s) in out.iter().enumerate() {
        assert_eq!(*s, i as f32 / 1000.0);
    }
}

#[test]
fn disabled_clip_is_excluded() {
    let (project, a, b) = project();
    let mut disabled = clip_at(a, 0, 0, 5);
    disabled.disabled = true;
    let tl = timeline(vec![
        audio_track(vec![disabled]),
        audio_track(vec![clip_at(b, 0, 0, 5)]),
    ]);
    let out = render(&project, &tl, 0, 5);
    for (i, s) in out.iter().enumerate() {
        assert_eq!(*s, i as f32 / 1000.0);
    }
}

#[test]
fn video_tracks_are_ignored() {
    let (project, a, _) = project();
    let mut video = audio_track(vec![clip_at(a, 0, 0, 5)]);
    video.kind = TrackKind::Video;
    let tl = timeline(vec![video]);
    assert!(render(&project, &tl, 0, 20).iter().all(|&s| s == 0.0));
}

/// `project()` plus a compound clip whose nested timeline plays `a.wav`
/// for 5 frames.
fn project_with_compound() -> (Project, vv_core::MediaId) {
    let (mut project, a, _) = project();
    let nested = project
        .timelines
        .insert(timeline(vec![audio_track(vec![clip_at(a, 0, 0, 5)])]));
    let compound = project.media_pool.insert(MediaItem {
        path: PathBuf::from("Compound Clip 1"),
        meta: project.media_pool[a].meta.clone(),
        content_hash: 10,
        compound: Some(nested),
        folder: None,
    });
    (project, compound)
}

/// Answers from `buffers`, counting the calls; `partial` answers `Partial`
/// instead of `Ready`, as for a file still decoding.
#[derive(Default)]
struct CountingSource {
    files_asked: Vec<PathBuf>,
    partial: bool,
    stored: Vec<MediaId>,
    cache: Option<Arc<Vec<f32>>>,
}

impl AudioSource for CountingSource {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        self.files_asked.push(path.to_path_buf());
        match buffers(path, stream) {
            Some(b) if self.partial => ClipAudio::Partial(b),
            Some(b) => ClipAudio::Ready(b),
            None => ClipAudio::Missing,
        }
    }

    fn cached_compound(&mut self, _: MediaId, _: u64) -> Option<Arc<Vec<f32>>> {
        self.cache.clone()
    }

    fn store_compound(&mut self, media_id: MediaId, _: u64, mixdown: Arc<Vec<f32>>) {
        self.stored.push(media_id);
        self.cache = Some(mixdown);
    }
}

#[test]
fn a_compound_clip_plays_its_nested_timeline_and_never_asks_for_its_own_file() {
    let (project, compound) = project_with_compound();
    let tl = timeline(vec![audio_track(vec![clip_at(compound, 0, 0, 5)])]);
    let mut source = CountingSource::default();
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut source);
    assert_eq!(source.files_asked, [PathBuf::from("a.wav")]);
    let mut out = vec![9.0; 60];
    mix_range(&snap, 0, &mut out);
    assert!(out[..50].iter().all(|&s| s == 0.5));
    assert!(out[50..].iter().all(|&s| s == 0.0));
}

#[test]
fn a_compound_used_twice_is_mixed_once() {
    let (project, compound) = project_with_compound();
    let tl = timeline(vec![audio_track(vec![
        clip_at(compound, 0, 0, 5),
        clip_at(compound, 10, 0, 5),
    ])]);
    let mut source = CountingSource::default();
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut source);
    assert_eq!(snap.clips.len(), 2);
    assert_eq!(source.stored, [compound]);
    assert_eq!(
        source.files_asked.len(),
        1,
        "the second use comes from the cache"
    );
}

/// A mixdown built on a file still decoding plays, but caching it would
/// keep the missing tail missing until `content_hash` changes.
#[test]
fn a_compound_over_a_partial_file_plays_but_is_not_cached() {
    let (project, compound) = project_with_compound();
    let mut source = CountingSource {
        partial: true,
        ..Default::default()
    };
    assert!(matches!(
        compound_mixdown(&project, compound, RATE, 1, &mut source),
        ClipAudio::Partial(_)
    ));
    assert!(source.stored.is_empty());

    source.partial = false;
    assert!(matches!(
        compound_mixdown(&project, compound, RATE, 1, &mut source),
        ClipAudio::Ready(_)
    ));
    assert_eq!(source.stored, [compound]);
}

#[test]
fn a_compound_over_a_pending_file_is_silent_and_not_cached() {
    let (project, compound) = project_with_compound();
    assert!(matches!(
        compound_mixdown(&project, compound, RATE, 1, &mut AllPending),
        ClipAudio::Pending
    ));
}

struct AllPending;

impl AudioSource for AllPending {
    fn file(&mut self, _: &Path, _: usize) -> ClipAudio {
        ClipAudio::Pending
    }
}

/// Safety net: `would_create_a_cycle` prevents it, but a compound containing
/// itself must end silent, not overflow the stack.
#[test]
fn a_compound_containing_itself_is_silent() {
    let (mut project, compound) = project_with_compound();
    let nested = project.media_pool[compound].compound.unwrap();
    project.timelines[nested]
        .tracks
        .push(audio_track(vec![clip_at(compound, 0, 0, 5)]));
    let tl = timeline(vec![audio_track(vec![clip_at(compound, 0, 0, 5)])]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut buffers);
    assert!(snap.clips.is_empty());
}

/// The bug case: media at 9.99 fps (10000/1001, the small-scale analogue
/// of 59.94 on 60) on a 10 fps timeline. Conformed, the clip
/// lasts on the timeline as long as its audio does, and the mix at the end of the clip is
/// still aligned to the right sample instead of being cut off.
#[test]
fn a_conformed_clip_lasts_as_long_as_its_audio_and_does_not_drift() {
    const SOURCE_FRAMES: FrameIdx = 1000;
    const AUDIO_SAMPLES: usize = 10_010; // 1000 frames / 9.99 fps = 100.1 s

    let mut project = Project::default();
    let media = project.media_pool.insert(MediaItem {
        path: PathBuf::from("slow.wav"),
        meta: MediaMeta {
            duration_frames: SOURCE_FRAMES,
            fps: Rational::new(10_000, 1001),
            width: 0,
            height: 0,
            has_video: true,
            has_audio: true,
            sample_rate: RATE,
            channels: 1,
            audio_streams: 1,
            file: Default::default(),
        },
        content_hash: 7,
        compound: None,
        folder: None,
    });
    let rate = Rational::conform_rate(Rational::new(10, 1), Rational::new(10_000, 1001));
    let clip = Clip::from_source_range(
        ClipId(0),
        ClipSource::Media(media),
        0,
        SOURCE_FRAMES,
        0,
        rate,
    );
    assert_eq!(clip.timeline_len, 1001, "100,1 s a 10 fps");
    let tl = timeline(vec![audio_track(vec![clip])]);

    let buffer: Arc<Vec<f32>> =
        Arc::new((0..AUDIO_SAMPLES).map(|i| i as f32 / 100_000.0).collect());
    let buffer_for =
        |path: &Path, _stream: usize| (path.to_str() == Some("slow.wav")).then(|| buffer.clone());
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut |p: &Path, s: usize| {
        buffer_for(p, s)
    });
    assert_eq!(snap.clips.len(), 1);
    assert_eq!(
        snap.clips[0].len, AUDIO_SAMPLES as u64,
        "all of the media audio fits in the clip, nothing cut"
    );

    // Last 10 samples of the clip: still the ones at the end of the buffer,
    // no drift accumulated over the preceding 100 s.
    let mut out = vec![9.0; 10];
    mix_range(&snap, AUDIO_SAMPLES as u64 - 10, &mut out);
    for (i, s) in out.iter().enumerate() {
        let expected = (AUDIO_SAMPLES - 10 + i) as f32 / 100_000.0;
        assert!(
            (s - expected).abs() < 1e-6,
            "campione {i}: {s} != {expected}"
        );
    }
}

/// A split in the middle of a source frame does not move the audio: the right
/// half restarts from the sample the original was playing at that point.
#[test]
fn splitting_a_conformed_clip_mid_source_frame_keeps_every_sample() {
    let mut project = Project::default();
    let media = project.media_pool.insert(MediaItem {
        path: PathBuf::from("slow.wav"),
        meta: MediaMeta {
            duration_frames: 1000,
            fps: Rational::new(10_000, 1001),
            width: 0,
            height: 0,
            has_video: true,
            has_audio: true,
            sample_rate: RATE,
            channels: 1,
            audio_streams: 1,
            file: Default::default(),
        },
        content_hash: 7,
        compound: None,
        folder: None,
    });
    let rate = Rational::conform_rate(Rational::new(10, 1), Rational::new(10_000, 1001));
    let clip = Clip::from_source_range(ClipId(1), ClipSource::Media(media), 0, 1000, 0, rate);
    assert_eq!(
        clip.source_frame_at(500),
        clip.source_frame_at(499),
        "500 is mid-frame"
    );
    let timeline_id = project
        .timelines
        .insert(timeline(vec![audio_track(vec![clip])]));

    let buffer: Arc<Vec<f32>> = Arc::new((0..10_010).map(|i| i as f32 / 100_000.0).collect());
    let buffer_for =
        |path: &Path, _stream: usize| (path.to_str() == Some("slow.wav")).then(|| buffer.clone());
    let mix_all = |project: &Project| {
        let timeline = &project.timelines[timeline_id];
        let snap =
            MixSnapshot::from_timeline(project, timeline, RATE, 1, &mut |p: &Path, s: usize| {
                buffer_for(p, s)
            });
        let mut out = vec![0.0; 10_010];
        mix_range(&snap, 0, &mut out);
        out
    };
    let before = mix_all(&project);

    let mut split = vv_core::SplitClip::new(timeline_id, 0, ClipId(1), 500);
    vv_core::Command::apply(&mut split, &mut project);
    assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 2);

    assert_eq!(mix_all(&project), before);
}

#[test]
fn constant_gain_scales_the_clip() {
    let (project, a, _) = project();
    let mut clip = clip_at(a, 0, 0, 5);
    clip.effects.gain_db = Keyframed::constant(-6.0206);
    let tl = timeline(vec![audio_track(vec![clip])]);
    for s in render(&project, &tl, 0, 10) {
        assert!((s - 0.25).abs() < 1e-3, "s={s}");
    }
}

#[test]
fn keyframed_gain_is_evaluated_per_block_at_the_source_frame() {
    let (project, a, _) = project();
    // Long enough to cover several gain blocks.
    let blocks = 3;
    let len_frames = (GAIN_BLOCK_FRAMES * blocks) as usize;
    let mut clip = clip_at(a, 0, 0, (len_frames / 10) as FrameIdx);
    let mut gain = Keyframed::constant(0.0f32);
    gain.upsert(0, 0.0, Interpolation::Hold);
    // From the second block (audio 800 = source frame 80) on: practically -inf.
    gain.upsert(80, -200.0, Interpolation::Hold);
    clip.effects.gain_db = gain;
    let tl = timeline(vec![audio_track(vec![clip])]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut |_: &Path, _: usize| {
        Some(Arc::new(vec![0.5; len_frames]))
    });
    let mut out = vec![0.0; len_frames];
    mix_range(&snap, 0, &mut out);
    let block = GAIN_BLOCK_FRAMES as usize;
    assert!(out[..block].iter().all(|&s| s == 0.5));
    assert!(out[block..].iter().all(|&s| s.abs() < 1e-6));
}

#[test]
fn audio_stream_index_selects_the_buffer() {
    let (project, a, _) = project();
    let mut clip = clip_at(a, 0, 0, 5);
    clip.audio_stream_index = 1;
    let tl = timeline(vec![audio_track(vec![clip])]);
    assert!(render(&project, &tl, 0, 10).iter().all(|&s| s == 0.25));
}

#[test]
fn clip_whose_buffer_is_not_ready_is_silent() {
    let (project, a, b) = project();
    let tl = timeline(vec![
        audio_track(vec![clip_at(a, 0, 0, 5)]),
        audio_track(vec![clip_at(b, 0, 0, 5)]),
    ]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut |p: &Path, s: usize| {
        (p != Path::new("b.wav")).then(|| buffers(p, s)).flatten()
    });
    let mut out = vec![0.0; 10];
    mix_range(&snap, 0, &mut out);
    assert!(out.iter().all(|&s| s == 0.5));
}

#[test]
fn clip_longer_than_its_buffer_is_clamped() {
    let (project, a, _) = project();
    let tl = timeline(vec![audio_track(vec![clip_at(a, 0, 95, 20)])]);
    let out = render(&project, &tl, 0, 100);
    assert!(out[..50].iter().all(|&s| s == 0.5));
    assert!(out[50..].iter().all(|&s| s == 0.0));
}

#[test]
fn mix_is_independent_of_how_the_range_is_split() {
    let (project, a, b) = project();
    let mut ca = clip_at(a, 1, 0, 30);
    let mut kf = Keyframed::constant(0.0f32);
    kf.upsert(0, 0.0, Interpolation::Linear);
    kf.upsert(30, -12.0, Interpolation::Linear);
    ca.effects.gain_db = kf;
    let tl = timeline(vec![
        audio_track(vec![ca]),
        audio_track(vec![clip_at(b, 7, 2, 20)]),
    ]);
    let whole = render(&project, &tl, 0, 400);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut buffers);
    let mut chunked = vec![0.0; 400];
    for (i, chunk) in chunked.chunks_mut(37).enumerate() {
        mix_range(&snap, (i * 37) as u64, chunk);
    }
    assert_eq!(whole, chunked);
}

#[test]
fn stereo_mix_keeps_channels_interleaved() {
    let (project, a, _) = project();
    let tl = timeline(vec![audio_track(vec![clip_at(a, 1, 0, 1)])]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 2, &mut |_: &Path, _: usize| {
        Some(Arc::new([0.1, 0.9].repeat(100)))
    });
    let mut out = vec![0.0; 40];
    mix_range(&snap, 5, &mut out);
    assert!(out[..10].iter().all(|&s| s == 0.0));
    for f in out[10..30].chunks(2) {
        assert_eq!(f, [0.1, 0.9]);
    }
    assert!(out[30..].iter().all(|&s| s == 0.0));
}

#[test]
fn mixer_seek_and_snapshot_swaps_do_not_reopen_or_leak() {
    let mut mixer = Mixer::new().unwrap();
    mixer.seek(12_345);
    assert_eq!(
        mixer.position(),
        12_345,
        "paused, the position does not advance"
    );
    for _ in 0..50 {
        let mix = Arc::new(MixSnapshot::empty(mixer.sample_rate(), mixer.channels()));
        mixer.set_state(Arc::new(MixerState::new(mix, None)));
    }
    assert!(
        mixer.retained.len() <= 3,
        "retained={}",
        mixer.retained.len()
    );
}

fn window(tempo: u64, origin: u64, chunks: &[&[f32]]) -> StretchedWindow {
    StretchedWindow {
        tempo,
        origin,
        chunks: chunks.iter().map(|c| Arc::new(c.to_vec())).collect(),
    }
}

#[test]
fn stretched_window_maps_timeline_position_to_stretched_frames() {
    let w = window(4, 100, &[&[1.0, 2.0, 3.0], &[4.0, 5.0]]);
    assert_eq!(w.covered_until(1), 120);

    let mut out = [9.0; 3];
    render_stretched(&w, 1, 100, &mut out);
    assert_eq!(out, [1.0, 2.0, 3.0]);

    // Position 110 = stretched frame 2, across the chunk boundary.
    render_stretched(&w, 1, 110, &mut out);
    assert_eq!(out, [3.0, 4.0, 5.0]);

    render_stretched(&w, 1, 116, &mut out);
    assert_eq!(out, [5.0, 0.0, 0.0], "past the end: silence");
    render_stretched(&w, 1, 50, &mut out);
    assert_eq!(out, [0.0; 3], "before the origin: silence");
}

#[test]
fn stretched_window_keeps_stereo_frames_aligned() {
    let w = window(2, 0, &[&[0.1, 0.2, 0.3, 0.4], &[0.5, 0.6]]);
    assert_eq!(w.covered_until(2), 6);
    let mut out = [0.0; 4];
    render_stretched(&w, 2, 2, &mut out);
    assert_eq!(out, [0.3, 0.4, 0.5, 0.6]);
}

#[test]
fn sample_and_frame_conversions_round_trip() {
    let fps = 25.0;
    for frame in [0, 1, 24, 25, 1234] {
        let s = timeline_frame_to_sample(frame, fps, PROJECT_SAMPLE_RATE);
        assert_eq!(sample_to_timeline_frame(s, fps, PROJECT_SAMPLE_RATE), frame);
    }
}

fn downmix_interleaved(samples: &[f32], from: u16, to: u16) -> Vec<f32> {
    let mut out = Vec::new();
    remix_channels_into(samples, from, to, &mut out);
    out
}

#[test]
fn downmix_interleaved_is_a_noop_when_channel_counts_match() {
    let samples = vec![0.1, 0.2, 0.3, 0.4];
    assert_eq!(downmix_interleaved(&samples, 2, 2), samples);
}

#[test]
fn downmix_interleaved_averages_all_channels_to_mono() {
    // A stereo frame [1.0, 0.0] -> mono must give the average, 0.5.
    let samples = vec![1.0, 0.0, 0.5, 0.5];
    let mono = downmix_interleaved(&samples, 2, 1);
    assert_eq!(mono, vec![0.5, 0.5]);
}

#[test]
fn downmix_interleaved_six_to_two_groups_even_and_odd_channels() {
    // Typical ffmpeg order for 5.1(side): L,R,C,LFE,Ls,Rs. With
    // an even index -> channel 0 (L,C,Ls) and odd -> channel 1
    // (R,LFE,Rs): a frame with L=1.0 and all the others at 0 must end up
    // almost entirely on channel 0 (average of 1.0,0.0,0.0 = 1/3), nothing
    // on channel 1.
    let l_only = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let stereo = downmix_interleaved(&l_only, 6, 2);
    assert_eq!(stereo.len(), 2);
    assert!((stereo[0] - (1.0 / 3.0)).abs() < 1e-6, "left={}", stereo[0]);
    assert_eq!(stereo[1], 0.0);
}

#[test]
fn downmix_interleaved_upmixes_mono_by_duplicating_to_every_channel() {
    let mono = vec![0.7, -0.3];
    let stereo = downmix_interleaved(&mono, 1, 2);
    assert_eq!(stereo, vec![0.7, 0.7, -0.3, -0.3]);
}

#[test]
fn downmix_interleaved_preserves_frame_count() {
    let samples = vec![0.0f32; 6 * 100]; // 100 frames at 6 channels
    let stereo = downmix_interleaved(&samples, 6, 2);
    assert_eq!(stereo.len(), 2 * 100);
}

#[test]
fn db_to_linear_matches_known_reference_points() {
    assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
    // -6dB ~= halves the amplitude; +6dB ~= doubles it.
    assert!((db_to_linear(-6.0) - 0.5012).abs() < 1e-3);
    assert!((db_to_linear(6.0) - 1.9953).abs() < 1e-3);
    // -20dB = exactly a factor of 0.1.
    assert!((db_to_linear(-20.0) - 0.1).abs() < 1e-6);
}

fn fade_test_clip(fade_in: u64, fade_out: u64) -> MixClip {
    MixClip {
        start: 0,
        len: 1000,
        source_offset: 0,
        step: 1.0,
        buffer: Arc::new(Vec::new()),
        gain_db: Keyframed::constant(0.0),
        track: 0,
        clip_fps: 10.0,
        media_offset: 0,
        media_step: 1.0,
        fade_in,
        fade_out,
    }
}

#[test]
fn fade_multiplier_ramps_linearly_in_and_out() {
    let clip = fade_test_clip(100, 200);
    assert_eq!(fade_multiplier(&clip, 0), 0.0);
    assert!((fade_multiplier(&clip, 50) - 0.5).abs() < 1e-6);
    assert_eq!(fade_multiplier(&clip, 100), 1.0);
    assert_eq!(
        fade_multiplier(&clip, 500),
        1.0,
        "on the plateau it stays at full volume"
    );
    assert!(
        (fade_multiplier(&clip, 900) - 0.5).abs() < 1e-6,
        "200 samples from the end"
    );
    assert_eq!(fade_multiplier(&clip, 1000), 0.0);
}

#[test]
fn no_fade_stays_at_full_volume() {
    let clip = fade_test_clip(0, 0);
    assert_eq!(fade_multiplier(&clip, 0), 1.0);
    assert_eq!(fade_multiplier(&clip, 500), 1.0);
    assert_eq!(fade_multiplier(&clip, 1000), 1.0);
}

#[test]
fn overlapping_fades_multiply_instead_of_dipping_below_either_ramp_alone() {
    // Fade in and out cover the whole clip: at the center each ramp is
    // 0.5, the product (not the minimum) is what one sees.
    let clip = fade_test_clip(1000, 1000);
    assert!((fade_multiplier(&clip, 500) - 0.25).abs() < 1e-6);
}

#[test]
fn fade_in_silences_the_start_of_a_block_and_full_gain_clip_is_unaffected() {
    let (project, a, _) = project();
    // 3 blocks of GAIN_BLOCK_FRAMES: the fade covers exactly the first one.
    let blocks = 3;
    let len_frames = (GAIN_BLOCK_FRAMES * blocks) as usize;
    let mut clip = clip_at(a, 0, 0, (len_frames / 10) as FrameIdx);
    clip.fade_in = (GAIN_BLOCK_FRAMES / 10) as FrameIdx;
    let tl = timeline(vec![audio_track(vec![clip])]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut |_: &Path, _: usize| {
        Some(Arc::new(vec![0.5; len_frames]))
    });
    let mut out = vec![9.0; len_frames];
    mix_range(&snap, 0, &mut out);
    let block = GAIN_BLOCK_FRAMES as usize;
    assert!(
        out[..block].iter().all(|&s| s == 0.0),
        "first block silenced by the fade-in"
    );
    assert!(
        out[block..].iter().all(|&s| s == 0.5),
        "past the fade-in the gain is unchanged"
    );
}

/// `b` from source frame 10 at `speed`, `len` timeline frames from 0,
/// varispeed.
fn sped_up_clip(media: vv_core::MediaId, speed: Rational, len: FrameIdx) -> Clip {
    let mut clip = clip_at(media, 0, 0, 1);
    clip.speed = speed;
    clip.pitch_correction = false;
    clip.conform(Rational::one());
    clip.source_offset = clip.rate.scale_round(10);
    clip.timeline_len = len;
    clip
}

#[test]
fn a_faster_clip_reads_its_audio_faster_averaging_what_it_skips() {
    let (project, _, b) = project();
    let tl = timeline(vec![audio_track(vec![sped_up_clip(
        b,
        Rational::new(2, 1),
        20,
    )])]);
    let out = render(&project, &tl, 0, 200);
    for i in [0usize, 7, 150] {
        let expected = (100.0 + 2.0 * i as f32 + 0.5) / 1000.0;
        assert!((out[i] - expected).abs() < 1e-5, "sample {i}: {}", out[i]);
    }
}

#[test]
fn a_slower_clip_interpolates_between_samples() {
    let (project, _, b) = project();
    let tl = timeline(vec![audio_track(vec![sped_up_clip(
        b,
        Rational::new(1, 2),
        40,
    )])]);
    let out = render(&project, &tl, 0, 400);
    for i in [0usize, 1, 301] {
        let expected = (100.0 + i as f32 / 2.0) / 1000.0;
        assert!((out[i] - expected).abs() < 1e-5, "sample {i}: {}", out[i]);
    }
}

/// Hands out a fixed "stretched" buffer and records what it was asked.
struct FakeStretch {
    ready: bool,
    asked: Vec<(std::ops::Range<u64>, f64)>,
}

impl AudioSource for FakeStretch {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        buffers(path, stream).map_or(ClipAudio::Missing, ClipAudio::Ready)
    }

    fn stretched(
        &mut self,
        _buffer: &Arc<Vec<f32>>,
        range: std::ops::Range<u64>,
        tempo: f64,
        _sample_rate: u32,
        _channels: u16,
    ) -> ClipAudio {
        self.asked.push((range, tempo));
        if self.ready {
            ClipAudio::Ready(Arc::new(vec![0.9; 100]))
        } else {
            ClipAudio::Pending
        }
    }
}

#[test]
fn a_pitch_corrected_clip_plays_the_stretch_of_its_source_range_or_nothing() {
    let (project, _, b) = project();
    let mut clip = sped_up_clip(b, Rational::new(2, 1), 20);
    clip.pitch_correction = true;
    let tl = timeline(vec![audio_track(vec![clip])]);

    let mut source = FakeStretch {
        ready: true,
        asked: Vec::new(),
    };
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut source);
    let mut out = vec![0.0; 200];
    mix_range(&snap, 0, &mut out);
    assert_eq!(source.asked, [(100..500, 2.0)]);
    assert_eq!(out[0], 0.9);
    assert_eq!(out[99], 0.9);
    assert_eq!(out[100], 0.0, "past the stretched audio");

    source.ready = false;
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut source);
    mix_range(&snap, 0, &mut out);
    assert!(out.iter().all(|&s| s == 0.0), "silent until it is ready");
}

#[test]
fn track_and_master_gains_scale_the_mix() {
    let (project, a, b) = project();
    let mut louder = audio_track(vec![clip_at(a, 0, 0, 5)]);
    louder.mix.gain_db = 20.0 * 2f32.log10();
    let mut tl = timeline(vec![louder, audio_track(vec![clip_at(b, 0, 0, 5)])]);
    tl.master.gain_db = -20.0 * 2f32.log10();
    let out = render(&project, &tl, 0, 5);
    for (i, s) in out.iter().enumerate() {
        let expected = (2.0 * 0.5 + i as f32 / 1000.0) / 2.0;
        assert!((s - expected).abs() < 1e-5, "{s} != {expected}");
    }
}

#[test]
fn a_compound_clip_plays_through_its_nested_master() {
    let (mut project, compound) = project_with_compound();
    let nested = project.media_pool[compound].compound.unwrap();
    project.timelines[nested].master.gain_db = -20.0 * 2f32.log10();
    let tl = timeline(vec![audio_track(vec![clip_at(compound, 0, 0, 5)])]);
    let out = render(&project, &tl, 0, 50);
    assert!(out.iter().all(|&s| (s - 0.25).abs() < 1e-5));
}

#[test]
fn balance_attenuates_only_the_opposite_channel() {
    assert_eq!(balance_gains(0.0), [1.0, 1.0]);
    assert_eq!(balance_gains(-0.25), [1.0, 0.75]);
    assert_eq!(balance_gains(2.0), [0.0, 1.0], "clamped");

    let (project, a, _) = project();
    let mut left = audio_track(vec![clip_at(a, 0, 0, 5)]);
    left.mix.pan = -0.5;
    let mut tl = timeline(vec![left]);
    tl.master.pan = 0.5;
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 2, &mut buffers);
    let mut out = vec![9.0; 4];
    mix_range(&snap, 0, &mut out);
    assert_eq!(out, [0.25, 0.25, 0.25, 0.25]);
}

#[test]
fn only_the_metered_mix_raises_the_meters_and_taking_resets_them() {
    let (project, a, b) = project();
    let mut quiet = audio_track(vec![clip_at(a, 0, 0, 5)]);
    quiet.mix.gain_db = -20.0 * 2f32.log10();
    let tl = timeline(vec![quiet, audio_track(vec![clip_at(b, 0, 0, 5)])]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut buffers);
    let mut out = vec![0.0; 20];

    mix_range(&snap, 0, &mut out);
    assert_eq!(snap.meters.master().take(), (0.0, 0.0));

    mix_range_metered(&snap, &mut MixState::new(&snap), 0, &mut out);
    let (l, r) = snap.meters.track(0).unwrap().take();
    assert!((l - 0.25).abs() < 1e-5 && l == r, "mono on both sides");
    assert_eq!(snap.meters.track(1).unwrap().take().0, 19.0 / 1000.0);
    assert!((snap.meters.master().take().0 - (0.25 + 0.019)).abs() < 1e-5);
    assert_eq!(snap.meters.track(0).unwrap().take(), (0.0, 0.0));
}

fn normalize(target_db: f32) -> vv_core::AudioEffect {
    vv_core::AudioEffect::new(vv_core::AudioEffectKind::Normalize { target_db })
}

#[test]
fn track_normalization_brings_its_peak_to_the_target_before_the_fader() {
    let (project, a, _) = project();
    let mut track = audio_track(vec![clip_at(a, 0, 0, 5)]);
    track.mix.effects = vec![normalize(-12.0), normalize(0.0)];
    track.mix.gain_db = -20.0 * 2f32.log10();
    let tl = timeline(vec![track]);
    let out = render(&project, &tl, 0, 50);
    assert!(
        out.iter().all(|&s| (s - 0.5).abs() < 1e-5),
        "last one wins, then the fader"
    );
}

#[test]
fn a_disabled_normalization_does_nothing_and_silence_stays_silent() {
    let (project, a, _) = project();
    let mut track = audio_track(vec![clip_at(a, 0, 0, 5)]);
    track.mix.effects = vec![vv_core::AudioEffect {
        enabled: false,
        ..normalize(0.0)
    }];
    let mut empty = audio_track(Vec::new());
    empty.mix.effects = vec![normalize(0.0)];
    let tl = timeline(vec![track, empty]);
    assert!(render(&project, &tl, 0, 50).iter().all(|&s| s == 0.5));
}

#[test]
fn master_normalization_measures_the_mix_after_the_track_faders() {
    let (project, a, _) = project();
    let mut quiet = audio_track(vec![clip_at(a, 0, 0, 5)]);
    quiet.mix.gain_db = -20.0 * 4f32.log10();
    let mut tl = timeline(vec![quiet, audio_track(vec![clip_at(a, 0, 0, 5)])]);
    tl.master.effects = vec![normalize(-20.0 * 2f32.log10())];
    // 0.125 + 0.5 = 0.625, brought to 0.5.
    let out = render(&project, &tl, 0, 50);
    assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-5));
}

/// Measures nothing: every peak is on its way, with `last` as the reading
/// kept from before.
struct PendingPeaks {
    last: Option<f32>,
    stored: bool,
}

impl AudioSource for PendingPeaks {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        buffers(path, stream).map_or(ClipAudio::Missing, ClipAudio::Ready)
    }

    fn store_compound(&mut self, _: MediaId, _: u64, _: Arc<Vec<f32>>) {
        self.stored = true;
    }

    fn peak(&mut self, _: PeakAnalysis) -> PeakReading {
        PeakReading::Pending(self.last)
    }
}

#[test]
fn a_pending_peak_plays_the_last_reading_and_keeps_a_compound_out_of_the_cache() {
    let (mut project, compound) = project_with_compound();
    let nested = project.media_pool[compound].compound.unwrap();
    project.timelines[nested].tracks[0].mix.effects = vec![normalize(0.0)];
    let tl = timeline(vec![audio_track(vec![clip_at(compound, 0, 0, 5)])]);

    let mut source = PendingPeaks {
        last: Some(0.25),
        stored: false,
    };
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut source);
    let mut out = vec![0.0; 50];
    mix_range(&snap, 0, &mut out);
    assert!(out.iter().all(|&s| (s - 2.0).abs() < 1e-5), "0.5 * 1/0.25");
    assert!(!source.stored);

    source.last = None;
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut source);
    mix_range(&snap, 0, &mut out);
    assert!(out.iter().all(|&s| s == 0.5), "unity until measured");
}

fn compressor() -> vv_core::AudioEffect {
    let mut params = vv_core::MultibandCompressor::DEFAULT;
    for band in &mut params.bands {
        band.threshold_db = -30.0;
        band.ratio = 8.0;
    }
    vv_core::AudioEffect::new(vv_core::AudioEffectKind::MultibandCompressor(params))
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0, |p, s| p.max(s.abs()))
}

#[test]
fn a_normalization_after_a_compressor_measures_the_compressed_signal() {
    let (project, _, b) = project();
    let mut track = audio_track(vec![clip_at(b, 0, 0, 90)]);
    track.mix.effects = vec![compressor(), normalize(-20.0 * 2f32.log10())];
    let tl = timeline(vec![track.clone()]);
    let out = render(&project, &tl, 0, 900);
    assert!((peak(&out) - 0.5).abs() < 1e-3, "{}", peak(&out));

    track.mix.effects.reverse();
    let tl = timeline(vec![track]);
    assert!(
        peak(&render(&project, &tl, 0, 900)) < 0.4,
        "compressed after"
    );
}

#[test]
fn the_output_stream_carries_the_effects_over_from_one_call_to_the_next() {
    let (project, _, b) = project();
    let mut track = audio_track(vec![clip_at(b, 0, 0, 90)]);
    track.mix.effects = vec![compressor()];
    let tl = timeline(vec![track]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut buffers);
    let whole = render(&project, &tl, 0, 600);

    let mut state = MixState::new(&snap);
    let mut halves = vec![0.0; 600];
    let (first, second) = halves.split_at_mut(300);
    mix_range_metered(&snap, &mut state, 0, first);
    mix_range_metered(&snap, &mut state, 300, second);
    assert_eq!(halves, whole);

    let mut jumped = vec![0.0; 300];
    mix_range_metered(&snap, &mut state, 300, &mut jumped);
    assert_eq!(
        jumped,
        render(&project, &tl, 300, 300),
        "a seek starts over"
    );
}

#[test]
fn the_compressor_reports_its_reduction_at_its_place_in_the_strip() {
    let (project, _, b) = project();
    let mut track = audio_track(vec![clip_at(b, 0, 0, 90)]);
    let disabled = vv_core::AudioEffect {
        enabled: false,
        ..normalize(0.0)
    };
    track.mix.effects = vec![disabled, compressor()];
    let tl = timeline(vec![track]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut buffers);
    let meter = || snap.meters.band_meter(MixerChannel::Track(0), 1).unwrap();
    let mut out = vec![0.0; 900];
    let silent = crate::dynamics::BandActivity::default();

    mix_range(&snap, 0, &mut out);
    assert_eq!(meter().take(), silent);

    mix_range_metered(&snap, &mut MixState::new(&snap), 0, &mut out);
    let activity = meter().take();
    assert!(
        activity.reduction_db.iter().any(|r| *r > 1.0),
        "{activity:?}"
    );
    assert!(activity.level.iter().any(|l| *l > 0.01), "{activity:?}");
    assert_eq!(meter().take(), silent);
    let disabled = snap.meters.band_meter(MixerChannel::Track(0), 0).unwrap();
    assert_eq!(disabled.take(), silent);
}

#[test]
fn the_output_stream_feeds_the_compressor_spectrum_and_a_new_snapshot_keeps_it() {
    let (project, _, b) = project();
    let mut track = audio_track(vec![clip_at(b, 0, 0, 90)]);
    track.mix.effects = vec![compressor()];
    let tl = timeline(vec![track]);
    let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut buffers);
    let tap = || snap.meters.spectrum(MixerChannel::Track(0), 0).unwrap();
    let mut out = vec![0.0; 900];

    mix_range(&snap, 0, &mut out);
    assert_eq!(tap().written(), 0);
    mix_range_metered(&snap, &mut MixState::new(&snap), 0, &mut out);
    assert_eq!(tap().written(), 900);

    let mut next = MixSnapshot::from_timeline(&project, &tl, RATE, 1, &mut buffers);
    next.meters.keep_spectra_of(&snap.meters);
    let kept = next.meters.spectrum(MixerChannel::Track(0), 0).unwrap();
    assert_eq!(kept.written(), 900);
    assert!(snap.meters.spectrum(MixerChannel::Track(0), 1).is_none());
}
