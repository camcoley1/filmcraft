//! Lumetri node graph and grade version commands (M8d; model in `filmcraft_project::color_graph`).
//!
//! Like the other Lumetri commands they address a clip by `clip` (default: the first selected
//! video clip with Lumetri Color, else the first selected video clip, which then gets one).
//! Nodes and versions are 0-based indices. Every edit is one undo step; `color.copyGrade` only
//! fills the session's grade clipboard.

use filmcraft_project::color_graph as cg;
use filmcraft_project::{ClipId, EffectInstance, TrackKind, find_effect};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, bool_p, clip_p, clips_p, f64_p, has_seq, str_p};
use crate::{Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled: has_seq, run, journal: true }
}

const MENU: &[&str] = &["Clip", "Color Grade"];

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "color.nodes",
            label: "List Color Nodes",
            menu: &[],
            shortcut: None,
            params: r#"{"clip":id?}"#,
            enabled: always,
            run: list,
            journal: false,
        },
        spec("color.selectNode", "Select Color Node", &[], r#"{"clip":id?,"node":n}"#, |s, p| {
            node_edit(s, p, "color.selectNode", "Select Color Node", cg::select_node)
        }),
        spec("color.addNode", "Add Color Node", MENU, r#"{"clip":id?,"after":n?,"label":str?}"#, add_node),
        spec("color.removeNode", "Remove Color Node", &[], r#"{"clip":id?,"node":n}"#, |s, p| {
            node_edit(s, p, "color.removeNode", "Remove Color Node", cg::remove_node)
        }),
        spec("color.moveNode", "Move Color Node", &[], r#"{"clip":id?,"from":n,"to":n}"#, move_node),
        spec("color.setNodeMix", "Set Color Node Mix", &[], r#"{"clip":id?,"node":n,"mix":0..1}"#, set_mix),
        spec("color.bypassNode", "Bypass Color Node", &[], r#"{"clip":id?,"node":n,"bypass":bool?}"#, bypass),
        spec("color.setNodeLabel", "Rename Color Node", &[], r#"{"clip":id?,"node":n,"label":str}"#, set_label),
        spec("color.setNodeMask", "Set Color Node Mask", &[], r#"{"clip":id?,"node":n,"mask":str|""}"#, set_mask),
        spec("color.version.add", "New Grade Version", MENU, r#"{"clip":id?,"name":str?,"duplicate":bool=true}"#, version_add),
        spec("color.version.switch", "Switch Grade Version", &[], r#"{"clip":id?,"version":n|"name":str}"#, version_switch),
        spec("color.version.rename", "Rename Grade Version", &[], r#"{"clip":id?,"version":n?,"name":str}"#, version_rename),
        spec("color.version.delete", "Delete Grade Version", &[], r#"{"clip":id?,"version":n?}"#, version_delete),
        CommandSpec {
            id: "color.copyGrade",
            label: "Copy Grade",
            menu: MENU,
            shortcut: None,
            params: r#"{"clip":id?}"#,
            enabled: has_seq,
            run: copy_grade,
            journal: false,
        },
        spec("color.pasteGrade", "Paste Grade", MENU, r#"{"clips":[id]?}"#, paste_grade),
        spec("color.resetGrade", "Reset Grade", MENU, r#"{"clip":id?}"#, reset_grade),
    ]
}

/// The graph as JSON: nodes (label, on, mix, mask, which is selected), versions and the active one.
pub fn graph_json(clip: ClipId, e: &EffectInstance) -> Value {
    let selected = cg::graph(e).and_then(cg::ColorGraph::active_version).map_or(0, cg::GradeVersion::selected);
    let labels: Vec<String> = match cg::graph(e).and_then(cg::ColorGraph::active_version) {
        Some(v) => v.nodes.iter().map(|n| n.label.clone()).collect(),
        None => vec![String::new()],
    };
    let nodes: Vec<Value> = cg::active_nodes(e)
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let label = labels.get(i).filter(|l| !l.is_empty()).cloned().unwrap_or_else(|| format!("Node {}", i + 1));
            json!({"index": i, "label": label, "enabled": n.enabled, "mix": n.mix, "mask": n.mask, "selected": i == selected})
        })
        .collect();
    let (versions, active) = cg::versions(e);
    json!({"clip": clip.0, "nodes": nodes, "versions": versions, "active": active, "masks": e.masks.iter().map(|m| m.name.clone()).collect::<Vec<_>>()})
}

/// The clip's Lumetri, read-only (no Lumetri is added).
fn find_lumetri(s: &Session, p: &Value) -> Option<(ClipId, EffectInstance)> {
    let seq = s.active_sequence()?;
    let video = |c: &ClipId| seq.find_item(*c).is_some_and(|(t, _)| seq.track(t).is_some_and(|t| t.kind == TrackKind::Video));
    let clip = match clip_p(p, "clip") {
        Some(c) => c,
        None => {
            let sel: Vec<ClipId> = s.state.selection.iter().copied().filter(video).collect();
            sel.iter().copied().find(|c| seq.find_item(*c).is_some_and(|(_, it)| it.effects.iter().any(|e| e.effect == "lumetri"))).or(sel.first().copied())?
        }
    };
    let e = seq.find_item(clip)?.1.effects.iter().find(|e| e.effect == "lumetri")?.clone();
    Some((clip, e))
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    Ok(match find_lumetri(s, p) {
        Some((clip, e)) => graph_json(clip, &e),
        None => json!({"clip": null, "nodes": [], "versions": [], "active": 0, "masks": []}),
    })
}

/// Edit the addressed clip's Lumetri (one undo step) and return its graph.
fn edit(s: &mut Session, p: &Value, cmd: &'static str, label: &str, f: impl FnOnce(&mut EffectInstance) -> std::result::Result<(), String>) -> Result<Value> {
    let (clip, idx) = crate::color::lumetri_clip(s, p, cmd)?;
    s.edit_sequence(label, |q, _, _| {
        let (_, it) = q.find_item_mut(clip).ok_or(filmcraft_edit::EditError::NoItem(clip))?;
        let e = it.effects.get_mut(idx).ok_or_else(|| bad(cmd, "no Lumetri"))?;
        f(e).map_err(|m| bad(cmd, m))
    })?;
    let e = s.active_sequence().and_then(|q| q.find_item(clip)).and_then(|(_, it)| it.effects.get(idx).cloned()).ok_or_else(|| bad(cmd, "no Lumetri"))?;
    Ok(graph_json(clip, &e))
}

fn index_p(p: &Value, k: &str, cmd: &'static str) -> Result<usize> {
    let v = p.get(k).ok_or_else(|| bad(cmd, format!("need `{k}`")))?;
    v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad(cmd, format!("`{k}` must be a node / version index (0, 1, ...)")))
}

fn node_edit(
    s: &mut Session,
    p: &Value,
    cmd: &'static str,
    label: &str,
    f: fn(&mut EffectInstance, usize) -> std::result::Result<(), String>,
) -> Result<Value> {
    let i = index_p(p, "node", cmd)?;
    edit(s, p, cmd, label, |e| f(e, i))
}

fn add_node(s: &mut Session, p: &Value) -> Result<Value> {
    let after = match p.get("after") {
        Some(_) => Some(index_p(p, "after", "color.addNode")?),
        None => None,
    };
    let label = str_p(p, "label").unwrap_or("").to_string();
    edit(s, p, "color.addNode", "Add Color Node", |e| cg::add_node(e, after, &label).map(|_| ()))
}

fn move_node(s: &mut Session, p: &Value) -> Result<Value> {
    let (from, to) = (index_p(p, "from", "color.moveNode")?, index_p(p, "to", "color.moveNode")?);
    edit(s, p, "color.moveNode", "Move Color Node", |e| cg::move_node(e, from, to))
}

fn set_mix(s: &mut Session, p: &Value) -> Result<Value> {
    let i = index_p(p, "node", "color.setNodeMix")?;
    let mix = f64_p(p, "mix").ok_or_else(|| bad("color.setNodeMix", "need `mix` (0..1)"))?;
    edit(s, p, "color.setNodeMix", "Set Color Node Mix", |e| cg::set_mix(e, i, mix))
}

fn bypass(s: &mut Session, p: &Value) -> Result<Value> {
    let i = index_p(p, "node", "color.bypassNode")?;
    let bypass = bool_p(p, "bypass");
    edit(s, p, "color.bypassNode", "Bypass Color Node", |e| {
        let on = match bypass {
            Some(b) => !b,
            // toggle
            None => !cg::active_nodes(e).get(i).is_some_and(|n| n.enabled),
        };
        cg::set_enabled(e, i, on)
    })
}

fn set_label(s: &mut Session, p: &Value) -> Result<Value> {
    let i = index_p(p, "node", "color.setNodeLabel")?;
    let label = str_p(p, "label").ok_or_else(|| bad("color.setNodeLabel", "need `label`"))?.to_string();
    edit(s, p, "color.setNodeLabel", "Rename Color Node", |e| cg::set_label(e, i, &label))
}

fn set_mask(s: &mut Session, p: &Value) -> Result<Value> {
    let i = index_p(p, "node", "color.setNodeMask")?;
    let mask = str_p(p, "mask").filter(|m| !m.is_empty()).map(str::to_string);
    edit(s, p, "color.setNodeMask", "Set Color Node Mask", |e| cg::set_mask(e, i, mask.as_deref()))
}

fn version_add(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_p(p, "name").map(str::to_string);
    let duplicate = bool_p(p, "duplicate").unwrap_or(true);
    edit(s, p, "color.version.add", "New Grade Version", |e| cg::add_version(e, name.as_deref(), duplicate).map(|_| ()))
}

/// The version `p` names (`version` index or `name`), or the active one.
fn version_p(s: &Session, p: &Value, cmd: &'static str) -> Result<Option<usize>> {
    if p.get("version").is_some() {
        return index_p(p, "version", cmd).map(Some);
    }
    let Some(name) = str_p(p, "name").filter(|_| cmd == "color.version.switch") else { return Ok(None) };
    let (names, _) = find_lumetri(s, p).map(|(_, e)| cg::versions(&e)).unwrap_or_default();
    names.iter().position(|n| n == name).map(Some).ok_or_else(|| bad(cmd, format!("no grade version `{name}`")))
}

fn version_switch(s: &mut Session, p: &Value) -> Result<Value> {
    let v = version_p(s, p, "color.version.switch")?.ok_or_else(|| bad("color.version.switch", "need `version` or `name`"))?;
    edit(s, p, "color.version.switch", "Switch Grade Version", |e| cg::switch_version(e, v))
}

fn version_rename(s: &mut Session, p: &Value) -> Result<Value> {
    let v = version_p(s, p, "color.version.rename")?;
    let name = str_p(p, "name").ok_or_else(|| bad("color.version.rename", "need `name`"))?.to_string();
    edit(s, p, "color.version.rename", "Rename Grade Version", |e| {
        let v = v.unwrap_or_else(|| cg::versions(e).1);
        cg::rename_version(e, v, &name)
    })
}

fn version_delete(s: &mut Session, p: &Value) -> Result<Value> {
    let v = version_p(s, p, "color.version.delete")?;
    edit(s, p, "color.version.delete", "Delete Grade Version", |e| {
        let v = v.unwrap_or_else(|| cg::versions(e).1);
        cg::delete_version(e, v)
    })
}

fn copy_grade(s: &mut Session, p: &Value) -> Result<Value> {
    let (clip, e) = find_lumetri(s, p).ok_or_else(|| bad("color.copyGrade", "select a video clip with Lumetri Color"))?;
    let grade = cg::active_grade(&e);
    let n = grade.nodes.len();
    s.state.grade_clipboard = Some(grade);
    Ok(json!({"clip": clip.0, "nodes": n}))
}

fn paste_grade(s: &mut Session, p: &Value) -> Result<Value> {
    let grade = s.state.grade_clipboard.clone().ok_or_else(|| bad("color.pasteGrade", "no grade copied (Copy Grade first)"))?;
    let seq = s.active_sequence().ok_or_else(|| bad("color.pasteGrade", "no active sequence"))?;
    let clips: Vec<ClipId> =
        clips_p(s, p).into_iter().filter(|c| seq.find_item(*c).is_some_and(|(t, _)| seq.track(t).is_some_and(|t| t.kind == TrackKind::Video))).collect();
    if clips.is_empty() {
        return Err(bad("color.pasteGrade", "select video clips"));
    }
    let lumetri = find_effect("lumetri").map(|d| d.instance()).ok_or_else(|| bad("color.pasteGrade", "no Lumetri Color effect"))?;
    let targets = clips.clone();
    s.edit_sequence("Paste Grade", |q, _, _| {
        for c in &targets {
            let (_, it) = q.find_item_mut(*c).ok_or(filmcraft_edit::EditError::NoItem(*c))?;
            if !it.effects.iter().any(|e| e.effect == "lumetri") {
                it.effects.push(lumetri.clone());
            }
            if let Some(e) = it.effects.iter_mut().find(|e| e.effect == "lumetri") {
                cg::paste_grade(e, &grade).map_err(|m| bad("color.pasteGrade", m))?;
            }
        }
        Ok(())
    })?;
    Ok(json!({"clips": clips.iter().map(|c| c.0).collect::<Vec<_>>(), "nodes": grade.nodes.len()}))
}

fn reset_grade(s: &mut Session, p: &Value) -> Result<Value> {
    edit(s, p, "color.resetGrade", "Reset Grade", |e| {
        cg::reset_grade(e);
        Ok(())
    })
}
