//! Finestra egui: media pool (sinistra), viewer (centro), timeline
//! multi-traccia (basso), toolbar con play/pause/seek per l'anteprima.
//!
//! Il viewer mostra l'anteprima del media della clip selezionata in
//! timeline, con crop/zoom/gain (milestone 5, valori statici) applicati
//! tramite il compositor GPU di `vv-render`. Non è ancora il compositing
//! multi-track completo della timeline (che richiede risolvere "quale clip
//! è attiva su quale track a che tempo" per ogni frame, milestone
//! successiva) — qui si vede sempre e solo la clip selezionata, non il
//! risultato finale con tutte le track sovrapposte.
//!
//! La texture del frame corrente viene riusata in-place a ogni frame
//! (`TextureHandle::set`) invece di riallocarla 25-60 volte al secondo. Il
//! compositor fa oggi un round-trip CPU->GPU->CPU per restare compatibile
//! con questo path: passare la texture GPU direttamente a egui (zero-copy)
//! è un'ottimizzazione futura, vedi doc di `vv_render::Compositor`.

mod player;
mod timeline_ui;

use player::Player;
use std::collections::HashMap;
use std::path::PathBuf;
use vv_core::{ClipId, FrameIdx, MediaId, TimelineId, Track, TrackKind};

/// Unica track video supportata per ora dalla riproduzione timeline-aware
/// (l'app crea sempre "video in track 0, audio in track 1", vedi
/// `add_media_to_timeline`): il compositing multi-track vero e proprio
/// resta per una milestone successiva.
const VIDEO_TRACK: usize = 0;

enum PoolAction {
    Preview(MediaId),
    AddToTimeline(MediaId),
}

/// Snapshot dei campi della clip selezionata che servono al pannello
/// proprietà, valutati al `source_frame` corrente. Una struct invece di
/// una tupla perché i campi hanno continuato a crescere con ogni nuova
/// proprietà keyframeable (transform, gain, ora color).
struct ClipPanelInfo {
    start: FrameIdx,
    len: FrameIdx,
    is_solid_color: bool,
    transform_constant: bool,
    transform_kf_here: bool,
    transform: vv_core::Transform,
    gain_constant: bool,
    gain_kf_here: bool,
    gain: f32,
    color_constant: bool,
    color_kf_here: bool,
    color: vv_core::Rgba,
}

/// Azione differita sugli effetti di una clip, raccolta durante il disegno
/// del pannello proprietà (che prende in prestito `self` immutabilmente) e
/// applicata subito dopo — stesso schema di `timeline_ui::PendingAction`.
enum PendingEffectChange {
    SetTransformDefault(usize, ClipId, vv_core::Transform),
    SetGainDefault(usize, ClipId, f32),
    SetColorDefault(usize, ClipId, vv_core::Rgba),
    UpsertTransformKeyframe(usize, ClipId, FrameIdx, vv_core::Transform),
    UpsertGainKeyframe(usize, ClipId, FrameIdx, f32),
    UpsertColorKeyframe(usize, ClipId, FrameIdx, vv_core::Rgba),
    RemoveTransformKeyframe(usize, ClipId, FrameIdx),
    RemoveGainKeyframe(usize, ClipId, FrameIdx),
    RemoveColorKeyframe(usize, ClipId, FrameIdx),
}

struct VibeVideoApp {
    project: vv_core::Project,
    history: vv_core::History,
    timeline_id: Option<TimelineId>,
    timeline_state: timeline_ui::TimelineState,
    import_error: Option<String>,

    preview_path: Option<PathBuf>,
    preview_meta: Option<vv_core::MediaMeta>,
    preview_player: Option<Player>,
    preview_error: Option<String>,
    frame_texture: Option<egui::TextureHandle>,

    /// Clip la cui anteprima è attualmente mostrata: guida sia il player
    /// (quale media riprodurre) sia il transform/gain applicati (milestone
    /// 5). `None` se non c'è ancora una clip selezionata.
    active_clip: Option<(usize, ClipId)>,
    compositor: vv_render::Compositor,

    /// Ultimo `timeline_state.playhead` già gestito da
    /// `ensure_active_clip_matches_playhead`: usato per distinguere "il
    /// player sta scrivendo il playhead lui stesso durante il playback"
    /// (nessun seek da fare, lo fa già avanzare la decodifica) da "l'utente
    /// ha trascinato il playhead da fermo" (serve un seek esplicito).
    last_synced_playhead: FrameIdx,

    /// Media aperto tramite il pulsante "Anteprima" del media pool (non
    /// ancora/non necessariamente sulla timeline): mentre è `Some`, il
    /// viewer mostra quel media al posto di quello guidato dal playhead, e
    /// `active_clip` resta `None` (nessun transform/gain di una clip si
    /// applica a un'anteprima "grezza"). Si esce da questa modalità
    /// interagendo con la timeline (selezione o playhead).
    browsing_media: Option<MediaId>,
}

impl Default for VibeVideoApp {
    fn default() -> Self {
        Self {
            project: vv_core::Project::default(),
            history: vv_core::History::default(),
            timeline_id: None,
            timeline_state: timeline_ui::TimelineState::default(),
            import_error: None,
            preview_path: None,
            preview_meta: None,
            preview_player: None,
            preview_error: None,
            frame_texture: None,
            active_clip: None,
            compositor: vv_render::Compositor::new_headless(),
            last_synced_playhead: 0,
            browsing_media: None,
        }
    }
}

impl VibeVideoApp {
    fn import_media(&mut self, path: PathBuf) {
        match vv_media::probe(&path) {
            Ok(meta) => {
                self.import_error = None;
                if self.timeline_id.is_none() {
                    let id = self.project.timelines.insert(vv_core::Timeline {
                        name: "Timeline 1".into(),
                        fps: meta.fps,
                        resolution: (meta.width, meta.height),
                        tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
                    });
                    self.timeline_id = Some(id);
                }
                let media_id = self.project.media_pool.insert(vv_core::MediaItem {
                    path: path.clone(),
                    meta,
                    content_hash: 0, // placeholder: hashing reale arriva con la cache (milestone 8)
                });
                self.preview_media(media_id);
            }
            Err(e) => self.import_error = Some(e.to_string()),
        }
    }

    fn preview_media(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let path = item.path.clone();
        let meta = item.meta.clone();
        let duration_secs = meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9);

        self.frame_texture = None;
        self.preview_player = None;
        self.preview_error = None;

        match Player::open(&path, duration_secs) {
            Ok(player) => self.preview_player = Some(player),
            Err(e) => self.preview_error = Some(e),
        }
        self.preview_path = Some(path);
        self.preview_meta = Some(meta);
    }

    /// La clip su `track_index` che copre `frame` (nello spazio della
    /// timeline), se c'è.
    fn clip_at(&self, track_index: usize, frame: FrameIdx) -> Option<ClipId> {
        let timeline_id = self.timeline_id?;
        self.project.timelines[timeline_id]
            .tracks
            .get(track_index)?
            .clips
            .iter()
            .find(|c| frame >= c.timeline_start && frame < c.timeline_end())
            .map(|c| c.id)
    }

    /// Carica `clip_id` (sulla track video) nel player, posizionandosi al
    /// frame locale corrispondente al playhead corrente — sia per lo
    /// scrub in una clip diversa sia per il primo aggancio all'avvio della
    /// riproduzione. Le clip SolidColor non hanno un player: il colore si
    /// legge direttamente nel pannello centrale dal playhead.
    fn load_video_clip(&mut self, clip_id: ClipId) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(clip) = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .cloned()
        else {
            return;
        };

        self.active_clip = Some((VIDEO_TRACK, clip_id));

        match clip.source {
            vv_core::ClipSource::Media(media_id) => {
                self.preview_media(media_id);
                if let Some(player) = &self.preview_player {
                    player.set_gain_db(clip.effects.gain_db.default);
                }
                let local = (self.timeline_state.playhead - clip.timeline_start)
                    .clamp(0, clip.timeline_len().saturating_sub(1));
                if let Some(player) = &mut self.preview_player {
                    player.seek_to_frame(clip.source_in + local);
                }
            }
            vv_core::ClipSource::SolidColor => {
                self.preview_player = None;
                self.frame_texture = None;
                self.preview_error = None;
            }
        }
    }

    /// Se non stiamo mostrando un'anteprima "grezza" da media pool, tiene
    /// il player agganciato a qualunque clip copra il playhead sulla track
    /// video: ricarica solo quando cambia davvero (scrub, selezione di
    /// un'altra zona, o dopo un avanzamento automatico).
    fn ensure_active_clip_matches_playhead(&mut self) {
        if self.browsing_media.is_some() || self.timeline_id.is_none() {
            return;
        }
        let desired = self.clip_at(VIDEO_TRACK, self.timeline_state.playhead);
        let current = self
            .active_clip
            .filter(|(t, _)| *t == VIDEO_TRACK)
            .map(|(_, id)| id);

        if desired != current {
            match desired {
                // `load_video_clip` fa già un seek verso il playhead
                // corrente: non serve altro qui.
                Some(clip_id) => self.load_video_clip(clip_id),
                None => {
                    self.active_clip = None;
                    self.preview_player = None;
                    self.frame_texture = None;
                }
            }
        } else if let Some(clip_id) = desired
            && self.timeline_state.playhead != self.last_synced_playhead
            && !self.preview_player.as_ref().is_some_and(Player::is_playing)
        {
            // Stessa clip, da fermo, ma il playhead si è mosso (scrub):
            // segui con un seek nella clip già aperta.
            self.seek_active_player_to_playhead(clip_id);
        }
        self.last_synced_playhead = self.timeline_state.playhead;
    }

    /// Fa un seek del player già aperto verso il punto della clip
    /// corrispondente al playhead corrente (senza riaprirlo).
    fn seek_active_player_to_playhead(&mut self, clip_id: ClipId) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(clip) = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .find(|c| c.id == clip_id)
        else {
            return;
        };
        let local = (self.timeline_state.playhead - clip.timeline_start)
            .clamp(0, clip.timeline_len().saturating_sub(1));
        let target = clip.source_in + local;
        if let Some(player) = &mut self.preview_player {
            player.seek_to_frame(target);
        }
    }

    /// Fa play/pause sulla clip sotto al playhead, senza bisogno che sia
    /// selezionata (bug: "la riproduzione parte solo se seleziono la
    /// clip"). Se il player non è ancora agganciato lo aggancia subito.
    fn toggle_playback(&mut self) {
        self.browsing_media = None;
        self.ensure_active_clip_matches_playhead();
        if let Some(player) = &mut self.preview_player {
            player.toggle_play_pause();
        }
    }

    /// Guida la riproduzione a ogni frame UI: se il player sta suonando,
    /// il playhead lo segue; se ha raggiunto la fine del *trim* della clip
    /// attiva (non della fine del file, che può essere più lunga), avanza
    /// automaticamente alla prossima clip sulla track video invece di
    /// continuare a riprodurre oltre il taglio (bug: "il playhead si
    /// ferma al primo taglio e il riquadro continua a riprodurre la clip
    /// per intero").
    ///
    /// Limite noto: se la prossima clip è un generatore SolidColor la
    /// riproduzione si ferma lì, perché un generatore non ha un player che
    /// faccia da orologio — avanzare il tempo "a vuoto" durante un
    /// generatore è un'estensione futura.
    fn drive_playback(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some((track_index, clip_id)) = self.active_clip else {
            return;
        };
        if track_index != VIDEO_TRACK {
            return;
        }
        let Some(clip) = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .cloned()
        else {
            return;
        };
        let Some(player) = &self.preview_player else {
            return;
        };
        if !player.is_playing() {
            return;
        }

        if player.current_source_frame() >= clip.source_out {
            self.advance_playback_past(timeline_id, clip.timeline_end());
        } else {
            let local = player.current_source_frame() - clip.source_in;
            self.timeline_state.playhead = clip.timeline_start + local;
        }
    }

    /// Trova la prima clip sulla track video con inizio >= `from_frame` e
    /// ci salta, continuando la riproduzione da lì; se non c'è nessuna
    /// clip successiva, mette in pausa (fine del contenuto).
    fn advance_playback_past(&mut self, timeline_id: TimelineId, from_frame: FrameIdx) {
        let next = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .filter(|c| c.timeline_start >= from_frame)
            .min_by_key(|c| c.timeline_start)
            .map(|c| (c.id, c.timeline_start));

        match next {
            Some((next_id, next_start)) => {
                self.timeline_state.playhead = next_start;
                self.load_video_clip(next_id);
                if let Some(player) = &mut self.preview_player {
                    player.play();
                }
            }
            None => {
                if let Some(player) = &mut self.preview_player {
                    player.pause();
                }
            }
        }
    }

    fn ensure_timeline(&mut self) -> TimelineId {
        if let Some(id) = self.timeline_id {
            return id;
        }
        let id = self.project.timelines.insert(vv_core::Timeline {
            name: "Timeline 1".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        self.timeline_id = Some(id);
        id
    }

    /// Crea una clip generatore SolidColor da 5s e la accoda in fondo alla
    /// track video 0. Il colore iniziale è grigio medio, modificabile
    /// subito dal pannello proprietà una volta selezionata.
    fn add_solid_color_clip(&mut self) {
        let timeline_id = self.ensure_timeline();
        let fps = self.project.timelines[timeline_id].fps.as_f64();
        let default_len = (fps * 5.0).round() as FrameIdx;
        let video_start = track_end(&self.project, timeline_id, 0);

        let effects = vv_core::EffectStack {
            color: Some(vv_core::Keyframed::constant(vv_core::Rgba {
                r: 0.6,
                g: 0.6,
                b: 0.6,
                a: 1.0,
            })),
            ..Default::default()
        };

        let clip = vv_core::Clip {
            id: self.project.alloc_clip_id(),
            source: vv_core::ClipSource::SolidColor,
            source_in: 0,
            source_out: default_len,
            timeline_start: video_start,
            effects,
            linked: None,
        };
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip,
            }),
        );
    }

    fn apply_active_clip_gain(&mut self) {
        let Some(gain) = self.active_clip_effects().map(|e| e.gain_db.default) else {
            return;
        };
        if let Some(player) = &self.preview_player {
            player.set_gain_db(gain);
        }
    }

    fn active_clip_effects(&self) -> Option<&vv_core::EffectStack> {
        let (track_index, clip_id) = self.active_clip?;
        let timeline_id = self.timeline_id?;
        self.project.timelines[timeline_id]
            .tracks
            .get(track_index)?
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .map(|c| &c.effects)
    }

    fn add_media_to_timeline(&mut self, media_id: MediaId) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let meta = item.meta.clone();

        let video_start = track_end(&self.project, timeline_id, 0);
        let video_clip_id = self.project.alloc_clip_id();
        // Se la clip ha anche audio, le due metà vengono collegate di
        // default (vedi doc di `Clip::linked`): l'utente può scollegarle
        // dal menu contestuale sulla clip, in timeline_ui.
        let audio_clip_id = meta.has_audio.then(|| self.project.alloc_clip_id());

        let video_clip = vv_core::Clip {
            id: video_clip_id,
            source: vv_core::ClipSource::Media(media_id),
            source_in: 0,
            source_out: meta.duration_frames,
            timeline_start: video_start,
            effects: vv_core::EffectStack::default(),
            linked: audio_clip_id,
        };
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip: video_clip,
            }),
        );

        if let Some(audio_clip_id) = audio_clip_id {
            let audio_start = track_end(&self.project, timeline_id, 1);
            let audio_clip = vv_core::Clip {
                id: audio_clip_id,
                source: vv_core::ClipSource::Media(media_id),
                source_in: 0,
                source_out: meta.duration_frames,
                timeline_start: audio_start,
                effects: vv_core::EffectStack::default(),
                linked: Some(video_clip_id),
            };
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::InsertClip {
                    timeline: timeline_id,
                    track_index: 1,
                    clip: audio_clip,
                }),
            );
        }
    }

    /// Normal delete: rimuove la clip selezionata, lascia un vuoto al suo
    /// posto sulla track. Le altre track non si muovono.
    fn delete_selected(&mut self) {
        if let (Some(timeline_id), Some((track_index, clip_id))) =
            (self.timeline_id, self.timeline_state.selected)
        {
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::LiftDelete::new(timeline_id, track_index, clip_id)),
            );
            self.timeline_state.selected = None;
        }
    }

    /// Ripple delete: rimuove la clip selezionata e chiude il gap su
    /// *tutte* le track, mantenendo il sync audio/video (vedi
    /// ARCHITECTURE.md § Ripple delete — comportamento scelto: sempre
    /// globale, nessun toggle).
    fn ripple_delete_selected(&mut self) {
        if let (Some(timeline_id), Some((track_index, clip_id))) =
            (self.timeline_id, self.timeline_state.selected)
        {
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::RippleDeleteAllTracks::new(
                    timeline_id,
                    track_index,
                    clip_id,
                )),
            );
            self.timeline_state.selected = None;
        }
    }

    /// Divide *tutte* le clip che coprono il playhead, su ogni track (tasto
    /// T): comportamento standard da "lametta", non richiede una
    /// selezione (bug: "il taglio funzionava solo sulla track
    /// selezionata"). Un solo passo di history per l'intero taglio.
    fn split_all_at_playhead(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let playhead = self.timeline_state.playhead;
        let targets: Vec<(usize, ClipId)> = self.project.timelines[timeline_id]
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(track_index, track)| {
                track
                    .clips
                    .iter()
                    .filter(move |c| playhead > c.timeline_start && playhead < c.timeline_end())
                    .map(move |c| (track_index, c.id))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let commands = targets
            .into_iter()
            .map(|(track_index, clip_id)| {
                Box::new(vv_core::SplitClip::new(
                    timeline_id,
                    track_index,
                    clip_id,
                    playhead,
                )) as Box<dyn vv_core::Command>
            })
            .collect();
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(commands)),
        );
    }
}

fn track_end(project: &vv_core::Project, timeline_id: TimelineId, track_index: usize) -> FrameIdx {
    project.timelines[timeline_id]
        .tracks
        .get(track_index)
        .and_then(|t| t.clips.iter().map(|c| c.timeline_end()).max())
        .unwrap_or(0)
}

fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

/// Bottone diamante per il toggle keyframe di un parametro, allo stile
/// standard delle NLE: vuoto se il parametro non è animato (click = crea il
/// primo keyframe qui), pieno se c'è già un keyframe esattamente al frame
/// corrente (click = rimuovilo), vuoto-ma-animato altrimenti (click =
/// aggiungine uno qui).
fn keyframe_button(
    ui: &mut egui::Ui,
    is_constant: bool,
    has_keyframe_here: bool,
) -> egui::Response {
    let (symbol, tooltip) = if is_constant {
        ("◇", "Anima: crea il primo keyframe qui")
    } else if has_keyframe_here {
        ("◆", "Rimuovi il keyframe qui")
    } else {
        ("◇", "Aggiungi un keyframe qui")
    };
    ui.button(symbol).on_hover_text(tooltip)
}

/// Traduce un'azione differita del pannello proprietà nel comando
/// `vv-core` corrispondente. Funzione libera (non un metodo) apposta:
/// testabile senza passare da un `egui::Context`.
fn build_effect_command(
    timeline_id: TimelineId,
    change: PendingEffectChange,
) -> Box<dyn vv_core::Command> {
    match change {
        PendingEffectChange::SetTransformDefault(track_index, clip_id, v) => Box::new(
            vv_core::SetClipTransform::new(timeline_id, track_index, clip_id, v),
        ),
        PendingEffectChange::SetGainDefault(track_index, clip_id, v) => Box::new(
            vv_core::SetClipGain::new(timeline_id, track_index, clip_id, v),
        ),
        PendingEffectChange::SetColorDefault(track_index, clip_id, v) => Box::new(
            vv_core::SetClipColor::new(timeline_id, track_index, clip_id, v),
        ),
        PendingEffectChange::UpsertTransformKeyframe(track_index, clip_id, frame, v) => {
            Box::new(vv_core::UpsertKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                frame,
                vv_core::KeyframeValue::Transform(v),
                vv_core::Interpolation::Linear,
            ))
        }
        PendingEffectChange::UpsertGainKeyframe(track_index, clip_id, frame, v) => {
            Box::new(vv_core::UpsertKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                frame,
                vv_core::KeyframeValue::Gain(v),
                vv_core::Interpolation::Linear,
            ))
        }
        PendingEffectChange::UpsertColorKeyframe(track_index, clip_id, frame, v) => {
            Box::new(vv_core::UpsertKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                frame,
                vv_core::KeyframeValue::Color(v),
                vv_core::Interpolation::Linear,
            ))
        }
        PendingEffectChange::RemoveTransformKeyframe(track_index, clip_id, frame) => {
            Box::new(vv_core::RemoveKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                vv_core::KeyframeTarget::Transform,
                frame,
            ))
        }
        PendingEffectChange::RemoveGainKeyframe(track_index, clip_id, frame) => {
            Box::new(vv_core::RemoveKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                vv_core::KeyframeTarget::Gain,
                frame,
            ))
        }
        PendingEffectChange::RemoveColorKeyframe(track_index, clip_id, frame) => {
            Box::new(vv_core::RemoveKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                vv_core::KeyframeTarget::Color,
                frame,
            ))
        }
    }
}

impl eframe::App for VibeVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(player) = &mut self.preview_player {
            player.tick();
        }

        // Gain keyframeato: va aggiornato a ogni frame UI in base alla
        // posizione corrente del player (control-rate ~60Hz, non
        // sample-accurate: sufficiente per l'automazione in anteprima,
        // l'export a milestone 9 potrà fare di meglio se servirà).
        if let Some(effects) = self.active_clip_effects()
            && !effects.gain_db.is_constant()
            && let Some(player) = &self.preview_player
        {
            let gain = effects.gain_db.value_at(player.current_source_frame());
            player.set_gain_db(gain);
        }

        ui.input(|i| {
            let delete_pressed =
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace);
            if delete_pressed && i.modifiers.shift {
                self.ripple_delete_selected();
            } else if delete_pressed {
                self.delete_selected();
            }
            if i.key_pressed(egui::Key::T) && !i.modifiers.command {
                self.split_all_at_playhead();
            }
            if i.modifiers.command && i.key_pressed(egui::Key::Z) {
                if i.modifiers.shift {
                    self.history.redo(&mut self.project);
                } else {
                    self.history.undo(&mut self.project);
                }
            }
            if i.key_pressed(egui::Key::Space) {
                self.toggle_playback();
            }
        });

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Importa media...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("video", &["mp4", "mov", "mkv", "avi"])
                        .pick_file()
                {
                    self.import_media(path);
                }
                if ui.button("Nuovo Solid Color").clicked() {
                    self.add_solid_color_clip();
                }

                ui.separator();
                if ui.button("Elimina (Del)").clicked() {
                    self.delete_selected();
                }
                if ui
                    .button("Ripple delete (Shift+Del)")
                    .on_hover_text(
                        "Rimuove la clip e chiude il gap su tutte le track, mantenendo il sync A/V",
                    )
                    .clicked()
                {
                    self.ripple_delete_selected();
                }
                if ui
                    .button("Dividi (T)")
                    .on_hover_text("Taglia tutte le clip sotto al playhead, su ogni track")
                    .clicked()
                {
                    self.split_all_at_playhead();
                }
                ui.separator();
                if ui.button("Undo (Ctrl+Z)").clicked() {
                    self.history.undo(&mut self.project);
                }
                if ui.button("Redo (Ctrl+Shift+Z)").clicked() {
                    self.history.redo(&mut self.project);
                }

                ui.separator();
                let playing = self.preview_player.as_ref().is_some_and(Player::is_playing);
                let label = if playing { "⏸" } else { "▶" };
                if ui.button(format!("{label} (Spazio)")).clicked() {
                    self.toggle_playback();
                }
                if let Some(player) = &self.preview_player {
                    let duration = player.duration_secs();
                    let pos = player.position_secs();
                    ui.label(format!("{pos:.2}s / {duration:.2}s"));
                }
            });
        });

        // Frame a cui vengono lette/scritte le proprietà nel pannello:
        // sempre il playhead della timeline tradotto nello spazio frame
        // sorgente della clip *selezionata* (source_in + offset locale,
        // clampato dentro la clip) — indipendente da quale clip stia
        // effettivamente riproducendo il player, così modificare le
        // proprietà di una clip diversa da quella attiva resta coerente
        // con quello che si vede scorrendo la timeline fin lì.
        let source_frame = self
            .timeline_state
            .selected
            .and_then(|(track_index, clip_id)| {
                let timeline_id = self.timeline_id?;
                let clip = self.project.timelines[timeline_id]
                    .tracks
                    .get(track_index)?
                    .clips
                    .iter()
                    .find(|c| c.id == clip_id)?;
                let local = (self.timeline_state.playhead - clip.timeline_start)
                    .clamp(0, clip.timeline_len().saturating_sub(1));
                Some(clip.source_in + local)
            })
            .unwrap_or(0);

        let mut pool_action = None;
        let mut pending_effect = None;
        egui::Panel::left("media_pool")
            .default_size(260.0)
            .show(ui, |ui| {
                ui.heading("Media Pool");
                if let Some(err) = &self.import_error {
                    ui.colored_label(egui::Color32::RED, err);
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let items: Vec<(MediaId, String, vv_core::MediaMeta)> = self
                        .project
                        .media_pool
                        .iter()
                        .map(|(id, item)| (id, file_label(&item.path), item.meta.clone()))
                        .collect();
                    for (id, label, meta) in items {
                        ui.group(|ui| {
                            ui.label(&label);
                            ui.small(format!(
                                "{}x{} · {:.2}fps · {}",
                                meta.width,
                                meta.height,
                                meta.fps.as_f64(),
                                if meta.has_audio { "audio" } else { "muto" }
                            ));
                            ui.horizontal(|ui| {
                                if ui.button("Anteprima").clicked() {
                                    pool_action = Some(PoolAction::Preview(id));
                                }
                                if ui.button("Aggiungi").clicked() {
                                    pool_action = Some(PoolAction::AddToTimeline(id));
                                }
                            });
                        });
                    }
                });

                if let Some((track_index, clip_id)) = self.timeline_state.selected {
                    ui.separator();
                    ui.heading("Clip selezionata");

                    let clip_info = self.timeline_id.and_then(|timeline_id| {
                        self.project.timelines[timeline_id]
                            .tracks
                            .get(track_index)?
                            .clips
                            .iter()
                            .find(|c| c.id == clip_id)
                            .map(|c| ClipPanelInfo {
                                start: c.timeline_start,
                                len: c.timeline_len(),
                                is_solid_color: matches!(c.source, vv_core::ClipSource::SolidColor),
                                transform_constant: c.effects.transform.is_constant(),
                                transform_kf_here: c
                                    .effects
                                    .transform
                                    .keyframe_at(source_frame)
                                    .is_some(),
                                transform: c.effects.transform.value_at(source_frame),
                                gain_constant: c.effects.gain_db.is_constant(),
                                gain_kf_here: c.effects.gain_db.keyframe_at(source_frame).is_some(),
                                gain: c.effects.gain_db.value_at(source_frame),
                                color_constant: c
                                    .effects
                                    .color
                                    .as_ref()
                                    .is_none_or(|k| k.is_constant()),
                                color_kf_here: c
                                    .effects
                                    .color
                                    .as_ref()
                                    .and_then(|k| k.keyframe_at(source_frame))
                                    .is_some(),
                                color: c
                                    .effects
                                    .color
                                    .as_ref()
                                    .map(|k| k.value_at(source_frame))
                                    .unwrap_or(vv_core::Rgba {
                                        r: 0.6,
                                        g: 0.6,
                                        b: 0.6,
                                        a: 1.0,
                                    }),
                            })
                    });

                    if let Some(ClipPanelInfo {
                        start,
                        len,
                        is_solid_color,
                        transform_constant,
                        transform_kf_here,
                        mut transform,
                        gain_constant,
                        gain_kf_here,
                        mut gain,
                        color_constant,
                        color_kf_here,
                        mut color,
                    }) = clip_info
                    {
                        ui.label(format!("Track: {track_index}"));
                        ui.label(format!("Start: {start} frame"));
                        ui.label(format!("Durata: {len} frame"));
                        ui.label(format!("Frame corrente (source): {source_frame}"));

                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Crop / zoom / posizione");
                            if keyframe_button(ui, transform_constant, transform_kf_here).clicked()
                            {
                                pending_effect = Some(if transform_kf_here {
                                    PendingEffectChange::RemoveTransformKeyframe(
                                        track_index,
                                        clip_id,
                                        source_frame,
                                    )
                                } else {
                                    PendingEffectChange::UpsertTransformKeyframe(
                                        track_index,
                                        clip_id,
                                        source_frame,
                                        transform,
                                    )
                                });
                            }
                        });
                        if !transform_constant {
                            ui.small(format!(
                                "{} keyframe · animato",
                                self.timeline_id
                                    .and_then(|tid| self.project.timelines[tid].tracks[track_index]
                                        .clips
                                        .iter()
                                        .find(|c| c.id == clip_id))
                                    .map(|c| c.effects.transform.keyframes().len())
                                    .unwrap_or(0)
                            ));
                        }

                        let mut transform_changed = false;
                        transform_changed |= ui
                            .add(
                                egui::Slider::new(&mut transform.crop[0], 0.0..=0.99)
                                    .text("sinistra"),
                            )
                            .changed();
                        transform_changed |= ui
                            .add(egui::Slider::new(&mut transform.crop[1], 0.0..=0.99).text("alto"))
                            .changed();
                        transform_changed |= ui
                            .add(
                                egui::Slider::new(&mut transform.crop[2], 0.01..=1.0)
                                    .text("destra"),
                            )
                            .changed();
                        transform_changed |= ui
                            .add(
                                egui::Slider::new(&mut transform.crop[3], 0.01..=1.0).text("basso"),
                            )
                            .changed();
                        // Margine minimo per non invertire il rettangolo di crop.
                        transform.crop[0] = transform.crop[0].min(transform.crop[2] - 0.01);
                        transform.crop[1] = transform.crop[1].min(transform.crop[3] - 0.01);

                        transform_changed |= ui
                            .add(egui::Slider::new(&mut transform.zoom, 0.1..=5.0).text("zoom"))
                            .changed();
                        transform_changed |= ui
                            .add(
                                egui::Slider::new(&mut transform.position[0], -1.0..=1.0)
                                    .text("posizione X"),
                            )
                            .changed();
                        transform_changed |= ui
                            .add(
                                egui::Slider::new(&mut transform.position[1], -1.0..=1.0)
                                    .text("posizione Y"),
                            )
                            .changed();
                        if ui.button("Reset transform").clicked() {
                            transform = vv_core::Transform::default();
                            transform_changed = true;
                        }
                        if transform_changed {
                            pending_effect = Some(if transform_constant {
                                PendingEffectChange::SetTransformDefault(
                                    track_index,
                                    clip_id,
                                    transform,
                                )
                            } else {
                                PendingEffectChange::UpsertTransformKeyframe(
                                    track_index,
                                    clip_id,
                                    source_frame,
                                    transform,
                                )
                            });
                        }

                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Gain audio (dB)");
                            if keyframe_button(ui, gain_constant, gain_kf_here).clicked() {
                                pending_effect = Some(if gain_kf_here {
                                    PendingEffectChange::RemoveGainKeyframe(
                                        track_index,
                                        clip_id,
                                        source_frame,
                                    )
                                } else {
                                    PendingEffectChange::UpsertGainKeyframe(
                                        track_index,
                                        clip_id,
                                        source_frame,
                                        gain,
                                    )
                                });
                            }
                        });
                        if ui
                            .add(egui::Slider::new(&mut gain, -60.0..=12.0).text("dB"))
                            .changed()
                        {
                            pending_effect = Some(if gain_constant {
                                PendingEffectChange::SetGainDefault(track_index, clip_id, gain)
                            } else {
                                PendingEffectChange::UpsertGainKeyframe(
                                    track_index,
                                    clip_id,
                                    source_frame,
                                    gain,
                                )
                            });
                        }

                        if is_solid_color {
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label("Colore");
                                if keyframe_button(ui, color_constant, color_kf_here).clicked() {
                                    pending_effect = Some(if color_kf_here {
                                        PendingEffectChange::RemoveColorKeyframe(
                                            track_index,
                                            clip_id,
                                            source_frame,
                                        )
                                    } else {
                                        PendingEffectChange::UpsertColorKeyframe(
                                            track_index,
                                            clip_id,
                                            source_frame,
                                            color,
                                        )
                                    });
                                }
                            });
                            let mut rgba = [color.r, color.g, color.b, color.a];
                            if ui.color_edit_button_rgba_unmultiplied(&mut rgba).changed() {
                                color = vv_core::Rgba {
                                    r: rgba[0],
                                    g: rgba[1],
                                    b: rgba[2],
                                    a: rgba[3],
                                };
                                pending_effect = Some(if color_constant {
                                    PendingEffectChange::SetColorDefault(
                                        track_index,
                                        clip_id,
                                        color,
                                    )
                                } else {
                                    PendingEffectChange::UpsertColorKeyframe(
                                        track_index,
                                        clip_id,
                                        source_frame,
                                        color,
                                    )
                                });
                            }
                        }
                    }
                }
            });

        if let Some(action) = pool_action {
            match action {
                PoolAction::Preview(id) => {
                    self.preview_media(id);
                    // Anteprima "grezza" del media pool: non è (ancora)
                    // detto che sia sulla timeline, quindi non ha un
                    // transform/gain di clip da applicare, e il playhead
                    // non deve strapparcela via al frame successivo.
                    self.active_clip = None;
                    self.browsing_media = Some(id);
                }
                PoolAction::AddToTimeline(id) => self.add_media_to_timeline(id),
            }
        }
        if let (Some(timeline_id), Some(change)) = (self.timeline_id, pending_effect) {
            let cmd = build_effect_command(timeline_id, change);
            self.history.do_command(&mut self.project, cmd);
            self.apply_active_clip_gain();
        }

        let selected_before_timeline_ui = self.timeline_state.selected;
        let playhead_before_timeline_ui = self.timeline_state.playhead;
        egui::Panel::bottom("timeline")
            .default_size(240.0)
            .resizable(true)
            .show(ui, |ui| {
                ui.heading("Timeline");
                if let Some(timeline_id) = self.timeline_id {
                    let labels: HashMap<MediaId, String> = self
                        .project
                        .media_pool
                        .iter()
                        .map(|(id, item)| (id, file_label(&item.path)))
                        .collect();
                    timeline_ui::show_timeline(
                        ui,
                        &mut self.project,
                        &mut self.history,
                        timeline_id,
                        &|id| labels.get(&id).cloned().unwrap_or_default(),
                        &mut self.timeline_state,
                    );
                } else {
                    ui.label("Importa un media per creare la timeline.");
                }
            });
        // Interagire con la timeline (selezionare una clip o spostare il
        // playhead) riprende il controllo del viewer dall'anteprima
        // "grezza" del media pool, se attiva.
        if self.timeline_state.selected != selected_before_timeline_ui
            || self.timeline_state.playhead != playhead_before_timeline_ui
        {
            self.browsing_media = None;
        }

        if self.browsing_media.is_none() {
            self.ensure_active_clip_matches_playhead();
            self.drive_playback();
        }

        egui::CentralPanel::default().show(ui, |ui| {
            // Le clip SolidColor non hanno un media da decodificare: il
            // colore (eventualmente keyframeato) va valutato al frame
            // *locale alla clip* sul playhead della timeline, l'unico
            // orologio che ha senso per un generatore (un Player non ha
            // motivo di esistere per un riempimento uniforme).
            let solid_color_frame_info = self.active_clip.and_then(|(track_index, clip_id)| {
                let timeline_id = self.timeline_id?;
                let tl = &self.project.timelines[timeline_id];
                let clip = tl
                    .tracks
                    .get(track_index)?
                    .clips
                    .iter()
                    .find(|c| c.id == clip_id)?;
                match &clip.source {
                    vv_core::ClipSource::SolidColor => {
                        let local_frame =
                            (self.timeline_state.playhead - clip.timeline_start).max(0);
                        let rgba = clip
                            .effects
                            .color
                            .as_ref()
                            .map(|k| k.value_at(local_frame))
                            .unwrap_or(vv_core::Rgba {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            });
                        Some((tl.resolution, rgba))
                    }
                    vv_core::ClipSource::Media(_) => None,
                }
            });

            if let Some(((w, h), rgba)) = solid_color_frame_info {
                let data = vv_render::solid_color_frame(rgba, w, h);
                let image =
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &data);
                match &mut self.frame_texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.frame_texture = Some(ui.ctx().load_texture(
                            "current-frame",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
            } else if let Some(frame) = self.preview_player.as_ref().and_then(|p| p.current_frame())
            {
                let source_frame = self
                    .preview_player
                    .as_ref()
                    .map(|p| p.current_source_frame())
                    .unwrap_or(0);
                let transform = self
                    .active_clip_effects()
                    .map(|e| e.transform.value_at(source_frame))
                    .unwrap_or_default();
                let composited = self.compositor.render_frame(
                    &frame.data,
                    frame.width,
                    frame.height,
                    &transform,
                    frame.width,
                    frame.height,
                );
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &composited,
                );
                match &mut self.frame_texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.frame_texture = Some(ui.ctx().load_texture(
                            "current-frame",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
            }

            if let Some(texture) = &self.frame_texture {
                let available = ui.available_size();
                let tex_size = texture.size_vec2();
                let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                let display_size = tex_size * scale.max(0.0);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::from_texture(texture).fit_to_exact_size(display_size));
                });
            } else if let Some(err) = &self.preview_error {
                ui.colored_label(egui::Color32::RED, format!("Errore player: {err}"));
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(if self.preview_player.is_some() {
                        "Decodifica in corso..."
                    } else {
                        "Importa un media, aggiungilo alla timeline e premi Spazio."
                    });
                });
            }
        });

        if self.preview_player.as_ref().is_some_and(Player::is_playing) {
            ui.ctx().request_repaint();
        }
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Argomento opzionale: path di un video da importare subito all'avvio
    // (comodo per debug/smoke test, oltre che per l'uso da riga di comando).
    let startup_path = std::env::args().nth(1).map(PathBuf::from);

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "vibevideo",
        options,
        Box::new(move |_cc| {
            let mut app = VibeVideoApp::default();
            if let Some(path) = startup_path {
                app.import_media(path);
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_timeline_with_clip(
        app: &mut VibeVideoApp,
        track_index: usize,
        start: FrameIdx,
        len: FrameIdx,
    ) -> vv_core::ClipId {
        if app.timeline_id.is_none() {
            let id = app.project.timelines.insert(vv_core::Timeline {
                name: "T".into(),
                fps: vv_core::Rational::new(25, 1),
                resolution: (1920, 1080),
                tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
            });
            app.timeline_id = Some(id);
        }
        let clip_id = app.project.alloc_clip_id();
        let clip = vv_core::Clip {
            id: clip_id,
            source: vv_core::ClipSource::SolidColor,
            source_in: 0,
            source_out: len,
            timeline_start: start,
            effects: vv_core::EffectStack::default(),
            linked: None,
        };
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::InsertClip {
                timeline: app.timeline_id.unwrap(),
                track_index,
                clip,
            }),
        );
        clip_id
    }

    #[test]
    fn ripple_delete_selected_shifts_other_tracks_and_clears_selection() {
        let mut app = VibeVideoApp::default();
        let video_a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let video_b = make_timeline_with_clip(&mut app, 0, 10, 10);
        let audio_a = make_timeline_with_clip(&mut app, 1, 0, 10);
        let _audio_b = make_timeline_with_clip(&mut app, 1, 10, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = Some((0, video_b));
        app.ripple_delete_selected();

        assert_eq!(app.timeline_state.selected, None);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].id, video_a);
        // La clip audio che partiva allo stesso istante si è spostata a 0
        // anche se sta su un'altra track: comportamento ripple globale.
        assert_eq!(tl.tracks[1].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips[0].id, audio_a);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 0);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 0);
    }

    #[test]
    fn delete_selected_does_not_shift_other_tracks() {
        let mut app = VibeVideoApp::default();
        let video_a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let video_b = make_timeline_with_clip(&mut app, 0, 10, 10);
        make_timeline_with_clip(&mut app, 1, 0, 10);
        make_timeline_with_clip(&mut app, 1, 10, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = Some((0, video_b));
        app.delete_selected();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].id, video_a);
        assert_eq!(tl.tracks[1].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 10);
    }

    /// Esercita il collegamento completo introdotto in milestone 5:
    /// caricare una clip video apre il player sul suo media
    /// (`load_video_clip`) e il gain impostato via comando arriva davvero
    /// all'`AudioPlayer` sottostante, senza panic.
    #[test]
    fn loading_a_media_clip_opens_preview_and_applies_gain() {
        let dir = std::env::temp_dir().join("vv-app-main-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let timeline_id = app.timeline_id.expect("import doveva creare la timeline");
        let media_id = app
            .project
            .media_pool
            .iter()
            .next()
            .map(|(id, _)| id)
            .expect("media importato atteso nel pool");

        app.add_media_to_timeline(media_id);
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.timeline_state.selected = Some((0, clip_id));
        app.load_video_clip(clip_id);

        assert_eq!(app.active_clip, Some((0, clip_id)));
        assert!(app.preview_player.is_some(), "doveva aprirsi un player");
        assert_eq!(
            app.active_clip_effects().map(|e| e.gain_db.default),
            Some(0.0)
        );

        self_test_set_gain(&mut app, timeline_id, 0, clip_id, -12.0);
        assert_eq!(
            app.active_clip_effects().map(|e| e.gain_db.default),
            Some(-12.0)
        );
        // Non deve panicare anche se il player è ancora in fase di apertura.
        app.apply_active_clip_gain();
    }

    fn self_test_set_gain(
        app: &mut VibeVideoApp,
        timeline_id: TimelineId,
        track_index: usize,
        clip_id: vv_core::ClipId,
        db: f32,
    ) {
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::SetClipGain::new(
                timeline_id,
                track_index,
                clip_id,
                db,
            )),
        );
    }

    #[test]
    fn build_effect_command_upsert_gain_keyframe_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        let cmd = build_effect_command(
            timeline_id,
            PendingEffectChange::UpsertGainKeyframe(0, clip_id, 5, -9.0),
        );
        app.history.do_command(&mut app.project, cmd);

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(
            clip.effects.gain_db.keyframe_at(5),
            Some((-9.0, vv_core::Interpolation::Linear))
        );
    }

    #[test]
    fn build_effect_command_remove_transform_keyframe_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        let transform = vv_core::Transform {
            zoom: 2.5,
            ..vv_core::Transform::default()
        };
        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::UpsertTransformKeyframe(0, clip_id, 3, transform),
            ),
        );
        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::RemoveTransformKeyframe(0, clip_id, 3),
            ),
        );

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(clip.effects.transform.is_constant());
    }

    #[test]
    fn build_effect_command_set_defaults_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::SetGainDefault(0, clip_id, -3.0),
            ),
        );
        let transform = vv_core::Transform {
            zoom: 1.5,
            ..vv_core::Transform::default()
        };
        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::SetTransformDefault(0, clip_id, transform),
            ),
        );

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(clip.effects.gain_db.default, -3.0);
        assert_eq!(clip.effects.transform.default.zoom, 1.5);
    }

    #[test]
    fn add_solid_color_clip_creates_timeline_and_initialized_color() {
        let mut app = VibeVideoApp::default();
        assert!(app.timeline_id.is_none());

        app.add_solid_color_clip();

        let timeline_id = app.timeline_id.expect("doveva crearsi una timeline");
        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(matches!(clip.source, vv_core::ClipSource::SolidColor));
        assert!(clip.effects.color.is_some());
        assert_eq!(clip.timeline_len(), 125); // 5s a 25fps di default
    }

    #[test]
    fn loading_solid_color_clip_clears_preview_player_and_sets_active_clip() {
        let mut app = VibeVideoApp::default();
        app.add_solid_color_clip();
        let timeline_id = app.timeline_id.unwrap();
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.load_video_clip(clip_id);

        assert_eq!(app.active_clip, Some((0, clip_id)));
        assert!(app.preview_player.is_none());
    }

    #[test]
    fn build_effect_command_color_upsert_and_remove_round_trip() {
        let mut app = VibeVideoApp::default();
        app.add_solid_color_clip();
        let timeline_id = app.timeline_id.unwrap();
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        let red = vv_core::Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::UpsertColorKeyframe(0, clip_id, 10, red),
            ),
        );
        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(
            clip.effects
                .color
                .as_ref()
                .unwrap()
                .keyframe_at(10)
                .unwrap()
                .0
                .r,
            1.0
        );

        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::RemoveColorKeyframe(0, clip_id, 10),
            ),
        );
        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(clip.effects.color.as_ref().unwrap().is_constant());
    }

    /// Riproduce il bug segnalato: trascinare il playhead della timeline
    /// non spostava l'anteprima del player della clip selezionata.
    #[test]
    fn dragging_timeline_playhead_seeks_the_active_media_player() {
        let dir = std::env::temp_dir().join("vv-app-playhead-sync-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let timeline_id = app.timeline_id.unwrap();
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.timeline_state.selected = Some((0, clip_id));
        app.load_video_clip(clip_id);
        // Il player appena aperto non ha ancora un frame in cache: il primo
        // sync forza comunque il seek verso il playhead (0 qui, no-op).
        app.ensure_active_clip_matches_playhead();

        // L'utente trascina il playhead a metà clip (frame 25 su 50, clip
        // a 25fps/2s): il player deve seguirlo con un seek.
        app.timeline_state.playhead = 25;
        app.ensure_active_clip_matches_playhead();

        let player = app.preview_player.as_ref().expect("player atteso");
        assert_eq!(
            player.current_source_frame(),
            25,
            "il player doveva fare un seek verso il frame del playhead"
        );

        // Da fermo e senza ulteriori spostamenti, un altro sync non deve
        // ri-seekare (idempotenza): verifichiamo indirettamente che
        // last_synced_playhead sia stato aggiornato, riportando il
        // playhead a un valore diverso e controllando che segua di nuovo.
        app.timeline_state.playhead = 10;
        app.ensure_active_clip_matches_playhead();
        assert_eq!(
            app.preview_player.as_ref().unwrap().current_source_frame(),
            10
        );
    }

    /// Riproduce il bug segnalato: durante il playback, raggiunto un
    /// taglio (fine del trim della clip), il playhead si fermava lì e il
    /// riquadro continuava a mostrare la clip come se non fosse tagliata.
    /// Verifica invece che si avanzi automaticamente alla clip successiva
    /// sulla track video, restando in riproduzione.
    #[test]
    fn playback_auto_advances_to_next_clip_at_a_cut() {
        let dir = std::env::temp_dir().join("vv-app-auto-advance-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let timeline_id = app.timeline_id.unwrap();
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);
        let first_clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        // Taglia la clip (50 frame) a metà: due clip, [0,25) e [25,50).
        app.timeline_state.playhead = 25;
        app.split_all_at_playhead();
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(
            tl.tracks[0].clips.len(),
            2,
            "il taglio doveva creare 2 clip"
        );
        let second_clip_id = tl.tracks[0].clips[1].id;
        assert_ne!(first_clip_id, second_clip_id);

        // Riproduci dall'inizio della prima metà.
        app.timeline_state.playhead = 0;
        app.ensure_active_clip_matches_playhead();
        assert_eq!(app.active_clip, Some((0, first_clip_id)));
        app.toggle_playback();
        assert!(app.preview_player.as_ref().unwrap().is_playing());

        // Simula "il player ha raggiunto la fine del trim della prima
        // metà" (frame 25, cioè source_out) senza aspettare la decodifica
        // reale: è esattamente la condizione che drive_playback controlla.
        app.preview_player.as_mut().unwrap().seek_to_frame(25);
        app.drive_playback();

        assert_eq!(
            app.active_clip,
            Some((0, second_clip_id)),
            "doveva avanzare alla clip successiva al taglio"
        );
        assert_eq!(app.timeline_state.playhead, 25);
        assert!(
            app.preview_player.as_ref().unwrap().is_playing(),
            "la riproduzione doveva continuare, non fermarsi"
        );
    }

    /// Riproduce il bug segnalato: la riproduzione partiva solo se una
    /// clip era selezionata esplicitamente in timeline.
    #[test]
    fn toggle_playback_works_without_any_selection() {
        let dir = std::env::temp_dir().join("vv-app-play-without-selection-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);

        assert_eq!(app.timeline_state.selected, None);
        assert_eq!(app.active_clip, None);

        app.toggle_playback();

        assert!(
            app.active_clip.is_some(),
            "doveva agganciare la clip sotto al playhead"
        );
        assert!(
            app.preview_player.as_ref().is_some_and(Player::is_playing),
            "la riproduzione doveva partire senza alcuna selezione"
        );
    }

    /// Riproduce il bug segnalato: tagliare con T richiedeva di
    /// selezionare esplicitamente la track video, e ogni track andava
    /// tagliata separatamente. `split_all_at_playhead` non deve dipendere
    /// dalla selezione e deve tagliare tutte le track in un colpo solo.
    #[test]
    fn split_all_at_playhead_cuts_every_track_without_selection() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        assert_eq!(app.timeline_state.selected, None);
        app.timeline_state.playhead = 8;
        app.split_all_at_playhead();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 2, "track video tagliata");
        assert_eq!(
            tl.tracks[1].clips.len(),
            2,
            "track audio tagliata anche senza selezione"
        );
        assert_eq!(tl.tracks[0].clips[0].id, video_id);
        assert_eq!(tl.tracks[1].clips[0].id, audio_id);

        // Un solo undo annulla entrambi i tagli (CompositeCommand).
        app.history.undo(&mut app.project);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 1);
    }
}
