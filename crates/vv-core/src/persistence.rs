//! Salvataggio/caricamento del progetto (milestone 10): serializzazione
//! RON di `Project`. Ogni tipo del modello deriva già `Serialize`/
//! `Deserialize` (vedi `model.rs`), quindi non c'è nulla da adattare lì:
//! qui solo lettura/scrittura su file e la (de)serializzazione stessa,
//! senza altra logica di progetto. `SlotMap` (usata per `media_pool` e
//! `timelines`) round-trippa le chiavi esattamente (stessa versione/indice
//! interni), quindi `MediaId`/`TimelineId` restano validi dopo un
//! caricamento — non serve rimappare nulla lato chiamante.

use crate::model::Project;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("errore di I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("errore RON: {0}")]
    Ron(#[from] ron::Error),
    #[error("errore RON: {0}")]
    RonParse(#[from] ron::error::SpannedError),
}

pub fn save_project(project: &Project, path: &Path) -> Result<(), PersistenceError> {
    let contents = ron::ser::to_string_pretty(project, ron::ser::PrettyConfig::default())?;
    std::fs::write(path, contents)?;
    Ok(())
}

pub fn load_project(path: &Path) -> Result<Project, PersistenceError> {
    let contents = std::fs::read_to_string(path)?;
    let mut project: Project = ron::from_str(&contents)?;
    // `Clip::rate` è derivato dagli fps: ricalcolarlo qui sistema i
    // progetti salvati prima che il campo esistesse (clip a fps diverso
    // da quello della timeline, fuori sync).
    project.refresh_clip_rates();
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    #[test]
    fn save_then_load_round_trips_a_project_with_clips_and_keyframes() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(Timeline {
            name: "Timeline 1".into(),
            fps: Rational::new(30, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        let media_id = project.media_pool.insert(MediaItem {
            path: "/tmp/example.mp4".into(),
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(30, 1),
                width: 1920,
                height: 1080,
                has_video: true,
                has_audio: true,
                sample_rate: 48000,
                channels: 2,
            },
            content_hash: 42,
        });

        let clip_id = project.alloc_clip_id();
        let mut effects = EffectStack::default();
        effects.gain_db.upsert(10, -6.0, Interpolation::Linear);
        let mut clip = Clip::from_source_range(
            clip_id,
            ClipSource::Media(media_id),
            0,
            50,
            0,
            Rational::one(),
        );
        clip.effects = effects;
        project.timelines[timeline_id].tracks[0].clips.push(clip);

        let dir = std::env::temp_dir().join("vv-core-persistence-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("project.vvproj");

        save_project(&project, &path).expect("save fallito");
        let loaded = load_project(&path).expect("load fallito");

        assert_eq!(loaded.timelines.len(), 1);
        let loaded_timeline = &loaded.timelines[timeline_id];
        assert_eq!(loaded_timeline.name, "Timeline 1");
        assert_eq!(loaded_timeline.fps, Rational::new(30, 1));
        assert_eq!(loaded_timeline.resolution, (1920, 1080));

        let loaded_clip = &loaded_timeline.tracks[0].clips[0];
        assert_eq!(loaded_clip.id, clip_id);
        assert_eq!(loaded_clip.source_out(), 50);
        assert_eq!(
            loaded_clip.effects.gain_db.keyframe_at(10),
            Some((-6.0, Interpolation::Linear))
        );

        let loaded_media = loaded.media_pool.get(media_id).unwrap();
        assert_eq!(
            loaded_media.path,
            std::path::PathBuf::from("/tmp/example.mp4")
        );
        assert_eq!(loaded_media.meta.sample_rate, 48000);
    }

    #[test]
    fn load_project_from_malformed_ron_returns_an_error() {
        let dir = std::env::temp_dir().join("vv-core-persistence-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.vvproj");
        std::fs::write(&path, "questo non è RON valido {{{").unwrap();

        assert!(load_project(&path).is_err());
    }

    #[test]
    fn load_project_from_missing_file_returns_an_error() {
        let dir = std::env::temp_dir().join("vv-core-persistence-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("does-not-exist.vvproj");
        let _ = std::fs::remove_file(&path);

        assert!(load_project(&path).is_err());
    }
}
