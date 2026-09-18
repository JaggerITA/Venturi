//! Selezione multipla nel media pool, con le stesse regole della timeline:
//! click semplice, ctrl+click per aggiungere/togliere, shift+click per un
//! range, rettangolo di selezione trascinando dallo sfondo. La logica di
//! selezione vive qui come funzione pura (testabile senza `egui::Ui`); il
//! disegno del pannello resta in `main.rs`.

use std::collections::BTreeSet;
use vv_core::MediaId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self { key: SortKey::Name, ascending: true }
    }
}

#[derive(Default)]
pub struct MediaPoolState {
    pub selected: BTreeSet<MediaId>,
    /// Origine del range per il prossimo shift+click (che *non* la sposta,
    /// come in `timeline_ui::TimelineState::selection_anchor`).
    anchor: Option<MediaId>,
    /// Rettangolo di selezione in corso, in coordinate schermo.
    pub marquee: Option<(egui::Pos2, egui::Pos2)>,
    /// Il media pool ha ricevuto l'ultimo click: Canc/Backspace cancella i
    /// media selezionati invece delle clip in timeline.
    pub focused: bool,
    pub sort: Sort,
}

impl MediaPoolState {
    /// `order` è l'ordine con cui gli elementi sono disegnati nel pannello:
    /// è quello che definisce il range di uno shift+click.
    pub fn click(&mut self, clicked: MediaId, modifiers: egui::Modifiers, order: &[MediaId]) {
        let (selected, anchor) = apply_click(&self.selected, self.anchor, clicked, modifiers, order);
        self.selected = selected;
        self.anchor = anchor;
    }

    /// `hits` in ordine di disegno: il primo diventa l'ancora per un
    /// eventuale shift+click successivo.
    pub fn set_marquee_selection(&mut self, hits: impl IntoIterator<Item = MediaId>) {
        let hits: Vec<MediaId> = hits.into_iter().collect();
        self.anchor = hits.first().copied();
        self.selected = hits.into_iter().collect();
    }

    /// Click sull'intestazione di una colonna: ordina per quel criterio,
    /// oppure inverte il verso se era già quello attivo.
    pub fn toggle_sort(&mut self, key: SortKey) {
        if self.sort.key == key {
            self.sort.ascending = !self.sort.ascending;
        } else {
            self.sort = Sort { key, ascending: true };
        }
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }
}

/// Ordina gli elementi del pannello secondo `sort`. A parità di durata
/// l'ordine resta quello per nome, così la lista non salta a ogni ridisegno.
pub fn sort_items<T>(
    items: &mut [T],
    sort: Sort,
    name: impl Fn(&T) -> &str,
    duration_secs: impl Fn(&T) -> f64,
) {
    items.sort_by(|a, b| {
        let by_name = natural_cmp(name(a), name(b));
        let ord = match sort.key {
            SortKey::Name => by_name,
            SortKey::Duration => duration_secs(a)
                .partial_cmp(&duration_secs(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(by_name),
        };
        if sort.ascending { ord } else { ord.reverse() }
    });
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

fn apply_click(
    current: &BTreeSet<MediaId>,
    anchor: Option<MediaId>,
    clicked: MediaId,
    modifiers: egui::Modifiers,
    order: &[MediaId],
) -> (BTreeSet<MediaId>, Option<MediaId>) {
    if modifiers.shift {
        let range = anchor
            .and_then(|a| {
                let from = order.iter().position(|id| *id == a)?;
                let to = order.iter().position(|id| *id == clicked)?;
                Some(order[from.min(to)..=from.max(to)].iter().copied().collect())
            })
            .unwrap_or_else(|| BTreeSet::from([clicked]));
        (range, anchor.or(Some(clicked)))
    } else if modifiers.command {
        let mut set = current.clone();
        if !set.remove(&clicked) {
            set.insert(clicked);
        }
        (set, Some(clicked))
    } else {
        (BTreeSet::from([clicked]), Some(clicked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MediaId` è una chiave slotmap, non costruibile a mano.
    fn ids(n: usize) -> Vec<MediaId> {
        let mut project = vv_core::Project::default();
        (0..n)
            .map(|_| {
                project.media_pool.insert(vv_core::MediaItem {
                    path: "/x".into(),
                    meta: vv_core::MediaMeta {
                        duration_frames: 0,
                        fps: vv_core::Rational::new(25, 1),
                        width: 0,
                        height: 0,
                        has_video: true,
                        has_audio: false,
                        sample_rate: 0,
                        channels: 0,
                    },
                    content_hash: 0,
                })
            })
            .collect()
    }

    fn plain() -> egui::Modifiers {
        egui::Modifiers::NONE
    }

    fn ctrl() -> egui::Modifiers {
        egui::Modifiers::COMMAND
    }

    fn shift() -> egui::Modifiers {
        egui::Modifiers::SHIFT
    }

    #[test]
    fn plain_click_replaces_selection() {
        let order = ids(3);
        let mut state = MediaPoolState::default();
        state.click(order[0], plain(), &order);
        state.click(order[2], plain(), &order);
        assert_eq!(state.selected, BTreeSet::from([order[2]]));
    }

    #[test]
    fn ctrl_click_toggles() {
        let order = ids(3);
        let mut state = MediaPoolState::default();
        state.click(order[0], plain(), &order);
        state.click(order[2], ctrl(), &order);
        assert_eq!(state.selected, BTreeSet::from([order[0], order[2]]));
        state.click(order[0], ctrl(), &order);
        assert_eq!(state.selected, BTreeSet::from([order[2]]));
    }

    #[test]
    fn shift_click_selects_range_and_keeps_the_anchor() {
        let order = ids(4);
        let mut state = MediaPoolState::default();
        state.click(order[1], plain(), &order);
        state.click(order[3], shift(), &order);
        assert_eq!(
            state.selected,
            BTreeSet::from([order[1], order[2], order[3]])
        );
        // L'ancora è rimasta la prima: un secondo shift+click parte ancora da lì.
        state.click(order[0], shift(), &order);
        assert_eq!(state.selected, BTreeSet::from([order[0], order[1]]));
    }

    fn names(items: &[(&str, f64)]) -> Vec<(String, f64)> {
        items.iter().map(|(n, d)| ((*n).to_string(), *d)).collect()
    }

    fn sorted(items: &[(&str, f64)], sort: Sort) -> Vec<String> {
        let mut rows = names(items);
        sort_items(&mut rows, sort, |r| r.0.as_str(), |r| r.1);
        rows.into_iter().map(|r| r.0).collect()
    }

    #[test]
    fn sorts_by_name_and_duration_in_both_directions() {
        let items = [("b.mp4", 5.0), ("a.mp4", 9.0), ("c.mp4", 1.0)];
        assert_eq!(
            sorted(&items, Sort { key: SortKey::Name, ascending: true }),
            ["a.mp4", "b.mp4", "c.mp4"]
        );
        assert_eq!(
            sorted(&items, Sort { key: SortKey::Name, ascending: false }),
            ["c.mp4", "b.mp4", "a.mp4"]
        );
        assert_eq!(
            sorted(&items, Sort { key: SortKey::Duration, ascending: true }),
            ["c.mp4", "b.mp4", "a.mp4"]
        );
        assert_eq!(
            sorted(&items, Sort { key: SortKey::Duration, ascending: false }),
            ["a.mp4", "b.mp4", "c.mp4"]
        );
    }

    #[test]
    fn equal_durations_keep_the_name_order() {
        let items = [("b.mp4", 5.0), ("a.mp4", 5.0)];
        assert_eq!(
            sorted(&items, Sort { key: SortKey::Duration, ascending: true }),
            ["a.mp4", "b.mp4"]
        );
    }

    #[test]
    fn clicking_the_same_column_inverts_the_order() {
        let mut state = MediaPoolState::default();
        assert_eq!(state.sort, Sort { key: SortKey::Name, ascending: true });
        state.toggle_sort(SortKey::Name);
        assert_eq!(state.sort, Sort { key: SortKey::Name, ascending: false });
        state.toggle_sort(SortKey::Duration);
        assert_eq!(state.sort, Sort { key: SortKey::Duration, ascending: true });
        state.toggle_sort(SortKey::Duration);
        assert_eq!(state.sort, Sort { key: SortKey::Duration, ascending: false });
        state.toggle_sort(SortKey::Name);
        assert_eq!(state.sort, Sort { key: SortKey::Name, ascending: true });
    }

    #[test]
    fn shift_click_without_anchor_selects_only_the_clicked_one() {
        let order = ids(3);
        let mut state = MediaPoolState::default();
        state.click(order[2], shift(), &order);
        assert_eq!(state.selected, BTreeSet::from([order[2]]));
    }
}
