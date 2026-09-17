//! Audio e clock del playback della timeline: il mixer suona tutte le track
//! audio e la sua posizione è il playhead. Senza device audio, o a velocità
//! diversa da 1x, il clock è a parete (e l'audio tace).

use std::sync::Arc;
use std::time::{Duration, Instant};

use vv_audio::mixer::{
    MixSnapshot, Mixer, PROJECT_SAMPLE_RATE, sample_to_timeline_frame, timeline_frame_to_sample,
};
use vv_core::{FrameIdx, Project, TimelineId};

use crate::mix_buffers::MixBufferCache;

const SCRUB_SNIPPET: Duration = Duration::from_millis(80);

pub struct TimelineAudio {
    mixer: Option<Mixer>,
    buffers: MixBufferCache,
    playing: bool,
    speed: f64,
    wall_base_sample: u64,
    wall_started_at: Option<Instant>,
    /// Fine del frammento di scrub e posizione da ripristinare.
    scrub_snippet: Option<(Instant, u64)>,
    synced: Option<(TimelineId, u64)>,
    #[cfg(test)]
    snapshot: Option<Arc<MixSnapshot>>,
}

impl TimelineAudio {
    pub fn new() -> Self {
        let mixer = Mixer::new()
            .map_err(|e| eprintln!("vv-app: mixer audio non disponibile: {e}"))
            .ok();
        let channels = mixer.as_ref().map_or(2, Mixer::channels);
        Self {
            mixer,
            buffers: MixBufferCache::spawn(PROJECT_SAMPLE_RATE, channels),
            playing: false,
            speed: 1.0,
            wall_base_sample: 0,
            wall_started_at: None,
            scrub_snippet: None,
            synced: None,
            #[cfg(test)]
            snapshot: None,
        }
    }

    /// Da chiamare a ogni frame: ricostruisce lo snapshot del mix se il
    /// progetto è cambiato o sono arrivati nuovi buffer decodificati.
    pub fn sync(&mut self, project: &Project, timeline_id: TimelineId, generation: u64) {
        let buffers_arrived = self.buffers.poll();
        if !buffers_arrived && self.synced == Some((timeline_id, generation)) {
            return;
        }
        self.synced = Some((timeline_id, generation));
        let Some(mixer) = &mut self.mixer else {
            return;
        };
        let Some(timeline) = project.timelines.get(timeline_id) else {
            return;
        };
        let buffers = &mut self.buffers;
        let snapshot = MixSnapshot::from_timeline(
            project,
            timeline,
            mixer.sample_rate(),
            mixer.channels(),
            |path, stream| buffers.get_or_request(path, stream),
        );
        let snapshot = Arc::new(snapshot);
        #[cfg(test)]
        {
            self.snapshot = Some(snapshot.clone());
        }
        mixer.set_snapshot(snapshot);
    }

    /// Il mix che il callback suonerebbe da `frame`, per i test.
    #[cfg(test)]
    pub fn render(&self, frame: FrameIdx, fps: f64, len_frames: FrameIdx) -> Vec<f32> {
        let snapshot = self.snapshot.as_ref().expect("nessuno snapshot pubblicato");
        let start = timeline_frame_to_sample(frame, fps, snapshot.sample_rate);
        let end = timeline_frame_to_sample(frame + len_frames, fps, snapshot.sample_rate);
        let mut out = vec![0.0; (end - start) as usize * snapshot.channels as usize];
        vv_audio::mixer::mix_range(snapshot, start, &mut out);
        out
    }

    #[cfg(test)]
    pub fn has_pending_buffers(&mut self) -> bool {
        self.buffers.has_pending()
    }

    /// Forza la ricostruzione dello snapshot al prossimo `sync` (es. progetto
    /// sostituito senza passare dalla history).
    pub fn invalidate(&mut self) {
        self.synced = None;
    }

    fn uses_mixer_clock(&self) -> bool {
        self.mixer.is_some() && (self.speed - 1.0).abs() < 1e-9
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
        if self.uses_mixer_clock() {
            if let Some(mixer) = &self.mixer {
                mixer.play();
            }
        } else {
            self.wall_started_at = Some(Instant::now());
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
    }

    fn position_sample(&self) -> u64 {
        if let Some((_, restore)) = self.scrub_snippet {
            return restore;
        }
        if self.uses_mixer_clock() {
            return self.mixer.as_ref().map_or(0, Mixer::position);
        }
        let elapsed = self.wall_started_at.map_or(0.0, |t| t.elapsed().as_secs_f64());
        self.wall_base_sample + (elapsed * self.speed * PROJECT_SAMPLE_RATE as f64) as u64
    }

    pub fn seek_frame(&mut self, frame: FrameIdx, fps: f64) {
        self.seek_sample(timeline_frame_to_sample(frame.max(0), fps, PROJECT_SAMPLE_RATE));
    }

    pub fn position_frame(&self, fps: f64) -> FrameIdx {
        sample_to_timeline_frame(self.position_sample(), fps, PROJECT_SAMPLE_RATE)
    }

    /// Finché il passo 4 di AUDIO_MIXER_PLAN.md non stretcha il mix, a
    /// velocità diverse da 1x il clock è a parete e l'audio tace.
    pub fn set_speed(&mut self, speed: f64) {
        if (speed - self.speed).abs() < 1e-9 {
            return;
        }
        let pos = self.position_sample();
        self.speed = speed;
        self.seek_sample(pos);
        if let Some(mixer) = &self.mixer {
            if self.playing && self.uses_mixer_clock() {
                mixer.play();
            } else {
                mixer.pause();
            }
        }
    }

    /// Breve frammento dalla posizione corrente senza entrare in
    /// riproduzione. No-op se si sta già riproducendo o senza device.
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
    }

    pub fn peak_linear_stereo(&self) -> (f32, f32) {
        self.mixer
            .as_ref()
            .map_or((0.0, 0.0), Mixer::peak_linear_stereo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: f64 = 25.0;

    #[test]
    fn seek_and_position_round_trip_in_timeline_frames() {
        let mut audio = TimelineAudio::new();
        for frame in [0, 1, 24, 250, 1234] {
            audio.seek_frame(frame, FPS);
            assert_eq!(audio.position_frame(FPS), frame);
        }
    }

    #[test]
    fn clock_advances_while_playing_and_stops_on_pause() {
        let mut audio = TimelineAudio::new();
        audio.seek_frame(100, FPS);
        audio.play();
        std::thread::sleep(Duration::from_millis(300));
        let playing_pos = audio.position_frame(FPS);
        assert!((103..=112).contains(&playing_pos), "pos={playing_pos}");

        audio.pause();
        let paused_pos = audio.position_frame(FPS);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(audio.position_frame(FPS), paused_pos);
        assert!(paused_pos >= playing_pos);
    }

    #[test]
    fn speed_multiplies_the_clock_without_jumping() {
        let mut audio = TimelineAudio::new();
        audio.seek_frame(0, FPS);
        audio.play();
        std::thread::sleep(Duration::from_millis(100));
        let before = audio.position_frame(FPS);
        audio.set_speed(4.0);
        assert!(audio.position_frame(FPS) >= before, "nessun salto indietro");
        std::thread::sleep(Duration::from_millis(250));
        let advanced = audio.position_frame(FPS) - before;
        // 250ms a 4x = 1s = 25 frame.
        assert!((20..=32).contains(&advanced), "advanced={advanced}");

        let at_reset = audio.position_frame(FPS);
        audio.set_speed(1.0);
        assert!(audio.position_frame(FPS) >= at_reset);
    }

    #[test]
    fn scrub_snippet_does_not_move_the_position() {
        let mut audio = TimelineAudio::new();
        audio.seek_frame(50, FPS);
        audio.play_scrub_snippet();
        assert!(!audio.is_playing());
        std::thread::sleep(SCRUB_SNIPPET + Duration::from_millis(50));
        audio.tick();
        assert!(!audio.is_scrub_snippet_active());
        assert_eq!(audio.position_frame(FPS), 50);
    }
}
