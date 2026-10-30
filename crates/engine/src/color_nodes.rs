//! Lumetri node graph and grade version commands (M8d; model in `filmcraft_project::color_graph`).
//!
//! Like the other Lumetri commands they address a clip by `clip` (default: the first selected
//! video clip with Lumetri Color, else the first selected video clip). `color.addNode`,
//! `color.version.add` and `color.pasteGrade` give a clip without Lumetri Color one, in the same
//! undo step; the others need it there already and change nothing when they fail.
//! Nodes and versions are 0-based indices; a version can also be named (`version: "B"`).
//! Every edit is one undo step (consecutive node selections of a clip, and consecutive Mix
//! changes of one node, merge into one); `color.copyGrade` only fills the session's grade
//! clipboard. Node Mix is a plain value, not keyframable.

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
        spec("color.version.switch", "Switch Grade Version", &[], r#"{"clip":id?,"version":n|str,"name":str?}"#, version_switch),
        spec("color.version.rename", "Rename Grade Version", &[], r#"{"clip":id?,"version":n|str?,"name":"new name"}"#, version_rename),
        spec("color.version.delete", "Delete Grade Version", &[], r#"{"clip":id?,"version":n|str?,"name":str?}"#, version_delete),
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

/// The addressed video clip and the index of its Lumetri Color (None: it has none yet). Read-only.
fn target(s: &Session, p: &Value, cmd: &'static str) -> Result<(ClipId, Option<usize>)> {
    let seq = s.active_sequence().ok_or_else(|| bad(cmd, "no active sequence"))?;
    let video = |c: &ClipId| seq.find_item(*c).is_some_and(|(t, _)| seq.track(t).is_some_and(|t| t.kind == TrackKind::Video));
    let lumetri = |c: ClipId| seq.find_item(c).and_then(|(_, it)| it.effects.iter().position(|e| e.effect == "lumetri"));
    let clip = match clip_p(p, "clip") {
        Some(c) if video(&c) => c,
        Some(_) => return Err(bad(cmd, "not a video clip")),
        None => {
            let sel: Vec<ClipId> = s.state.selection.iter().copied().filter(video).collect();
            sel.iter().copied().find(|c| lumetri(*c).is_some()).or(sel.first().copied()).ok_or_else(|| bad(cmd, "select a video clip"))?
        }
    };
    Ok((clip, lumetri(clip)))
}

/// The clip's Lumetri, read-only (no Lumetri is added).
fn find_lumetri(s: &Session, p: &Value) -> Option<(ClipId, EffectInstance)> {
    let (clip, idx) = target(s, p, "color.nodes").ok()?;
    let e = s.active_sequence()?.find_item(clip)?.1.effects.get(idx?)?.clone();
    Some((clip, e))
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    Ok(match find_lumetri(s, p) {
        Some((clip, e)) => graph_json(clip, &e),
        None => json!({"clip": null, "nodes": [], "versions": [], "active": 0, "masks": []}),
    })
}

/// A new Lumetri Color instance.
fn new_lumetri(cmd: &'static str) -> Result<EffectInstance> {
    find_effect("lumetri").map(|d| d.instance()).ok_or_else(|| bad(cmd, "no Lumetri Color effect"))
}

/// The item's Lumetri Color, inserted (where `effects.apply` puts a video effect: before the
/// intrinsic ones) when it has none. Returns its index.
fn lumetri_in(it: &mut filmcraft_project::TrackItem, fresh: &EffectInstance) -> usize {
    if let Some(i) = it.effects.iter().position(|e| e.effect == "lumetri") {
        return i;
    }
    let pos = it.effects.iter().position(|e| e.def().is_some_and(|d| d.intrinsic)).unwrap_or(it.effects.len());
    it.effects.insert(pos, fresh.clone());
    pos
}

/// How an edit is recorded.
struct Step<'a> {
    cmd: &'static str,
    label: &'a str,
    /// Shares one undo step with the previous edit of the same key.
    merge: Option<String>,
    /// Give a clip without Lumetri Color one (in the same step).
    create: bool,
}

/// Edit the addressed clip's Lumetri as one undo step (creating it in that step for `create`;
/// nothing changes when `f` fails) and return its graph.
fn edit(s: &mut Session, p: &Value, step: Step<'_>, f: impl FnOnce(&mut EffectInstance) -> std::result::Result<(), String>) -> Result<Value> {
    let Step { cmd, label, merge, create } = step;
    let (clip, idx) = target(s, p, cmd)?;
    if idx.is_none() && !create {
        return Err(bad(cmd, "the clip has no Lumetri Color"));
    }
    let fresh = new_lumetri(cmd)?;
    s.edit_sequence_as(label, merge.as_deref(), |q, _, _| {
        let (_, it) = q.find_item_mut(clip).ok_or(filmcraft_edit::EditError::NoItem(clip))?;
        let i = lumetri_in(it, &fresh);
        let e = it.effects.get_mut(i).ok_or_else(|| bad(cmd, "no Lumetri"))?;
        f(e).map_err(|m| bad(cmd, m))
    })?;
    let e = s
        .active_sequence()
        .and_then(|q| q.find_item(clip))
        .and_then(|(_, it)| it.effects.iter().find(|e| e.effect == "lumetri").cloned())
        .ok_or_else(|| bad(cmd, "no Lumetri"))?;
    Ok(graph_json(clip, &e))
}

/// A plain step: one undo step of its own, on an existing Lumetri.
fn step<'a>(cmd: &'static str, label: &'a str) -> Step<'a> {
    Step { cmd, label, merge: None, create: false }
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
    let mut st = step(cmd, label);
    if cmd == "color.selectNode" {
        // clicking through the nodes is one undo step
        st.merge = Some(format!("color.selectNode:{}", target(s, p, cmd)?.0.0));
    }
    edit(s, p, st, |e| f(e, i))
}

fn add_node(s: &mut Session, p: &Value) -> Result<Value> {
    let after = match p.get("after") {
        Some(_) => Some(index_p(p, "after", "color.addNode")?),
        None => None,
    };
    let label = str_p(p, "label").unwrap_or("").to_string();
    edit(s, p, Step { create: true, ..step("color.addNode", "Add Color Node") }, |e| cg::add_node(e, after, &label).map(|_| ()))
}

fn move_node(s: &mut Session, p: &Value) -> Result<Value> {
    let (from, to) = (index_p(p, "from", "color.moveNode")?, index_p(p, "to", "color.moveNode")?);
    edit(s, p, step("color.moveNode", "Move Color Node"), |e| cg::move_node(e, from, to))
}

fn set_mix(s: &mut Session, p: &Value) -> Result<Value> {
    let i = index_p(p, "node", "color.setNodeMix")?;
    let mix = f64_p(p, "mix").ok_or_else(|| bad("color.setNodeMix", "need `mix` (0..1)"))?;
    // a drag of the Mix field is one undo step
    let merge = Some(format!("color.setNodeMix:{}:{i}", target(s, p, "color.setNodeMix")?.0.0));
    edit(s, p, Step { merge, ..step("color.setNodeMix", "Set Color Node Mix") }, |e| cg::set_mix(e, i, mix))
}

fn bypass(s: &mut Session, p: &Value) -> Result<Value> {
    let i = index_p(p, "node", "color.bypassNode")?;
    let bypass = bool_p(p, "bypass");
    edit(s, p, step("color.bypassNode", "Bypass Color Node"), |e| {
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
    edit(s, p, step("color.setNodeLabel", "Rename Color Node"), |e| cg::set_label(e, i, &label))
}

fn set_mask(s: &mut Session, p: &Value) -> Result<Value> {
    let i = index_p(p, "node", "color.setNodeMask")?;
    let mask = str_p(p, "mask").filter(|m| !m.is_empty()).map(str::to_string);
    edit(s, p, step("color.setNodeMask", "Set Color Node Mask"), |e| cg::set_mask(e, i, mask.as_deref()))
}

fn version_add(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_p(p, "name").map(str::to_string);
    let duplicate = bool_p(p, "duplicate").unwrap_or(true);
    edit(s, p, Step { create: true, ..step("color.version.add", "New Grade Version") }, |e| cg::add_version(e, name.as_deref(), duplicate).map(|_| ()))
}

/// The version `p` addresses: `version` (an index, or a name), else (`name_selects`) `name`;
/// None: the active one.
fn version_p(s: &Session, p: &Value, cmd: &'static str, name_selects: bool) -> Result<Option<usize>> {
    let by_name = |name: &str| -> Result<Option<usize>> {
        let (names, _) = find_lumetri(s, p).map(|(_, e)| cg::versions(&e)).unwrap_or_default();
        names.iter().position(|n| n == name).map(Some).ok_or_else(|| bad(cmd, format!("no grade version `{name}`")))
    };
    match p.get("version") {
        Some(Value::String(name)) => by_name(name),
        Some(_) => index_p(p, "version", cmd).map(Some),
        None => match str_p(p, "name").filter(|_| name_selects) {
            Some(name) => by_name(name),
            None => Ok(None),
        },
    }
}

fn version_switch(s: &mut Session, p: &Value) -> Result<Value> {
    let v = version_p(s, p, "color.version.switch", true)?.ok_or_else(|| bad("color.version.switch", "need `version` (index or name)"))?;
    edit(s, p, step("color.version.switch", "Switch Grade Version"), |e| cg::switch_version(e, v))
}

/// `version` picks the version (default: the active one); `name` is its new name.
fn version_rename(s: &mut Session, p: &Value) -> Result<Value> {
    let v = version_p(s, p, "color.version.rename", false)?;
    let name = str_p(p, "name").ok_or_else(|| bad("color.version.rename", "need `name` (the new name)"))?.to_string();
    edit(s, p, step("color.version.rename", "Rename Grade Version"), |e| {
        let v = v.unwrap_or_else(|| cg::versions(e).1);
        cg::rename_version(e, v, &name)
    })
}

fn version_delete(s: &mut Session, p: &Value) -> Result<Value> {
    let v = version_p(s, p, "color.version.delete", true)?;
    edit(s, p, step("color.version.delete", "Delete Grade Version"), |e| {
        let v = v.unwrap_or_else(|| cg::versions(e).1);
        cg::delete_version(e, v)
    })
}

fn copy_grade(s: &mut Session, p: &Value) -> Result<Value> {
    let (clip, e) = find_lumetri(s, p).ok_or_else(|| bad("color.copyGrade", "select a video clip with Lumetri Color"))?;
    let grade = cg::active_grade(&e);
    let n = grade.nodes.len();
    // keyframes are in the clip's media time: keep where it starts, to re-time them on paste
    let from = s.active_sequence().and_then(|q| q.find_item(clip)).map_or(filmcraft_time::Tick::ZERO, |(_, it)| it.source_in);
    s.state.grade_clipboard = Some((grade, from));
    Ok(json!({"clip": clip.0, "nodes": n}))
}

fn paste_grade(s: &mut Session, p: &Value) -> Result<Value> {
    let (grade, from) = s.state.grade_clipboard.clone().ok_or_else(|| bad("color.pasteGrade", "no grade copied (Copy Grade first)"))?;
    let seq = s.active_sequence().ok_or_else(|| bad("color.pasteGrade", "no active sequence"))?;
    let clips: Vec<ClipId> =
        clips_p(s, p).into_iter().filter(|c| seq.find_item(*c).is_some_and(|(t, _)| seq.track(t).is_some_and(|t| t.kind == TrackKind::Video))).collect();
    if clips.is_empty() {
        return Err(bad("color.pasteGrade", "select video clips"));
    }
    let lumetri = new_lumetri("color.pasteGrade")?;
    let targets = clips.clone();
    s.edit_sequence("Paste Grade", |q, _, _| {
        for c in &targets {
            let (_, it) = q.find_item_mut(*c).ok_or(filmcraft_edit::EditError::NoItem(*c))?;
            // the grade's keyframes move from the copied clip's media time to this clip's
            let mut grade = grade.clone();
            cg::shift_keyframes(&mut grade, it.source_in.0.saturating_sub(from.0));
            let i = lumetri_in(it, &lumetri);
            let e = it.effects.get_mut(i).ok_or_else(|| bad("color.pasteGrade", "no Lumetri"))?;
            cg::paste_grade(e, &grade).map_err(|m| bad("color.pasteGrade", m))?;
        }
        Ok(())
    })?;
    Ok(json!({"clips": clips.iter().map(|c| c.0).collect::<Vec<_>>(), "nodes": grade.nodes.len()}))
}

fn reset_grade(s: &mut Session, p: &Value) -> Result<Value> {
    edit(s, p, step("color.resetGrade", "Reset Grade"), |e| {
        cg::reset_grade(e);
        Ok(())
    })
}
