//! Output realtime via `cpal`.
//!
//! Milestone 2: un buffer audio pre-decodificato in RAM (vedi
//! `vv_media::decode_audio_track`) viene riprodotto con un cursore
//! condiviso (atomic) che il resto dell'app legge come clock di
//! sincronizzazione A/V — l'audio è il master durante il playback, vedi
//! ARCHITECTURE.md § Pipeline audio.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub struct AudioPlayer {
    _stream: cpal::Stream,
    playing: Arc<AtomicBool>,
    /// Posizione in *frame audio* (un campione per canale), non in byte né
    /// in campioni totali.
    position_frames: Arc<AtomicUsize>,
    /// Guadagno lineare (non dB) codificato come bit pattern di un f32, per
    /// poterlo leggere/scrivere via atomic dal callback realtime senza lock.
    gain_linear_bits: Arc<AtomicU32>,
    /// Picco assoluto (post-gain) del canale sinistro/mono e destro
    /// dell'ultimo buffer scritto dal callback, stesso incapsulamento
    /// bit-pattern di `gain_linear_bits`: per l'audiometer stereo nella UI
    /// (vedi `Player::peak_linear_stereo` in vv-app), non una misura
    /// accurata/professionale (nessun RMS, nessuna finestra — solo il
    /// valore assoluto massimo tra i campioni dell'ultimo callback). Un
    /// sorgente mono duplica lo stesso valore su entrambi.
    peak_left_bits: Arc<AtomicU32>,
    peak_right_bits: Arc<AtomicU32>,
    sample_rate: u32,
    channels: u16,
    /// Campioni interleaved dietro un lock invece di un `Arc<Vec<f32>>`
    /// immutabile: `extend_samples` vi accoda altro audio (finestra di
    /// speed-up successiva, vedi `vv-app::player::Player`) senza dover
    /// riaprire lo stream cpal, cosa che produrrebbe un piccolo click
    /// udibile a ogni estensione — accettabile per un cambio di velocità
    /// esplicito (raro), non per uno scorrimento di finestra continuo
    /// ogni pochi secondi durante il fast-forward. Il lock è preso anche
    /// dal callback realtime: tenuto per una copia di poche migliaia di
    /// campioni, mai per un'allocazione (quella la fa solo lo scrittore,
    /// fuori dal thread audio) — un compromesso pragmatico per un player
    /// di anteprima, non pensato per latenze da DAW professionale.
    samples: Arc<Mutex<Vec<f32>>>,
}

impl AudioPlayer {
    pub fn new(samples: Vec<f32>, sample_rate: u32, channels: u16) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("nessun device audio di output")?;

        let config = cpal::StreamConfig {
            channels,
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let playing = Arc::new(AtomicBool::new(false));
        let position_frames = Arc::new(AtomicUsize::new(0));
        let gain_linear_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let peak_left_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let peak_right_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let channels_usize = channels as usize;

        let samples = Arc::new(Mutex::new(samples));
        let cb_samples = samples.clone();
        let cb_playing = playing.clone();
        let cb_position = position_frames.clone();
        let cb_gain = gain_linear_bits.clone();
        let cb_peak_left = peak_left_bits.clone();
        let cb_peak_right = peak_right_bits.clone();

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !cb_playing.load(Ordering::Relaxed) {
                        data.fill(0.0);
                    } else {
                        let gain = f32::from_bits(cb_gain.load(Ordering::Relaxed));
                        let buf = cb_samples.lock().unwrap_or_else(|e| e.into_inner());
                        let total_frames = buf.len() / channels_usize.max(1);
                        let mut pos = cb_position.load(Ordering::Relaxed);
                        for frame in data.chunks_mut(channels_usize) {
                            if pos >= total_frames {
                                frame.fill(0.0);
                                continue;
                            }
                            let start = pos * channels_usize;
                            for (dst, src) in
                                frame.iter_mut().zip(&buf[start..start + channels_usize])
                            {
                                *dst = src * gain;
                            }
                            pos += 1;
                        }
                        drop(buf);
                        cb_position.store(pos, Ordering::Relaxed);
                    }
                    // Picco (post-gain) di questo buffer per canale, per
                    // l'audiometer stereo: calcolato qui in entrambi i rami
                    // (silenzio quando in pausa/a fine buffer inclusi), non
                    // solo quando si riproduce davvero, così il meter
                    // scende a zero da solo alla pausa invece di restare
                    // "incollato" all'ultimo valore. Un sorgente mono
                    // duplica lo stesso picco su entrambi i canali (nessuna
                    // allocazione qui: il thread audio non deve mai
                    // allocare).
                    let mut peak_left = 0.0f32;
                    let mut peak_right = 0.0f32;
                    for frame in data.chunks(channels_usize.max(1)) {
                        if let Some(&s) = frame.first() {
                            peak_left = peak_left.max(s.abs());
                        }
                        match frame.get(1) {
                            Some(&s) => peak_right = peak_right.max(s.abs()),
                            None => peak_right = peak_left,
                        }
                    }
                    cb_peak_left.store(peak_left.to_bits(), Ordering::Relaxed);
                    cb_peak_right.store(peak_right.to_bits(), Ordering::Relaxed);
                },
                move |err| eprintln!("vv-audio: errore stream: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;

        Ok(Self {
            _stream: stream,
            playing,
            position_frames,
            gain_linear_bits,
            peak_left_bits,
            peak_right_bits,
            sample_rate,
            channels,
            samples,
        })
    }

    /// Accoda altro audio interleaved (stesso `sample_rate`/canali di
    /// apertura) in coda al buffer corrente, senza interrompere lo stream
    /// né toccare la posizione di lettura — per estendere la finestra di
    /// una riproduzione accelerata in corso (vedi doc del campo
    /// `samples`).
    pub fn extend_samples(&self, more: &[f32]) {
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(more);
    }

    fn total_frames(&self) -> usize {
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).len() / self.channels.max(1) as usize
    }

    /// Imposta il guadagno in decibel (0.0 = invariato, -inf teorico -> 0
    /// lineare, valori positivi amplificano). Applicato in tempo reale nel
    /// callback audio, nessuna riconversione del buffer.
    pub fn set_gain_db(&self, db: f32) {
        self.gain_linear_bits
            .store(db_to_linear(db).to_bits(), Ordering::Relaxed);
    }

    /// Picco lineare (0.0..=1.0 di norma, può superare 1.0 con un gain
    /// positivo) di canale sinistro/mono e destro dell'ultimo buffer
    /// scritto dal callback audio: per un audiometer stereo nella UI, non
    /// una misura professionale (vedi doc dei campi `peak_left_bits`/
    /// `peak_right_bits`).
    pub fn peak_linear_stereo(&self) -> (f32, f32) {
        (
            f32::from_bits(self.peak_left_bits.load(Ordering::Relaxed)),
            f32::from_bits(self.peak_right_bits.load(Ordering::Relaxed)),
        )
    }

    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn seek_seconds(&self, secs: f64) {
        let frame = (secs.max(0.0) * self.sample_rate as f64) as usize;
        self.position_frames
            .store(frame.min(self.total_frames()), Ordering::Relaxed);
    }

    pub fn position_seconds(&self) -> f64 {
        self.position_frames.load(Ordering::Relaxed) as f64 / self.sample_rate as f64
    }

    pub fn duration_seconds(&self) -> f64 {
        self.total_frames() as f64 / self.sample_rate as f64
    }

    /// `true` quando il playback ha raggiunto la fine del buffer.
    pub fn finished(&self) -> bool {
        self.position_frames.load(Ordering::Relaxed) >= self.total_frames()
    }
}

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_to_linear_matches_known_reference_points() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        // -6dB ~= dimezza l'ampiezza; +6dB ~= raddoppia.
        assert!((db_to_linear(-6.0) - 0.5012).abs() < 1e-3);
        assert!((db_to_linear(6.0) - 1.9953).abs() < 1e-3);
        // -20dB = fattore 0.1 esatto.
        assert!((db_to_linear(-20.0) - 0.1).abs() < 1e-6);
    }

    /// Estendere il buffer non deve toccare la posizione di lettura né
    /// interrompere lo stream: la finestra scorrevole del fast-forward
    /// (vedi `vv-app::player::Player`) accoda audio mentre si suona senza
    /// riaprire `AudioPlayer`, per evitare il click di una riapertura a
    /// ogni estensione.
    #[test]
    fn extend_samples_grows_duration_without_disturbing_playback_position() {
        let sample_rate = 44_100;
        let channels = 1u16;
        let initial: Vec<f32> = vec![0.5; sample_rate as usize]; // 1s
        let player = AudioPlayer::new(initial, sample_rate, channels).unwrap();

        assert!((player.duration_seconds() - 1.0).abs() < 1e-6);
        player.seek_seconds(0.5);
        assert!((player.position_seconds() - 0.5).abs() < 1e-6);

        let more: Vec<f32> = vec![0.25; sample_rate as usize]; // +1s
        player.extend_samples(&more);

        assert!(
            (player.duration_seconds() - 2.0).abs() < 1e-6,
            "duration={}",
            player.duration_seconds()
        );
        assert!(
            (player.position_seconds() - 0.5).abs() < 1e-6,
            "extend_samples non deve spostare la posizione di lettura"
        );
        assert!(!player.finished());
    }
}
