//! Player *audio* per una singola clip: apre/segue l'`AudioPlayer` (vedi
//! ARCHITECTURE.md § Pipeline audio), la cui posizione è il clock di
//! sincronizzazione durante il playback. Senza audio (media muto) usa un
//! clock a parete (`Instant`) come fallback.
//!
//! Il VIDEO non passa più da qui: vive nel buffer a livello di timeline
//! (`render_ahead::RenderAhead`), che bufferizza N secondi avanti dal
//! playhead attraversando quante clip servono, invece di un decode-ahead
//! per singolo player. `Player` resta solo audio perché l'audio ha un
//! modello più semplice — nessun preload speciale necessario, riaprire un
//! `AudioPlayer` è economico grazie a `VibeVideoApp::audio_cache` (che
//! evita di ridecodificare l'intera traccia ogni volta, la causa
//! originale di un hitch percepibile risolta in una fix precedente) — a
//! differenza del video, dove il costo di un seek/riapertura dipende dal
//! GOP del sorgente e può arrivare a ~1s.
//!
//! L'anteprima "grezza" di un media dal media pool (non necessariamente
//! sulla timeline, vedi `VibeVideoApp::browsing_media`) ha bisogno anche
//! lei di un decode-ahead video, ma non passa da qui: usa un
//! `vv_media::DecodeAhead` a sé (`VibeVideoApp::browsing_decode_ahead`),
//! visto che non è legata a nessuna clip/posizione di timeline a cui
//! `render_ahead` potrebbe agganciarsi.
//!
//! Vive in vv-app perché orchestra vv-audio per uso esclusivo della UI:
//! non è (ancora) logica riutilizzabile al di fuori dell'app, quindi non
//! giustifica una crate propria (milestone 2).

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use vv_core::FrameIdx;

pub struct Player {
    audio: Option<vv_audio::AudioPlayer>,
    fps: f64,
    duration_secs: f64,
    playing: bool,
    wall_clock_started_at: Option<Instant>,
    wall_clock_base_secs: f64,
    /// Moltiplicatore di velocità (1.0/2.0/4.0, vedi `begin_speed_window`).
    /// Con audio, `self.audio` a `speed != 1.0` contiene solo una
    /// *finestra* di audio già time-stretched (pitch preservato) a questa
    /// velocità, non l'intera traccia (vedi doc di `window_origin_secs`) —
    /// tutte le posizioni pubbliche (`position_secs`/`seek_secs`) restano
    /// nello spazio tempo del media originale, tradotte qui.
    speed: f64,
    /// Posizione, nello spazio tempo del media *originale*, a cui
    /// corrisponde `audio.position_seconds() == 0.0` per la finestra di
    /// `audio` caricata ora — sempre `0.0` quando `speed == 1.0` (l'audio
    /// è il buffer originale per intero, non una finestra). Il chiamante
    /// (`VibeVideoApp::request_speed_window`) apre sempre una finestra a
    /// partire dal playhead corrente, quindi questo valore è fissato da
    /// `begin_speed_window` e non cambia quando la finestra viene poi
    /// estesa (`extend_speed_window`) — solo `AudioPlayer::extend_samples`
    /// fa crescere il buffer dietro le quinte.
    window_origin_secs: f64,
}

impl Player {
    /// `fps` è quello nativo del media (da `MediaMeta`, non più letto da
    /// un decoder video visto che qui non se ne apre più uno): serve a
    /// tradurre tra posizione in secondi e frame sorgente
    /// (`current_source_frame`/`seek_to_frame`).
    ///
    /// Se `cached_audio` è `Some`, riusa quel buffer già decodificato
    /// invece di ridecodificare da zero la traccia audio. Tocca al
    /// chiamante mantenere la cache per path (vedi
    /// `VibeVideoApp::audio_cache`); ritorna anche il buffer usato (nuovo
    /// se decodificato ora, lo stesso passato se riusato) perché il
    /// chiamante possa aggiornarla.
    ///
    /// `audio_stream_index` seleziona quale stream audio del media
    /// decodificare quando `cached_audio` è `None` (vedi
    /// `vv_media::decode_audio_track`/`Clip::audio_stream_index`). Il
    /// player dell'anteprima segue solo la clip *video* attiva
    /// (`VibeVideoApp::open_audio_player` chiama sempre con indice 0, lo
    /// stream "primario"): un'anteprima dal vivo che rispetti anche lo
    /// stream scelto sulla clip *audio* è un'estensione futura, stesso
    /// limite già noto di "player non ancora trim-aware" — l'export
    /// invece mixa ogni clip audio con il proprio `audio_stream_index`
    /// (vedi `export::mix_audio_track`).
    pub fn open(
        path: &Path,
        duration_secs: f64,
        fps: vv_core::Rational,
        cached_audio: Option<Arc<vv_media::AudioBuffer>>,
        audio_stream_index: usize,
    ) -> Result<(Self, Option<Arc<vv_media::AudioBuffer>>), String> {
        let audio_buffer = match cached_audio {
            Some(buf) => Some(buf),
            None => vv_media::decode_audio_track(path, audio_stream_index)
                .map_err(|e| e.to_string())?
                .map(Arc::new),
        };
        let audio = match &audio_buffer {
            Some(buf) => Some(vv_audio::AudioPlayer::new(
                buf.samples.clone(),
                buf.sample_rate,
                buf.channels,
            )?),
            None => None,
        };

        Ok((
            Self {
                audio,
                fps: fps.as_f64(),
                duration_secs,
                playing: false,
                wall_clock_started_at: None,
                wall_clock_base_secs: 0.0,
                speed: 1.0,
                window_origin_secs: 0.0,
            },
            audio_buffer,
        ))
    }

    pub fn play(&mut self) {
        if self.playing {
            return;
        }
        self.playing = true;
        match &self.audio {
            Some(audio) => audio.play(),
            None => self.wall_clock_started_at = Some(Instant::now()),
        }
    }

    pub fn pause(&mut self) {
        if !self.playing {
            return;
        }
        self.playing = false;
        match &self.audio {
            Some(audio) => audio.pause(),
            None => {
                if let Some(start) = self.wall_clock_started_at.take() {
                    self.wall_clock_base_secs += start.elapsed().as_secs_f64();
                }
            }
        }
    }

    pub fn toggle_play_pause(&mut self) {
        if self.playing {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Gain statico della clip in dB (milestone 5: `EffectStack::gain_db`).
    /// No-op se la clip non ha audio.
    pub fn set_gain_db(&self, db: f32) {
        if let Some(audio) = &self.audio {
            audio.set_gain_db(db);
        }
    }

    /// Picco lineare (sinistra, destra) dell'ultimo buffer audio
    /// riprodotto, per l'audiometer stereo nella UI (vedi
    /// `vv_audio::AudioPlayer::peak_linear_stereo`). `(0.0, 0.0)` se la
    /// clip non ha audio.
    pub fn peak_linear_stereo(&self) -> (f32, f32) {
        self.audio
            .as_ref()
            .map_or((0.0, 0.0), |a| a.peak_linear_stereo())
    }

    pub fn seek_secs(&mut self, secs: f64) {
        let secs = secs.clamp(0.0, self.duration_secs.max(0.0));
        if let Some(audio) = &self.audio {
            // `audio` può contenere solo una finestra di audio
            // time-stretched a `self.speed` a partire da
            // `self.window_origin_secs` (vedi doc del campo): va tradotta
            // prima di cercarci dentro.
            audio.seek_seconds((secs - self.window_origin_secs) / self.speed);
        }
        self.wall_clock_base_secs = secs;
        if self.playing {
            self.wall_clock_started_at = Some(Instant::now());
        }
    }

    /// `false` se `secs` (spazio del media originale) cade fuori dalla
    /// finestra di audio accelerato caricata: un seek lì verrebbe
    /// clampato al bordo della finestra.
    pub fn speed_window_covers_secs(&self, secs: f64) -> bool {
        match &self.audio {
            Some(audio) if (self.speed - 1.0).abs() >= 1e-9 => {
                let end = self.window_origin_secs + audio.duration_seconds() * self.speed;
                secs >= self.window_origin_secs && secs < end
            }
            _ => true,
        }
    }

    pub fn fps(&self) -> f64 {
        self.fps
    }

    /// Posizione nello spazio tempo del media *originale*, indipendente da
    /// `speed` — vedi doc del campo `speed`.
    pub fn position_secs(&self) -> f64 {
        match &self.audio {
            // Il cursore dell'AudioPlayer avanza solo mentre `playing` è
            // vero (il callback lo controlla), quindi resta corretto anche
            // da fermo: nessun bisogno di un ramo separato per la pausa.
            Some(audio) => self.window_origin_secs + audio.position_seconds() * self.speed,
            None if self.playing => {
                self.wall_clock_base_secs
                    + self
                        .wall_clock_started_at
                        .map(|t| t.elapsed().as_secs_f64() * self.speed)
                        .unwrap_or(0.0)
            }
            None => self.wall_clock_base_secs,
        }
    }

    /// Torna a velocità normale (1.0): per clip con audio riapre
    /// `AudioPlayer` sul buffer *originale* (mai stretchato, sempre
    /// istantaneo perché già in `VibeVideoApp::audio_cache`) alla
    /// posizione corrente — `original_audio` deve essere `Some` ogni
    /// volta che la clip attiva ha una traccia audio (nessuno stretch da
    /// aspettare per tornare indietro, a differenza di
    /// `begin_speed_window`). Per clip senza audio, `original_audio` è
    /// ignorato: la velocità si applica solo al clock interno. No-op se
    /// già a 1.0.
    pub fn reset_speed(&mut self, original_audio: Option<&Arc<vv_media::AudioBuffer>>) {
        if (self.speed - 1.0).abs() < 1e-9 {
            return;
        }
        let was_playing = self.playing;
        let pos = self.position_secs();
        self.speed = 1.0;
        self.window_origin_secs = 0.0;
        if let Some(buf) = original_audio {
            self.audio =
                vv_audio::AudioPlayer::new(buf.samples.clone(), buf.sample_rate, buf.channels)
                    .ok();
        }
        self.playing = false;
        self.wall_clock_started_at = None;
        self.seek_secs(pos);
        if was_playing {
            self.play();
        }
    }

    /// Cambia il moltiplicatore di velocità per una clip *senza* traccia
    /// audio (fallback a orologio a parete): non c'è nulla da stretchare,
    /// si applica solo al clock. Per clip con audio si usa invece
    /// `begin_speed_window`/`reset_speed`.
    pub fn set_speed_no_audio(&mut self, speed: f64) {
        debug_assert!(
            self.audio.is_none(),
            "set_speed_no_audio chiamato su una clip con audio"
        );
        self.speed = speed;
    }

    /// Avvia una nuova finestra di riproduzione accelerata: sostituisce
    /// l'audio corrente con `window_samples` (già stretchati a `tempo`,
    /// stesso `sample_rate`/`channels` del media originale), la cui
    /// posizione locale 0 corrisponde a `original_start_secs` nello
    /// spazio del media originale (vedi doc di `window_origin_secs`) —
    /// tocca al chiamante passare `window_samples` a partire esattamente
    /// dal playhead corrente. Usato sia per la primissima accelerazione
    /// (1x -> 2x) sia per un cambio di tier con una finestra già aperta
    /// (2x -> 4x, dove la finestra precedente non serve più) — mai per la
    /// sola estensione della finestra corrente, che passa invece da
    /// `extend_speed_window` per non riaprire lo stream audio (vedi doc
    /// di `vv_audio::AudioPlayer::extend_samples`). Mantiene lo stato
    /// play/pausa corrente.
    pub fn begin_speed_window(
        &mut self,
        tempo: f64,
        original_start_secs: f64,
        window_samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<(), String> {
        match &self.audio {
            // Stesso media, quindi stesso formato: niente riapertura dello stream.
            Some(audio) => audio.replace_samples(&window_samples),
            None => {
                let audio = vv_audio::AudioPlayer::new(window_samples, sample_rate, channels)?;
                if self.playing {
                    audio.play();
                }
                self.audio = Some(audio);
            }
        }
        self.speed = tempo;
        self.window_origin_secs = original_start_secs;
        self.wall_clock_started_at = None;
        Ok(())
    }

    /// Svuota la finestra accelerata e la fa ripartire da `secs`: silenzio
    /// e posizione ferma lì finché `begin_speed_window` non la riempie.
    pub fn restart_speed_window_at(&mut self, secs: f64) {
        if let Some(audio) = &self.audio {
            audio.replace_samples(&[]);
        }
        self.window_origin_secs = secs;
    }

    /// Accoda altro audio (stessa finestra/tempo di `begin_speed_window`)
    /// in coda allo stream senza interromperlo. No-op se la clip non ha
    /// audio (nessuna finestra attiva da estendere).
    pub fn extend_speed_window(&mut self, more_samples: &[f32]) {
        if let Some(audio) = &self.audio {
            audio.extend_samples(more_samples);
        }
    }

    pub fn duration_secs(&self) -> f64 {
        self.duration_secs
    }

    /// Seek per indice di frame sorgente (fps nativo del media), utile per
    /// tradurre una posizione della timeline in una posizione del player.
    pub fn seek_to_frame(&mut self, frame: FrameIdx) {
        self.seek_secs(frame as f64 / self.fps.max(1e-9));
    }

    /// Indice del frame sorgente corrispondente alla posizione attuale
    /// (nello spazio frame del media, fps nativo — non ancora traslato per
    /// il trim `source_in` di una clip, vedi nota in vv-app::main sul
    /// player non ancora trim-aware).
    pub fn current_source_frame(&self) -> FrameIdx {
        (self.position_secs() * self.fps).round() as FrameIdx
    }

    /// Da chiamare a ogni frame UI: mette in pausa automaticamente a fine
    /// clip (altrimenti il wall clock continuerebbe a correre oltre la
    /// durata).
    pub fn tick(&mut self) {
        if self.playing && self.position_secs() >= self.duration_secs {
            self.pause();
            self.wall_clock_base_secs = self.duration_secs;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    /// Clip senza audio: esercita il fallback a wall-clock (`Instant`) in
    /// modo deterministico, senza dipendere da un device audio reale.
    fn make_video_only_clip(duration_secs: u32) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("vv-app-player-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("video_only.mp4");

        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());
        path
    }

    const TEST_FPS: vv_core::Rational = vv_core::Rational::new(25, 1);

    #[test]
    fn wall_clock_playback_advances_pauses_and_seeks() {
        let path = make_video_only_clip(3);
        let (mut player, _audio_buffer) =
            Player::open(&path, 3.0, TEST_FPS, None, 0).expect("apertura player fallita");

        assert_eq!(player.position_secs(), 0.0);
        assert!(!player.is_playing());

        player.play();
        std::thread::sleep(Duration::from_millis(200));
        let pos_while_playing = player.position_secs();
        assert!(
            pos_while_playing > 0.1 && pos_while_playing < 1.0,
            "pos={pos_while_playing}"
        );

        player.pause();
        let pos_at_pause = player.position_secs();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            player.position_secs(),
            pos_at_pause,
            "il clock non deve avanzare da fermo"
        );

        player.seek_secs(2.0);
        assert!((player.position_secs() - 2.0).abs() < 0.01);

        // Oltre la durata: tick() deve auto-pausare.
        player.seek_secs(2.999);
        player.play();
        std::thread::sleep(Duration::from_millis(50));
        player.tick();
        assert!(!player.is_playing(), "deve auto-pausare a fine clip");
    }

    #[test]
    fn seek_to_frame_round_trips_with_current_source_frame() {
        let path = make_video_only_clip(2);
        let (mut player, _audio_buffer) = Player::open(&path, 2.0, TEST_FPS, None, 0).unwrap();

        player.seek_to_frame(20);
        assert_eq!(player.current_source_frame(), 20);

        player.seek_to_frame(0);
        assert_eq!(player.current_source_frame(), 0);
    }
}
