//! Project panel ▸ search by content (M6e).
//!
//! A query that starts with `?` is not a name filter: Enter runs `ai.search` (the helper's CLIP
//! index and the transcripts), and the panel shows the hits instead of the item list: the item,
//! the media time, what matched (picture or words) and how well. A click selects the item, a
//! double click opens it in the Source monitor at that time. Without the helper (or without the
//! `sidecar` build) the panel says why and the plain name filter is untouched.

use egui::{Align2, Rect, Sense, pos2, vec2};
use filmcraft_project::ItemId;
use serde_json::json;

use crate::FilmcraftApp;
use crate::panels::project::Actions;
use crate::theme::Tokens;

/// The query asks for a search by content.
pub fn is_ai(query: &str) -> bool {
    query.starts_with('?')
}

/// The query without its `?` and spaces.
pub fn question(query: &str) -> &str {
    query.strip_prefix('?').unwrap_or(query).trim()
}

/// The text the item filters use: the query lower-cased, nothing for a search by content.
pub fn name_filter(query: &str) -> String {
    if is_ai(query) { String::new() } else { query.to_ascii_lowercase() }
}

/// `1:05.5` for 65.5 seconds.
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0);
    format!("{}:{:04.1}", (s / 60.0) as u64, s % 60.0)
}

/// The hits of the last search in the content area.
pub fn results(app: &mut FilmcraftApp, ui: &mut egui::Ui, content: Rect, pre: &str, fs: f32, actions: &mut Actions) {
    let t = app.tokens;
    let q = question(&app.ui.project_search).to_string();
    let find = filmcraft_engine::find_command("ai.search");
    let note = |ui: &egui::Ui, y: f32, text: &str| {
        ui.painter().text(pos2(content.min.x + 10.0, y), Align2::LEFT_CENTER, text, Tokens::ui(fs), t.text_dim);
    };
    let Some(cmd) = find else {
        note(ui, content.min.y + 16.0, tl!("Search by content needs the AI helper build."));
        return;
    };
    if let Err(why) = (cmd.enabled)(&app.session) {
        note(ui, content.min.y + 16.0, &why);
        return;
    }
    let res = app.session.search_results.clone();
    if q.is_empty() || res.query != q {
        note(ui, content.min.y + 16.0, tl!("Press Enter to search by content"));
        return;
    }
    let mut y = content.min.y + 4.0;
    if res.pending {
        note(ui, y + 10.0, tl!("Searching…"));
        y += 22.0;
    } else if let Some(n) = &res.note {
        note(ui, y + 10.0, n);
        y += 22.0;
    }
    if res.hits.is_empty() && !res.pending {
        note(ui, y + 10.0, tl!("No matches"));
        return;
    }
    // the hits scroll: a long list is reachable by wheel and scroll bar
    let area = Rect::from_min_max(pos2(content.min.x, y), content.max);
    if area.height() < 22.0 {
        return;
    }
    let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(area).id_salt((pre, "aiHits")));
    bui.set_clip_rect(area.intersect(ui.clip_rect()));
    egui::ScrollArea::vertical().id_salt((pre, "ai-scroll")).auto_shrink([false, false]).show(&mut bui, |ui| {
        for (n, h) in res.hits.iter().enumerate() {
            let (row, resp) = ui.allocate_exact_size(vec2((area.width() - 12.0).max(40.0), 22.0), Sense::click());
            if !ui.is_rect_visible(row) {
                continue;
            }
            let name = app.session.project.item(h.item).map(|i| i.name.clone()).unwrap_or_default();
            let selected = app.session.state.project_selection.contains(&h.item);
            if selected {
                ui.painter().rect_filled(row, 3.0, t.row_selected);
            } else if resp.hovered() {
                ui.painter().rect_filled(row, 3.0, t.hover);
            }
            let secs = h.time.seconds();
            let what = if h.kind == "transcript" { tl!("Words").to_string() } else { format!("{} {:.2}", tl!("Picture"), h.score) };
            let stale = if h.stale { format!(" ({})", tl!("changed since indexed")) } else { String::new() };
            ui.painter().text(pos2(row.min.x + 8.0, row.center().y), Align2::LEFT_CENTER, &name, Tokens::ui(fs), t.text);
            let detail = format!("{}  {}{}", clock(secs), what, stale);
            ui.painter().text(pos2(row.max.x - 8.0, row.center().y), Align2::RIGHT_CENTER, &detail, Tokens::ui(fs - 0.5), t.text_dim);
            app.auto.add(&format!("{pre}.aiHit.{n}"), row, &format!("{name} {}", clock(secs)));
            if resp.clicked() {
                actions.push(("project.select".into(), json!({"items": [h.item.0]})));
            }
            if resp.double_clicked() {
                open_at(h.item, h.time.0, actions);
            }
        }
    });
}

/// Open the item in the Source monitor with the playhead on the hit.
fn open_at(item: ItemId, time: i64, actions: &mut Actions) {
    actions.push(("source.open".into(), json!({"item": item.0})));
    actions.push(("source.setPlayhead".into(), json!({"time": time})));
}

/// Enter in the search field: run the search by content.
pub fn run(query: &str, actions: &mut Actions) {
    let q = question(query);
    if !q.is_empty() {
        actions.push(("ai.search".into(), json!({"query": q})));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_mark_makes_a_search_by_content_and_leaves_name_filters_alone() {
        assert!(is_ai("?ocean") && is_ai("? two words") && !is_ai("ocean") && !is_ai(" ?x"));
        assert_eq!(question("?  red car "), "red car");
        assert_eq!(question("plain"), "plain");
        assert_eq!(name_filter("?ocean"), "");
        assert_eq!(name_filter("MoV"), "mov");
    }

    #[test]
    fn clocks_read_minutes_and_seconds() {
        assert_eq!(clock(65.5), "1:05.5");
        assert_eq!(clock(0.0), "0:00.0");
        assert_eq!(clock(-3.0), "0:00.0");
    }
}
