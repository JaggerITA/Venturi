//! Audio and playback clock of the timeline: the mixer plays all the audio
//! tracks and its position is the playhead. In fast forward it plays windows
//! of the mix already stretched in the background. Without an audio device
//! the clock is wall time and nothing plays.

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use vv_audio::mixer::{
    Bus, MixClip, MixMeters, MixSnapshot, Mixer, MixerState, PROJECT_SAMPLE_RATE, StretchedWindow,
    mix_range, sample_to_timeline_frame, timeline_frame_to_sample,
};
use vv_core::{FrameIdx, Keyframed, Project, TimelineId};

use crate::mix_buffers::MixBufferCache;

const SCRUB_SNIPPET: Duration = Duration::from_millis(80);

/// Duration (timeline audio frames) of each stretched window: stretching
/// everything in one go would delay the first sound by seconds, 8s come back
/// in ~150-200ms. A multiple of every tempo (2/4/8) so the stretched window
/// has an exact length and the queued windows do not drift.
const WINDOW_FRAMES: u64 = 8 * PROJECT_SAMPLE_RATE as u64;

/// Unplayed audio below which the next window is queued: at 8x it is 0.5s
/// real time against ~200ms of stretching.
const EXTEND_MARGIN_FRAMES: u64 = 4 * PROJECT_SAMPLE_RATE as u64;

#[derive(Clone, Copy)]
struct StretchRequest {
    id: u64,
    tempo: u64,
    start: u64,
    /// `false`: extends the current window instead of replacing it.
    new_window: bool,
}

struct StretchResult {
    id: u64,
    samples: Result<Vec<f32>, String>,
}

pub struct TimelineAudio {
    mixer: Option<Mixer>,
    buffers: MixBufferCache,
    mix: Arc<MixSnapshot>,
    stretched: Option<StretchedWindow>,
    playing: bool,
    /// Effective speed: a fast forward request applies only when its first
    /// window is ready.
    speed: f64,
    wall_base_sample: u64,
    wall_started_at: Option<Instant>,
    /// End of the scrub fragment and position to restore.
    scrub_snippet: Option<(Instant, u64)>,
    synced: Option<(TimelineId, u64)>,
    /// Media of the media pool preview currently in the snapshot.
    synced_media: Option<PathBuf>,
    /// At most one stretch in flight; a result with another id is stale.
    stretch_request: Option<StretchRequest>,
    next_request_id: u64,
    stretch_tx: mpsc::Sender<StretchResult>,
    stretch_rx: mpsc::Receiver<StretchResult>,
}

impl TimelineAudio {
    pub fn new() -> Self {
        let mixer = Mixer::new()
            .map_err(|e| eprintln!("vv-app: audio mixer unavailable: {e}"))
            .ok();
        let channels = mixer.as_ref().map_or(2, Mixer::channels);
        let (stretch_tx, stretch_rx) = mpsc::channel();
        Self {
            mixer,
            buffers: MixBufferCache::spawn(PROJECT_SAMPLE_RATE, channels),
            mix: Arc::new(MixSnapshot::empty(PROJECT_SAMPLE_RATE, channels)),
            stretched: None,
            playing: false,
            speed: 1.0,
            wall_base_sample: 0,
            wall_started_at: None,
            scrub_snippet: None,
            synced: None,
            synced_media: None,
            stretch_request: None,
            next_request_id: 0,
            stretch_tx,
            stretch_rx,
        }
    }

    fn channels(&self) -> u16 {
        self.mix.channels
    }

    /// To be called every frame: rebuilds the mix snapshot if the project
    /// changed or new decoded buffers arrived.
    pub fn sync(&mut self, project: &Project, timeline_id: TimelineId, generation: u64) {
        let buffers_arrived = self.buffers.poll();
        if !buffers_arrived && self.synced == Some((timeline_id, generation)) {
            return;
        }
        self.synced = Some((timeline_id, generation));
        self.synced_media = None;
        if self.mixer.is_none() {
            return;
        }
        let Some(timeline) = project.timelines.get(timeline_id) else {
            return;
        };
        let (rate, channels) = (self.mix.sample_rate, self.mix.channels);
        self.mix = Arc::new(MixSnapshot::from_timeline(
            project,
            timeline,
            rate,
            channels,
            &mut self.buffers,
        ));
        self.buffers.sweep_unused();
        self.publish();
    }

    /// Like `sync`, but for the preview of a media: plays all its
    /// `audio_streams` from the start of the file, without a timeline.
    pub fn sync_media(&mut self, path: &Path, audio_streams: usize, fps: f64) {
        let buffers_arrived = self.buffers.poll();
        if !buffers_arrived && self.synced_media.as_deref() == Some(path) {
            return;
        }
        self.synced_media = Some(path.to_path_buf());
        self.synced = None;
        if self.mixer.is_none() {
            return;
        }
        let (rate, channels) = (self.mix.sample_rate, self.mix.channels);
        let clips: Vec<MixClip> = (0..audio_streams)
            .filter_map(|stream| self.buffers.get_or_request_first(path, stream))
            .map(|buffer| MixClip {
                start: 0,
                len: buffer.len() as u64 / channels.max(1) as u64,
                source_offset: 0,
                step: 1.0,
                buffer,
                gain_db: Keyframed::constant(0.0),
                track: 0,
                clip_fps: fps,
                media_offset: 0,
                media_step: 1.0,
                fade_in: 0,
                fade_out: 0,
            })
            .collect();
        self.mix = Arc::new(MixSnapshot::new(
            rate,
            channels,
            clips,
            Vec::new(),
            Bus::UNITY,
        ));
        self.publish();
    }

    fn publish(&mut self) {
        if let Some(mixer) = &mut self.mixer {
            mixer.set_state(Arc::new(MixerState {
                mix: self.mix.clone(),
                stretched: self.stretched.clone(),
            }));
        }
    }

    /// The mix the callback would play from `frame` at 1x, for the tests.
    #[cfg(test)]
    pub fn render(&self, frame: FrameIdx, fps: f64, len_frames: FrameIdx) -> Vec<f32> {
        let rate = self.mix.sample_rate;
        let start = timeline_frame_to_sample(frame, fps, rate);
        let end = timeline_frame_to_sample(frame + len_frames, fps, rate);
        let mut out = vec![0.0; (end - start) as usize * self.channels() as usize];
        mix_range(&self.mix, start, &mut out);
        out
    }

    /// Peak of the loaded stretched audio, for the tests.
    #[cfg(test)]
    pub fn stretched_peak(&self) -> Option<f32> {
        let window = self.stretched.as_ref()?;
        let peak = window.chunks.iter().flat_map(|c| c.iter());
        Some(peak.fold(0.0f32, |m, s| m.max(s.abs())))
    }

    #[cfg(test)]
    pub fn has_pending_buffers(&mut self) -> bool {
        self.buffers.has_pending()
    }

    /// Forces a snapshot rebuild on the next `sync` (e.g. project replaced
    /// without going through the history).
    pub fn invalidate(&mut self) {
        self.synced = None;
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn play(&mut self) {
        self.stop_scrub_snippet();
        if self.playing {
            return;
        }
        self.playing = true;
        match &self.mixer {
            Some(mixer) => mixer.play(),
            None => self.wall_started_at = Some(Instant::now()),
        }
    }

    pub fn pause(&mut self) {
        self.stop_scrub_snippet();
        if !self.playing {
            return;
        }
        let pos = self.position_sample();
        self.playing = false;
        if let Some(mixer) = &self.mixer {
            mixer.pause();
        }
        self.seek_sample(pos);
    }

    fn seek_sample(&mut self, sample: u64) {
        if let Some((stop_at, _)) = self.scrub_snippet {
            self.scrub_snippet = Some((stop_at, sample));
        }
        if let Some(mixer) = &self.mixer {
            mixer.seek(sample);
        }
        self.wall_base_sample = sample;
        self.wall_started_at = self.playing.then(Instant::now);

        let channels = self.channels();
        if let Some(window) = &self.stretched
            && !(window.origin..window.covered_until(channels)).contains(&sample)
        {
            // Outside the window: restart from there. While waiting the clock
            // advances silently at the same speed, without reopening the stream.
            let tempo = match self.stretch_request {
                Some(req) if req.new_window => req.tempo,
                _ => window.tempo,
            };
            self.stretched = Some(StretchedWindow {
                tempo,
                origin: sample,
                chunks: Vec::new(),
            });
            self.speed = tempo as f64;
            self.publish();
            self.request_window(tempo, sample, true);
        }
    }

    fn position_sample(&self) -> u64 {
        if let Some((_, restore)) = self.scrub_snippet {
            return restore;
        }
        if let Some(mixer) = &self.mixer {
            return mixer.position();
        }
        let elapsed = self
            .wall_started_at
            .map_or(0.0, |t| t.elapsed().as_secs_f64());
        self.wall_base_sample + (elapsed * self.speed * PROJECT_SAMPLE_RATE as f64) as u64
    }

    pub fn seek_frame(&mut self, frame: FrameIdx, fps: f64) {
        self.seek_sample(timeline_frame_to_sample(
            frame.max(0),
            fps,
            PROJECT_SAMPLE_RATE,
        ));
    }

    pub fn position_frame(&self, fps: f64) -> FrameIdx {
        sample_to_timeline_frame(self.position_sample(), fps, PROJECT_SAMPLE_RATE)
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// 1x applies immediately; 2x/4x/8x when the first stretched window is
    /// ready (meanwhile playback continues at the current speed).
    pub fn request_speed(&mut self, speed: f64) {
        let tempo = speed.round().max(1.0) as u64;
        if self.mixer.is_none() {
            let pos = self.position_sample();
            self.speed = tempo as f64;
            self.seek_sample(pos);
            return;
        }
        if tempo == 1 {
            self.stretch_request = None;
            self.speed = 1.0;
            if self.stretched.take().is_some() {
                self.publish();
            }
            return;
        }
        let already_requested = match self.stretch_request {
            Some(req) if req.new_window => req.tempo == tempo,
            _ => self.stretched.as_ref().is_some_and(|w| w.tempo == tempo),
        };
        if !already_requested {
            let pos = self.position_sample();
            self.request_window(tempo, pos, true);
        }
    }

    fn request_window(&mut self, tempo: u64, start: u64, new_window: bool) {
        let request = StretchRequest {
            id: self.next_request_id,
            tempo,
            start,
            new_window,
        };
        self.next_request_id += 1;
        self.stretch_request = Some(request);

        let mix = self.mix.clone();
        let tx = self.stretch_tx.clone();
        std::thread::spawn(move || {
            let ch = mix.channels as usize;
            let mut samples = vec![0.0; WINDOW_FRAMES as usize * ch];
            mix_range(&mix, start, &mut samples);
            let expected = (WINDOW_FRAMES / tempo) as usize * ch;
            let samples =
                vv_audio::stretch_samples(&samples, mix.sample_rate, mix.channels, tempo as f64)
                    .map(|mut stretched| {
                        stretched.resize(expected, 0.0);
                        stretched
                    });
            let _ = tx.send(StretchResult {
                id: request.id,
                samples,
            });
        });
    }

    fn apply_stretch_results(&mut self) {
        while let Ok(result) = self.stretch_rx.try_recv() {
            let Some(request) = self.stretch_request.filter(|r| r.id == result.id) else {
                continue;
            };
            self.stretch_request = None;
            let samples = match result.samples {
                Ok(samples) => Arc::new(samples),
                Err(e) => {
                    eprintln!("vv-app: audio stretch at {}x failed: {e}", request.tempo);
                    continue;
                }
            };
            let channels = self.channels();
            if request.new_window {
                let pos = self.position_sample();
                // While stretching, the playhead moved past it, or was moved.
                if !(request.start..request.start + WINDOW_FRAMES).contains(&pos) {
                    self.request_window(request.tempo, pos, true);
                    continue;
                }
                self.stretched = Some(StretchedWindow {
                    tempo: request.tempo,
                    origin: request.start,
                    chunks: vec![samples],
                });
                self.speed = request.tempo as f64;
            } else {
                let Some(window) = &mut self.stretched else {
                    continue;
                };
                if window.tempo != request.tempo || window.covered_until(channels) != request.start
                {
                    continue;
                }
                window.chunks.push(samples);
                self.drop_played_chunks();
            }
            self.publish();
        }
    }

    /// Discards the windows already played: in a long fast forward they
    /// would pile up.
    fn drop_played_chunks(&mut self) {
        let channels = self.channels().max(1) as usize;
        let pos = self.position_sample();
        let Some(window) = &mut self.stretched else {
            return;
        };
        while window.chunks.len() > 1 {
            let end = window.origin + (window.chunks[0].len() / channels) as u64 * window.tempo;
            if end > pos {
                break;
            }
            window.chunks.remove(0);
            window.origin = end;
        }
    }

    fn extend_window_if_needed(&mut self) {
        if !self.playing || self.stretch_request.is_some() {
            return;
        }
        let Some(window) = &self.stretched else {
            return;
        };
        let covered_until = window.covered_until(self.channels());
        if covered_until.saturating_sub(self.position_sample()) < EXTEND_MARGIN_FRAMES {
            self.request_window(window.tempo, covered_until, false);
        }
    }

    /// Short fragment from the current position without entering playback.
    /// No-op if already playing or without a device.
    pub fn play_scrub_snippet(&mut self) {
        if self.playing {
            return;
        }
        let Some(mixer) = &self.mixer else {
            return;
        };
        let pos = self.position_sample();
        self.scrub_snippet = None;
        mixer.seek(pos);
        mixer.play();
        self.scrub_snippet = Some((Instant::now() + SCRUB_SNIPPET, pos));
    }

    pub fn is_scrub_snippet_active(&self) -> bool {
        self.scrub_snippet.is_some()
    }

    fn stop_scrub_snippet(&mut self) {
        if let Some((_, pos)) = self.scrub_snippet.take()
            && let Some(mixer) = &self.mixer
        {
            mixer.pause();
            mixer.seek(pos);
        }
    }

    pub fn tick(&mut self) {
        if self
            .scrub_snippet
            .is_some_and(|(stop_at, _)| Instant::now() >= stop_at)
        {
            self.stop_scrub_snippet();
        }
        self.apply_stretch_results();
        self.extend_window_if_needed();
    }

    pub fn peak_linear_stereo(&self) -> (f32, f32) {
        self.mixer
            .as_ref()
            .map_or((0.0, 0.0), Mixer::peak_linear_stereo)
    }

    /// Per-track and master peaks of what is playing.
    pub fn meters(&self) -> &MixMeters {
        &self.mix.meters
    }
}

#[cfg(test)]
#[path = "tests/timeline_audio.rs"]
mod tests;
