//! Player lineare per una singola clip: combina il decode-ahead video
//! (`vv_media::DecodeAhead`) e l'output audio (`vv_audio::AudioPlayer`),
//! che è il clock di sincronizzazione quando presente (vedi
//! ARCHITECTURE.md § Pipeline audio). Senza audio si usa un clock a parete
//! (`Instant`) come fallback.
//!
//! Vive in vv-app perché orchestra due crate diverse (vv-media + vv-audio)
//! per uso esclusivo della UI: non è (ancora) logica riutilizzabile al di
//! fuori dell'app, quindi non giustifica una crate propria (milestone 2).

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use vv_core::FrameIdx;
use vv_media::FrameRgba;

pub struct Player {
    decode_ahead: vv_media::DecodeAhead,
    audio: Option<vv_audio::AudioPlayer>,
    fps: f64,
    duration_secs: f64,
    playing: bool,
    wall_clock_started_at: Option<Instant>,
    wall_clock_base_secs: f64,
}

impl Player {
    pub fn open(path: &Path, duration_secs: f64) -> Result<Self, String> {
        let decode_ahead =
            vv_media::DecodeAhead::spawn(path.to_path_buf(), 300, 60).map_err(|e| e.to_string())?;
        let fps = decode_ahead.fps.as_f64();

        let audio = match vv_media::decode_audio_track(path).map_err(|e| e.to_string())? {
            Some(buf) => Some(vv_audio::AudioPlayer::new(
                buf.samples,
                buf.sample_rate,
                buf.channels,
            )?),
            None => None,
        };

        Ok(Self {
            decode_ahead,
            audio,
            fps,
            duration_secs,
            playing: false,
            wall_clock_started_at: None,
            wall_clock_base_secs: 0.0,
        })
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

    pub fn seek_secs(&mut self, secs: f64) {
        let secs = secs.clamp(0.0, self.duration_secs.max(0.0));
        if let Some(audio) = &self.audio {
            audio.seek_seconds(secs);
        }
        self.wall_clock_base_secs = secs;
        if self.playing {
            self.wall_clock_started_at = Some(Instant::now());
        }
        let frame_idx = (secs * self.fps).round() as FrameIdx;
        self.decode_ahead.seek(frame_idx, secs);
    }

    pub fn position_secs(&self) -> f64 {
        match &self.audio {
            // Il cursore dell'AudioPlayer avanza solo mentre `playing` è
            // vero (il callback lo controlla), quindi resta corretto anche
            // da fermo: nessun bisogno di un ramo separato per la pausa.
            Some(audio) => audio.position_seconds(),
            None if self.playing => {
                self.wall_clock_base_secs
                    + self
                        .wall_clock_started_at
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(0.0)
            }
            None => self.wall_clock_base_secs,
        }
    }

    pub fn duration_secs(&self) -> f64 {
        self.duration_secs
    }

    /// Frame corrente da mostrare nel viewer, se già decodificato. Aggiorna
    /// anche il target del decode-ahead alla posizione corrente.
    pub fn current_frame(&self) -> Option<Arc<FrameRgba>> {
        let idx = (self.position_secs() * self.fps).round() as FrameIdx;
        self.decode_ahead.set_target(idx);
        self.decode_ahead.cache().get(idx)
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

    #[test]
    fn wall_clock_playback_advances_pauses_and_seeks() {
        let path = make_video_only_clip(3);
        let mut player = Player::open(&path, 3.0).expect("apertura player fallita");

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
}
