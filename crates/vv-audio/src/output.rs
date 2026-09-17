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
    /// Canali della *sorgente* (quelli in cui arrivano i campioni passati
    /// a `new`/`extend_samples`): serve a `extend_samples` per fare il
    /// downmix di ogni chunk accodato con la stessa conversione usata in
    /// apertura, vedi doc di `channels`.
    source_channels: u16,
    /// Canali con cui è stato aperto lo stream cpal — non necessariamente
    /// uguali a `source_channels`: `new` fa il downmix/upmix al numero di
    /// canali *nativo del device* (`Device::default_output_config`)
    /// prima di aprire lo stream, invece di chiedere a cpal/PipeWire il
    /// conteggio canali della sorgente as-is. Chiedere ad es. 6 canali
    /// (un file con traccia audio 5.1) su un device stereo costringe
    /// PipeWire a inserire uno stadio di remix nel suo grafo, che in
    /// pratica introduce latenza in uscita udibile — il playhead segue la
    /// posizione di *decodifica* (quanto scritto nel buffer del
    /// callback), non l'istante in cui il suono esce dagli altoparlanti,
    /// quindi quella latenza si vede come un disallineamento fisso tra
    /// audio e waveform/playhead (bug segnalato: "il picco è a 51s ma la
    /// waveform lo mostra a 49-50s", su un file con audio 5.1 riprodotto
    /// su un device a 2 canali). Il downmix qui, prima di consegnare i
    /// campioni a cpal, tiene la conversione fuori dal grafo PipeWire.
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
    /// `source_channels` sono i canali dei campioni interleaved passati
    /// qui (tipicamente quelli della traccia audio decodificata): lo
    /// stream cpal viene aperto invece al numero di canali nativo del
    /// device di output (vedi doc del campo `channels`), con un downmix
    /// (o upmix, es. mono su device stereo) fatto qui una sola volta sul
    /// buffer intero anziché ad ogni callback.
    pub fn new(samples: Vec<f32>, sample_rate: u32, source_channels: u16) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("nessun device audio di output")?;

        // Canali nativi del device: se non disponibili (raro), ricade sui
        // canali della sorgente (comportamento di prima, nessun downmix).
        let channels = device
            .default_output_config()
            .map(|c| c.channels())
            .unwrap_or(source_channels);
        let samples = downmix_interleaved(&samples, source_channels, channels);

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
            source_channels,
            channels,
            samples,
        })
    }

    /// Accoda altro audio interleaved (stesso `sample_rate`/canali *della
    /// sorgente* passati a `new`, non necessariamente quelli dello stream
    /// cpal aperto — vedi doc di `source_channels`/`channels`) in coda al
    /// buffer corrente, senza interrompere lo stream né toccare la
    /// posizione di lettura — per estendere la finestra di una
    /// riproduzione accelerata in corso (vedi doc del campo `samples`).
    pub fn extend_samples(&self, more: &[f32]) {
        let more = downmix_interleaved(more, self.source_channels, self.channels);
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(&more);
    }

    /// Sostituisce l'intero buffer (stessi `sample_rate`/canali sorgente di
    /// `new`) e riporta la posizione a 0, senza riaprire lo stream cpal:
    /// riaprirlo blocca il thread chiamante per centinaia di ms.
    pub fn replace_samples(&self, samples: &[f32]) {
        let samples = downmix_interleaved(samples, self.source_channels, self.channels);
        let mut buf = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        *buf = samples;
        self.position_frames.store(0, Ordering::Relaxed);
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

pub(crate) fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Converte campioni interleaved da `from` a `to` canali: nessuna semantica
/// di layout (L/R/center/LFE/...) — non è un downmix "broadcast-accurate",
/// solo un compromesso semplice e simmetrico che tiene l'audio comunque
/// intelligibile e allineato ai tempi reali, evitando di chiedere a
/// cpal/PipeWire un conteggio canali che il device non supporta nativamente
/// (vedi doc del campo `AudioPlayer::channels` sul perché quello introduce
/// latenza in uscita).
///
/// - `from == to` (o uno dei due è 0): nessuna conversione, copia diretta.
/// - `to == 1`: media di tutti i canali sorgente per ogni frame.
/// - `from > to` (downmix, es. 6→2): i canali sorgente sono distribuiti a
///   turno sui canali di destinazione (`indice_sorgente % to`) e mediati —
///   con l'ordine canali tipico ffmpeg per il 5.1 (L,R,C,LFE,Ls,Rs) questo
///   raggruppa L,C,Ls sul primo canale e R,LFE,Rs sul secondo, cioè
///   approssima un downmix stereo ragionevole senza dover conoscere il
///   channel layout.
/// - `from < to` (upmix, es. mono→stereo): i canali sorgente sono replicati
///   a turno (`indice_dest % from`) sui canali di destinazione.
pub(crate) fn downmix_interleaved(samples: &[f32], from: u16, to: u16) -> Vec<f32> {
    if from == to || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let from = from as usize;
    let to = to as usize;
    let frames = samples.len() / from;
    let mut out = vec![0.0f32; frames * to];

    if to == 1 {
        for f in 0..frames {
            let src = &samples[f * from..f * from + from];
            out[f] = src.iter().sum::<f32>() / from as f32;
        }
        return out;
    }

    if from > to {
        let mut sums = vec![0.0f32; to];
        let mut counts = vec![0u32; to];
        for f in 0..frames {
            sums.fill(0.0);
            counts.fill(0);
            let src = &samples[f * from..f * from + from];
            for (i, &s) in src.iter().enumerate() {
                let bucket = i % to;
                sums[bucket] += s;
                counts[bucket] += 1;
            }
            let dst = &mut out[f * to..f * to + to];
            for c in 0..to {
                dst[c] = if counts[c] > 0 { sums[c] / counts[c] as f32 } else { 0.0 };
            }
        }
    } else {
        for f in 0..frames {
            let src = &samples[f * from..f * from + from];
            let dst = &mut out[f * to..f * to + to];
            for c in 0..to {
                dst[c] = src[c % from];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_interleaved_is_a_noop_when_channel_counts_match() {
        let samples = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(downmix_interleaved(&samples, 2, 2), samples);
    }

    #[test]
    fn downmix_interleaved_averages_all_channels_to_mono() {
        // Un frame stereo [1.0, 0.0] -> mono deve dare la media, 0.5.
        let samples = vec![1.0, 0.0, 0.5, 0.5];
        let mono = downmix_interleaved(&samples, 2, 1);
        assert_eq!(mono, vec![0.5, 0.5]);
    }

    #[test]
    fn downmix_interleaved_six_to_two_groups_even_and_odd_channels() {
        // Ordine tipico ffmpeg per il 5.1(side): L,R,C,LFE,Ls,Rs. Con
        // indice pari -> canale 0 (L,C,Ls) e dispari -> canale 1
        // (R,LFE,Rs): un frame con L=1.0 e tutti gli altri a 0 deve finire
        // quasi tutto sul canale 0 (media di 1.0,0.0,0.0 = 1/3), niente
        // sul canale 1.
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
        let samples = vec![0.0f32; 6 * 100]; // 100 frame a 6 canali
        let stereo = downmix_interleaved(&samples, 6, 2);
        assert_eq!(stereo.len(), 2 * 100);
    }

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
