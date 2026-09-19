//! Audio e clock del playback della timeline: il mixer suona tutte le track
//! audio e la sua posizione è il playhead. In fast forward suona finestre
//! del mix già stretchate in background. Senza device audio il clock è a
//! parete e non suona nulla.

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use vv_audio::mixer::{
    MixClip, MixSnapshot, Mixer, MixerState, PROJECT_SAMPLE_RATE, StretchedWindow, mix_range,
    sample_to_timeline_frame, timeline_frame_to_sample,
};
use vv_core::{FrameIdx, Keyframed, Project, TimelineId};

use crate::mix_buffers::MixBufferCache;

const SCRUB_SNIPPET: Duration = Duration::from_millis(80);

/// Durata (frame audio di timeline) di ogni finestra stretchata: stretchare
/// tutto in un colpo ritarderebbe di secondi il primo suono, 8s tornano in
/// ~150-200ms. Multiplo di ogni tempo (2/4/8) così la finestra stretchata
/// ha una lunghezza esatta e le finestre accodate non derivano.
const WINDOW_FRAMES: u64 = 8 * PROJECT_SAMPLE_RATE as u64;

/// Audio non ancora suonato sotto cui si accoda la finestra successiva:
/// a 8x sono 0.5s reali contro ~200ms di stretch.
const EXTEND_MARGIN_FRAMES: u64 = 4 * PROJECT_SAMPLE_RATE as u64;

#[derive(Clone, Copy)]
struct StretchRequest {
    id: u64,
    tempo: u64,
    start: u64,
    /// `false`: estende la finestra corrente invece di sostituirla.
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
    /// Velocità effettiva: una richiesta di fast forward si applica solo
    /// quando la sua prima finestra è pronta.
    speed: f64,
    wall_base_sample: u64,
    wall_started_at: Option<Instant>,
    /// Fine del frammento di scrub e posizione da ripristinare.
    scrub_snippet: Option<(Instant, u64)>,
    synced: Option<(TimelineId, u64)>,
    /// Media dell'anteprima del media pool attualmente nello snapshot.
    synced_media: Option<PathBuf>,
    /// Al più uno stretch in volo; un risultato con altro id è superato.
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

    /// Da chiamare a ogni frame: ricostruisce lo snapshot del mix se il
    /// progetto è cambiato o sono arrivati nuovi buffer decodificati.
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
        let buffers = &mut self.buffers;
        self.mix = Arc::new(MixSnapshot::from_timeline(
            project,
            timeline,
            rate,
            channels,
            |path, stream| buffers.get_or_request(path, stream),
        ));
        self.publish();
    }

    /// Come `sync`, ma per l'anteprima di un media: suona tutti i suoi
    /// `audio_streams` da inizio file, senza timeline.
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
                buffer,
                gain_db: Keyframed::constant(0.0),
                clip_fps: fps,
            })
            .collect();
        self.mix = Arc::new(MixSnapshot {
            sample_rate: rate,
            channels,
            clips,
        });
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

    /// Il mix che il callback suonerebbe da `frame` a 1x, per i test.
    #[cfg(test)]
    pub fn render(&self, frame: FrameIdx, fps: f64, len_frames: FrameIdx) -> Vec<f32> {
        let rate = self.mix.sample_rate;
        let start = timeline_frame_to_sample(frame, fps, rate);
        let end = timeline_frame_to_sample(frame + len_frames, fps, rate);
        let mut out = vec![0.0; (end - start) as usize * self.channels() as usize];
        mix_range(&self.mix, start, &mut out);
        out
    }

    /// Picco dell'audio stretchato caricato, per i test.
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

    /// Forza la ricostruzione dello snapshot al prossimo `sync` (es. progetto
    /// sostituito senza passare dalla history).
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
            // Fuori finestra: si riparte da lì. Nell'attesa il clock avanza
            // in silenzio alla stessa velocità, senza riaprire lo stream.
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
        let elapsed = self.wall_started_at.map_or(0.0, |t| t.elapsed().as_secs_f64());
        self.wall_base_sample + (elapsed * self.speed * PROJECT_SAMPLE_RATE as f64) as u64
    }

    pub fn seek_frame(&mut self, frame: FrameIdx, fps: f64) {
        self.seek_sample(timeline_frame_to_sample(frame.max(0), fps, PROJECT_SAMPLE_RATE));
    }

    pub fn position_frame(&self, fps: f64) -> FrameIdx {
        sample_to_timeline_frame(self.position_sample(), fps, PROJECT_SAMPLE_RATE)
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// 1x si applica subito; 2x/4x/8x quando la prima finestra stretchata è
    /// pronta (intanto si prosegue alla velocità corrente).
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
                    eprintln!("vv-app: stretch audio a {}x fallito: {e}", request.tempo);
                    continue;
                }
            };
            let channels = self.channels();
            if request.new_window {
                let pos = self.position_sample();
                // Mentre si stretchava il playhead è andato oltre, o è stato spostato.
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

    /// Scarta le finestre già suonate: in un fast forward lungo si
    /// accumulerebbero.
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
        self.apply_stretch_results();
        self.extend_window_if_needed();
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
    const RATE: f64 = PROJECT_SAMPLE_RATE as f64;

    fn secs(audio: &TimelineAudio) -> f64 {
        audio.position_sample() as f64 / RATE
    }

    fn wait_for_speed(audio: &mut TimelineAudio, speed: f64) -> Duration {
        let started = Instant::now();
        while audio.speed() != speed {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "lo stretch doveva completarsi"
            );
            std::thread::sleep(Duration::from_millis(5));
            audio.tick();
        }
        started.elapsed()
    }

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

    /// Finché la prima finestra non è pronta si resta a 1x; quando arriva la
    /// testina continua da dove è arrivata, non da dove era alla richiesta.
    #[test]
    fn fast_forward_applies_when_the_first_window_is_ready_without_jumping_back() {
        let mut audio = TimelineAudio::new();
        audio.play();
        std::thread::sleep(Duration::from_millis(100));
        let at_request = secs(&audio);
        audio.request_speed(2.0);
        assert_eq!(audio.speed(), 1.0);

        let wait = wait_for_speed(&mut audio, 2.0);
        assert!(wait < Duration::from_secs(2), "wait={wait:?}");
        let at_apply = secs(&audio);
        assert!(
            at_apply >= at_request + wait.as_secs_f64() - 0.05,
            "at_request={at_request} wait={wait:?} at_apply={at_apply}"
        );

        std::thread::sleep(Duration::from_millis(300));
        let advanced = secs(&audio) - at_apply;
        assert!(advanced > 0.45 && advanced < 0.9, "advanced={advanced}");

        audio.request_speed(1.0);
        assert_eq!(audio.speed(), 1.0, "tornare a 1x è immediato");
        assert!(audio.stretched.is_none());
    }

    fn assert_window_extends_seamlessly(tempo: f64) {
        let mut audio = TimelineAudio::new();
        audio.play();
        audio.request_speed(tempo);
        wait_for_speed(&mut audio, tempo);

        let window_secs = WINDOW_FRAMES as f64 / RATE;
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut max = 0.0;
        while Instant::now() < deadline && max <= window_secs + 1.0 {
            audio.tick();
            let pos = secs(&audio);
            assert!(pos >= max - 1e-6, "mai indietro: pos={pos} max={max}");
            max = pos;
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(max > window_secs + 1.0, "max={max}");
        let window = audio.stretched.as_ref().unwrap();
        let covered = window.covered_until(audio.channels()) as f64 / RATE;
        assert!(
            covered > max,
            "l'audio deve precedere la testina: covered={covered} max={max}"
        );
    }

    #[test]
    fn fast_forward_window_extends_seamlessly_at_4x() {
        assert_window_extends_seamlessly(4.0);
    }

    #[test]
    fn fast_forward_window_extends_seamlessly_at_8x() {
        assert_window_extends_seamlessly(8.0);
    }

    /// Seek fuori dalla finestra in fast forward: riparte da lì, senza
    /// tornare a 1x né bloccarsi.
    #[test]
    fn seeking_outside_the_window_restarts_fast_forward_from_there() {
        let mut audio = TimelineAudio::new();
        audio.play();
        audio.request_speed(4.0);
        wait_for_speed(&mut audio, 4.0);

        let target = 60 * PROJECT_SAMPLE_RATE as u64;
        audio.seek_sample(target);
        assert_eq!(audio.speed(), 4.0);
        assert!(secs(&audio) >= 60.0);

        let started = Instant::now();
        while audio.stretched.as_ref().unwrap().chunks.is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "finestra mai arrivata"
            );
            std::thread::sleep(Duration::from_millis(5));
            audio.tick();
        }
        assert_eq!(audio.stretched.as_ref().unwrap().origin, target);
        let pos = secs(&audio);
        assert!(pos > 60.0 && pos < 63.0, "pos={pos}");
    }

    #[test]
    fn a_superseded_stretch_result_is_discarded() {
        let mut audio = TimelineAudio::new();
        audio.play();
        audio.request_speed(2.0);
        audio.request_speed(4.0);
        wait_for_speed(&mut audio, 4.0);
        std::thread::sleep(Duration::from_millis(500));
        audio.tick();
        assert_eq!(audio.speed(), 4.0);
        assert_eq!(audio.stretched.as_ref().unwrap().tempo, 4);
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
