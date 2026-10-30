//! Node-based colour (M8d): a Lumetri Color effect can hold a graph of serial nodes, each its own
//! Lumetri parameter block with an on/off switch, a mix (0 = the node's input, 1 = its full
//! result) and optionally one of the effect's masks limiting it, in one or more named grade
//! versions (A, B, ...) of which one is active.
//!
//! Storage: the effect instance's own `params` are always the parameters of the **selected** node
//! of the **active** version, so every Lumetri control, command, keyframe and MCP path edits that
//! node exactly as before; the graph keeps every other node's parameters. Without a graph
//! (`EffectInstance::color` = `None`: every project before schema v18) the effect is one node with
//! mix 1, which is the single Lumetri it always was.
//!
//! A mask a node names limits that node only, so it no longer limits the whole effect
//! ([`effect_wide_masks`]).
//!
//! Every index read from a file is hostile: accessors clamp `active` / `selected`, a graph with no
//! version or no node reads as no graph, and the node and version counts are capped.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::effect::{EffectInstance, find_effect};
use crate::keyframe::Param;
use crate::mask::Mask;

/// Most nodes one grade version holds.
pub const MAX_NODES: usize = 32;
/// Most grade versions one effect holds (A … Z).
pub const MAX_VERSIONS: usize = 26;

fn yes() -> bool {
    true
}
fn one() -> f64 {
    1.0
}

/// One serial node: a Lumetri parameter block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorNode {
    #[serde(default)]
    pub label: String,
    /// Off: the node passes its input through (bypass).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 0..1: how much of the node's result replaces its input.
    #[serde(default = "one")]
    pub mix: f64,
    /// The name of one of the effect's masks: the node applies inside it only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<String>,
    /// The node's parameters. Empty for the selected node of the active version, whose parameters
    /// are the effect instance's own.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Param>,
}

impl ColorNode {
    /// A node with Lumetri's default parameters.
    pub fn new(label: impl Into<String>) -> Self {
        ColorNode { label: label.into(), enabled: true, mix: 1.0, mask: None, params: default_params() }
    }

    /// Whether the node changes nothing about how its parameters render (on, full mix, unmasked).
    pub fn is_plain(&self) -> bool {
        self.enabled && self.mix >= 1.0 && self.mask.is_none()
    }
}

/// A named grade: its nodes in order, and which of them the Lumetri controls edit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradeVersion {
    pub name: String,
    pub nodes: Vec<ColorNode>,
    #[serde(default)]
    pub selected: usize,
}

/// The grade versions of a Lumetri Color effect and the active one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorGraph {
    pub versions: Vec<GradeVersion>,
    #[serde(default)]
    pub active: usize,
}

/// One node as the renderer reads it (the selected node's parameters are the effect's own).
#[derive(Clone, Copy, Debug)]
pub struct NodeRef<'a> {
    pub enabled: bool,
    pub mix: f64,
    pub mask: Option<&'a str>,
    pub params: &'a BTreeMap<String, Param>,
}

/// Lumetri's default parameters.
pub fn default_params() -> BTreeMap<String, Param> {
    find_effect("lumetri").map(|d| d.instance().params).unwrap_or_default()
}

/// The name a new grade version gets: the first unused letter.
fn next_version_name(versions: &[GradeVersion]) -> String {
    ('A'..='Z').map(String::from).find(|n| !versions.iter().any(|v| &v.name == n)).unwrap_or_else(|| format!("{}", versions.len() + 1))
}

impl ColorGraph {
    /// The graph an effect without one is: version A with one node, selected.
    fn single() -> Self {
        ColorGraph {
            versions: vec![GradeVersion { name: "A".into(), nodes: vec![ColorNode { params: BTreeMap::new(), ..ColorNode::new("") }], selected: 0 }],
            active: 0,
        }
    }

    /// The active version's index (clamped).
    pub fn active(&self) -> usize {
        self.active.min(self.versions.len().saturating_sub(1))
    }

    /// The active version (None only for a graph with no version, which reads as no graph).
    pub fn active_version(&self) -> Option<&GradeVersion> {
        self.versions.get(self.active())
    }

    fn usable(&self) -> bool {
        self.active_version().is_some_and(|v| !v.nodes.is_empty())
    }
}

impl GradeVersion {
    /// The selected node's index (clamped).
    pub fn selected(&self) -> usize {
        self.selected.min(self.nodes.len().saturating_sub(1))
    }
}

/// The effect's graph, if it has a usable one.
pub fn graph(e: &EffectInstance) -> Option<&ColorGraph> {
    e.color.as_deref().filter(|g| g.usable())
}

/// The nodes of the active version in order, as the renderer evaluates them. Without a graph:
/// the effect itself, one node with mix 1.
pub fn active_nodes(e: &EffectInstance) -> Vec<NodeRef<'_>> {
    let Some(v) = graph(e).and_then(ColorGraph::active_version) else {
        return vec![NodeRef { enabled: true, mix: 1.0, mask: None, params: &e.params }];
    };
    let sel = v.selected();
    v.nodes
        .iter()
        .enumerate()
        .map(|(i, n)| NodeRef { enabled: n.enabled, mix: n.mix, mask: n.mask.as_deref(), params: if i == sel { &e.params } else { &n.params } })
        .collect()
}

/// A node as a plain Lumetri Color instance (its parameters, no masks, no graph): what the renderer
/// evaluates for it.
pub fn node_instance(n: &NodeRef<'_>) -> EffectInstance {
    let mut e = find_effect("lumetri").map(|d| d.instance()).unwrap_or_else(|| EffectInstance {
        effect: "lumetri".into(),
        enabled: true,
        params: BTreeMap::new(),
        masks: Vec::new(),
        post_fader: false,
        essential: false,
        layer: None,
        color: None,
    });
    e.params = n.params.clone();
    e
}

/// Whether the effect renders as the single Lumetri of its own parameters (no graph, or a graph
/// whose active version is one plain node).
pub fn is_single(e: &EffectInstance) -> bool {
    match graph(e).and_then(ColorGraph::active_version) {
        None => true,
        Some(v) => v.nodes.len() == 1 && v.nodes.first().is_some_and(ColorNode::is_plain),
    }
}

/// The effect's masks that limit the whole effect: all of them except those a node of the active
/// version names (those limit their node).
pub fn effect_wide_masks(e: &EffectInstance) -> Vec<Mask> {
    let named: Vec<&str> = active_nodes(e).iter().filter_map(|n| n.mask).collect();
    e.masks.iter().filter(|m| !named.contains(&m.name.as_str())).cloned().collect()
}

/// The effect's graph for editing, created (as the single node it already is) when it has none.
fn graph_mut(e: &mut EffectInstance) -> &mut ColorGraph {
    if !e.color.as_deref().is_some_and(ColorGraph::usable) {
        e.color = Some(Box::new(ColorGraph::single()));
    }
    // (just made usable above)
    e.color.get_or_insert_with(|| Box::new(ColorGraph::single()))
}

/// Normalise indices after a change so the stored ones are in range.
fn tidy(g: &mut ColorGraph) {
    g.active = g.active();
    for v in &mut g.versions {
        v.selected = v.selected();
    }
}

/// The active version, with the selected node's parameters written into it (what a copy, a
/// duplicate or another version switch needs).
pub fn active_grade(e: &EffectInstance) -> GradeVersion {
    let mut v = graph(e).and_then(ColorGraph::active_version).cloned().unwrap_or_else(|| ColorGraph::single().versions.remove(0));
    let sel = v.selected();
    if let Some(n) = v.nodes.get_mut(sel) {
        n.params = e.params.clone();
    }
    v
}

/// Put the selected node's parameters back into its slot (before the selection or version changes).
fn stash(e: &mut EffectInstance) {
    let params = std::mem::take(&mut e.params);
    let g = graph_mut(e);
    let a = g.active();
    if let Some(v) = g.versions.get_mut(a) {
        let sel = v.selected();
        if let Some(n) = v.nodes.get_mut(sel) {
            n.params = params;
        }
    }
}

/// Load the selected node of the active version into the effect's parameters.
fn load(e: &mut EffectInstance) {
    let g = graph_mut(e);
    let a = g.active();
    let params = g.versions.get_mut(a).and_then(|v| {
        let sel = v.selected();
        v.nodes.get_mut(sel).map(|n| std::mem::take(&mut n.params))
    });
    e.params = params.filter(|p| !p.is_empty()).unwrap_or_else(default_params);
}

fn node_index(e: &EffectInstance, i: usize) -> Result<usize, String> {
    let n = active_nodes(e).len();
    if i < n { Ok(i) } else { Err(format!("no node {} (the grade has {n})", i + 1)) }
}

/// Select node `i` of the active version for editing.
pub fn select_node(e: &mut EffectInstance, i: usize) -> Result<(), String> {
    let i = node_index(e, i)?;
    stash(e);
    let g = graph_mut(e);
    let a = g.active();
    if let Some(v) = g.versions.get_mut(a) {
        v.selected = i;
    }
    load(e);
    Ok(())
}

/// Add a node with Lumetri's defaults after `after` (default: the selected node) and select it.
/// Returns its index.
pub fn add_node(e: &mut EffectInstance, after: Option<usize>, label: &str) -> Result<usize, String> {
    let n = active_nodes(e).len();
    if n >= MAX_NODES {
        return Err(format!("a grade has at most {MAX_NODES} nodes"));
    }
    let sel = graph(e).and_then(ColorGraph::active_version).map_or(0, GradeVersion::selected);
    let at = after.map_or(sel, |a| a.min(n.saturating_sub(1))) + 1;
    stash(e);
    let g = graph_mut(e);
    let a = g.active();
    let v = g.versions.get_mut(a).ok_or("no grade version")?;
    let label = if label.is_empty() { format!("Node {}", v.nodes.len() + 1) } else { label.to_string() };
    v.nodes.insert(at.min(v.nodes.len()), ColorNode::new(label));
    v.selected = at.min(v.nodes.len() - 1);
    load(e);
    Ok(at)
}

/// Remove node `i` (a grade keeps at least one node).
pub fn remove_node(e: &mut EffectInstance, i: usize) -> Result<(), String> {
    let i = node_index(e, i)?;
    if active_nodes(e).len() <= 1 {
        return Err("a grade keeps at least one node".into());
    }
    stash(e);
    let g = graph_mut(e);
    let a = g.active();
    let v = g.versions.get_mut(a).ok_or("no grade version")?;
    v.nodes.remove(i);
    if v.selected > i || v.selected >= v.nodes.len() {
        v.selected = v.selected.saturating_sub(1);
    }
    tidy(g);
    load(e);
    Ok(())
}

/// Move node `from` to position `to` (the selection follows the node it was on).
pub fn move_node(e: &mut EffectInstance, from: usize, to: usize) -> Result<(), String> {
    let from = node_index(e, from)?;
    let to = node_index(e, to)?;
    stash(e);
    let g = graph_mut(e);
    let a = g.active();
    let v = g.versions.get_mut(a).ok_or("no grade version")?;
    let selected_node = v.selected();
    let node = v.nodes.remove(from);
    v.nodes.insert(to, node);
    // where the previously selected node is now
    let mut order: Vec<usize> = (0..v.nodes.len()).collect();
    let moved = order.remove(from);
    order.insert(to, moved);
    v.selected = order.iter().position(|o| *o == selected_node).unwrap_or(0);
    load(e);
    Ok(())
}

fn node_mut(e: &mut EffectInstance, i: usize) -> Result<&mut ColorNode, String> {
    let i = node_index(e, i)?;
    let g = graph_mut(e);
    let a = g.active();
    g.versions.get_mut(a).and_then(|v| v.nodes.get_mut(i)).ok_or_else(|| "no such node".into())
}

/// Set node `i`'s mix (clamped to 0..1; not a number is refused).
pub fn set_mix(e: &mut EffectInstance, i: usize, mix: f64) -> Result<(), String> {
    if !mix.is_finite() {
        return Err("mix must be a number".into());
    }
    node_mut(e, i)?.mix = mix.clamp(0.0, 1.0);
    Ok(())
}

/// Turn node `i` on or off (off = bypassed).
pub fn set_enabled(e: &mut EffectInstance, i: usize, on: bool) -> Result<(), String> {
    node_mut(e, i)?.enabled = on;
    Ok(())
}

pub fn set_label(e: &mut EffectInstance, i: usize, label: &str) -> Result<(), String> {
    node_mut(e, i)?.label = label.chars().take(64).collect();
    Ok(())
}

/// Limit node `i` to the effect mask called `mask` (None: unlimited).
pub fn set_mask(e: &mut EffectInstance, i: usize, mask: Option<&str>) -> Result<(), String> {
    if let Some(m) = mask
        && !e.masks.iter().any(|x| x.name == m)
    {
        return Err(format!("the effect has no mask `{m}`"));
    }
    node_mut(e, i)?.mask = mask.map(str::to_string);
    Ok(())
}

/// The grade versions' names, and the active one's index.
pub fn versions(e: &EffectInstance) -> (Vec<String>, usize) {
    match graph(e) {
        Some(g) => (g.versions.iter().map(|v| v.name.clone()).collect(), g.active()),
        None => (vec!["A".into()], 0),
    }
}

/// Add a grade version (a copy of the active one with `duplicate`, else one default node) and
/// switch to it. Returns its index.
pub fn add_version(e: &mut EffectInstance, name: Option<&str>, duplicate: bool) -> Result<usize, String> {
    if versions(e).0.len() >= MAX_VERSIONS {
        return Err(format!("an effect has at most {MAX_VERSIONS} grade versions"));
    }
    let mut v = if duplicate { active_grade(e) } else { GradeVersion { name: String::new(), nodes: vec![ColorNode::new("Node 1")], selected: 0 } };
    stash(e);
    let g = graph_mut(e);
    v.name = name.map(|n| n.chars().take(32).collect()).filter(|n: &String| !n.trim().is_empty()).unwrap_or_else(|| next_version_name(&g.versions));
    g.versions.push(v);
    g.active = g.versions.len() - 1;
    let i = g.active;
    load(e);
    Ok(i)
}

/// Switch to grade version `i`.
pub fn switch_version(e: &mut EffectInstance, i: usize) -> Result<(), String> {
    let n = versions(e).0.len();
    if i >= n {
        return Err(format!("no grade version {} (the effect has {n})", i + 1));
    }
    stash(e);
    graph_mut(e).active = i;
    load(e);
    Ok(())
}

pub fn rename_version(e: &mut EffectInstance, i: usize, name: &str) -> Result<(), String> {
    let name: String = name.trim().chars().take(32).collect();
    if name.is_empty() {
        return Err("a grade version needs a name".into());
    }
    let n = versions(e).0.len();
    let g = graph_mut(e);
    let v = g.versions.get_mut(i).ok_or_else(|| format!("no grade version {} (the effect has {n})", i + 1))?;
    v.name = name;
    Ok(())
}

/// Delete grade version `i` (an effect keeps at least one).
pub fn delete_version(e: &mut EffectInstance, i: usize) -> Result<(), String> {
    let n = versions(e).0.len();
    if i >= n {
        return Err(format!("no grade version {} (the effect has {n})", i + 1));
    }
    if n <= 1 {
        return Err("an effect keeps at least one grade version".into());
    }
    stash(e);
    let g = graph_mut(e);
    g.versions.remove(i);
    if g.active > i || g.active >= g.versions.len() {
        g.active = g.active.saturating_sub(1);
    }
    tidy(g);
    load(e);
    Ok(())
}

/// Replace the active version's nodes with `grade`'s (a pasted grade; its name is kept).
pub fn paste_grade(e: &mut EffectInstance, grade: &GradeVersion) -> Result<(), String> {
    if grade.nodes.is_empty() || grade.nodes.len() > MAX_NODES {
        return Err(format!("a grade has 1 to {MAX_NODES} nodes"));
    }
    stash(e);
    let g = graph_mut(e);
    let a = g.active();
    let v = g.versions.get_mut(a).ok_or("no grade version")?;
    v.nodes = grade.nodes.clone();
    // masks are the effect's own: a pasted node naming one this effect lacks is unlimited
    let names: Vec<String> = e.masks.iter().map(|m| m.name.clone()).collect();
    let g = graph_mut(e);
    let a = g.active();
    if let Some(v) = g.versions.get_mut(a) {
        for n in &mut v.nodes {
            if n.mask.as_ref().is_some_and(|m| !names.contains(m)) {
                n.mask = None;
            }
        }
        v.selected = grade.selected();
    }
    load(e);
    Ok(())
}

/// Reset the active version to one node with Lumetri's defaults.
pub fn reset_grade(e: &mut EffectInstance) {
    let fresh = GradeVersion { name: String::new(), nodes: vec![ColorNode::new("Node 1")], selected: 0 };
    let _ = paste_grade(e, &fresh);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyframe::ParamValue;

    fn lumetri() -> EffectInstance {
        find_effect("lumetri").unwrap().instance()
    }
    fn exposure(e: &EffectInstance) -> f64 {
        e.param("exposure").and_then(|p| p.value.as_f64()).unwrap_or(f64::NAN)
    }
    fn set_exposure(e: &mut EffectInstance, v: f64) {
        e.param_mut("exposure").unwrap().value = ParamValue::Float(v);
    }

    #[test]
    fn an_effect_without_a_graph_is_one_plain_node() {
        let e = lumetri();
        assert!(e.color.is_none() && is_single(&e));
        let nodes = active_nodes(&e);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].enabled && nodes[0].mix == 1.0 && nodes[0].mask.is_none());
        assert!(std::ptr::eq(nodes[0].params, &e.params));
        assert_eq!(versions(&e), (vec!["A".to_string()], 0));
    }

    #[test]
    fn nodes_keep_their_own_parameters_through_selection() {
        let mut e = lumetri();
        set_exposure(&mut e, 0.5);
        assert_eq!(add_node(&mut e, None, "").unwrap(), 1);
        // the new node is selected and has defaults
        assert_eq!(exposure(&e), 0.0);
        set_exposure(&mut e, -1.0);
        let nodes = active_nodes(&e);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].params.get("exposure").unwrap().value.as_f64(), Some(0.5));
        assert_eq!(nodes[1].params.get("exposure").unwrap().value.as_f64(), Some(-1.0));
        select_node(&mut e, 0).unwrap();
        assert_eq!(exposure(&e), 0.5);
        select_node(&mut e, 1).unwrap();
        assert_eq!(exposure(&e), -1.0);
        assert!(!is_single(&e));
        assert!(select_node(&mut e, 2).is_err());
    }

    #[test]
    fn moving_removing_and_mixing_nodes() {
        let mut e = lumetri();
        set_exposure(&mut e, 1.0);
        add_node(&mut e, None, "b").unwrap();
        set_exposure(&mut e, 2.0);
        add_node(&mut e, None, "c").unwrap();
        set_exposure(&mut e, 3.0);
        // order 1, 2, 3; selected the third
        move_node(&mut e, 2, 0).unwrap();
        let ex: Vec<f64> = active_nodes(&e).iter().map(|n| n.params.get("exposure").unwrap().value.as_f64().unwrap()).collect();
        assert_eq!(ex, vec![3.0, 1.0, 2.0]);
        assert_eq!(exposure(&e), 3.0, "the selection follows the moved node");
        set_mix(&mut e, 1, 0.25).unwrap();
        set_enabled(&mut e, 2, false).unwrap();
        assert!(set_mix(&mut e, 1, f64::NAN).is_err());
        set_mix(&mut e, 0, 7.0).unwrap();
        let nodes = active_nodes(&e);
        assert_eq!((nodes[0].mix, nodes[1].mix, nodes[2].enabled), (1.0, 0.25, false));
        remove_node(&mut e, 0).unwrap();
        assert_eq!(active_nodes(&e).len(), 2);
        assert_eq!(exposure(&e), 1.0, "removing the selected node selects a neighbour");
        remove_node(&mut e, 0).unwrap();
        assert!(remove_node(&mut e, 0).is_err(), "the last node stays");
    }

    #[test]
    fn versions_switch_duplicate_rename_delete() {
        let mut e = lumetri();
        set_exposure(&mut e, 0.5);
        assert_eq!(add_version(&mut e, None, true).unwrap(), 1);
        assert_eq!(versions(&e), (vec!["A".to_string(), "B".to_string()], 1));
        assert_eq!(exposure(&e), 0.5, "a duplicate starts as a copy");
        set_exposure(&mut e, -0.5);
        switch_version(&mut e, 0).unwrap();
        assert_eq!(exposure(&e), 0.5);
        switch_version(&mut e, 1).unwrap();
        assert_eq!(exposure(&e), -0.5);
        add_version(&mut e, Some("Warm"), false).unwrap();
        assert_eq!(exposure(&e), 0.0, "a new version is one default node");
        rename_version(&mut e, 0, "Day").unwrap();
        assert!(rename_version(&mut e, 0, "  ").is_err());
        assert_eq!(versions(&e).0, vec!["Day", "B", "Warm"]);
        delete_version(&mut e, 2).unwrap();
        assert_eq!(versions(&e), (vec!["Day".to_string(), "B".to_string()], 1));
        assert_eq!(exposure(&e), -0.5);
        delete_version(&mut e, 0).unwrap();
        assert!(delete_version(&mut e, 0).is_err(), "the last version stays");
        assert!(switch_version(&mut e, 3).is_err());
    }

    #[test]
    fn copy_paste_and_reset_a_grade() {
        let mut a = lumetri();
        set_exposure(&mut a, 1.5);
        add_node(&mut a, None, "two").unwrap();
        set_exposure(&mut a, -0.5);
        set_mix(&mut a, 1, 0.5).unwrap();
        let grade = active_grade(&a);
        assert_eq!(grade.nodes.len(), 2);
        let mut b = lumetri();
        paste_grade(&mut b, &grade).unwrap();
        let (na, nb) = (active_nodes(&a), active_nodes(&b));
        assert_eq!(na.len(), nb.len());
        for (x, y) in na.iter().zip(&nb) {
            assert_eq!((x.params, x.mix, x.enabled), (y.params, y.mix, y.enabled));
        }
        reset_grade(&mut b);
        assert!(is_single(&b));
        assert_eq!(exposure(&b), 0.0);
    }

    #[test]
    fn node_masks_name_effect_masks_and_leave_the_effect_wide_ones() {
        let mut e = lumetri();
        e.masks = vec![Mask::new("face", crate::mask::MaskPath::rect(0.0, 0.0, 10.0, 10.0)), Mask::new("sky", crate::mask::MaskPath::rect(0.0, 0.0, 5.0, 5.0))];
        assert_eq!(effect_wide_masks(&e).len(), 2);
        add_node(&mut e, None, "").unwrap();
        assert!(set_mask(&mut e, 1, Some("nope")).is_err());
        set_mask(&mut e, 1, Some("face")).unwrap();
        let wide = effect_wide_masks(&e);
        assert_eq!(wide.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["sky"]);
        assert_eq!(active_nodes(&e)[1].mask, Some("face"));
        // pasting onto an effect without that mask leaves the node unlimited
        let mut other = lumetri();
        paste_grade(&mut other, &active_grade(&e)).unwrap();
        assert!(active_nodes(&other).iter().all(|n| n.mask.is_none()));
    }

    #[test]
    fn hostile_graphs_read_safely() {
        let mut e = lumetri();
        // no versions, or a version without nodes: no graph
        e.color = Some(Box::new(ColorGraph { versions: vec![], active: 9 }));
        assert!(is_single(&e) && active_nodes(&e).len() == 1);
        e.color = Some(Box::new(ColorGraph { versions: vec![GradeVersion { name: "A".into(), nodes: vec![], selected: 4 }], active: 0 }));
        assert!(is_single(&e) && active_nodes(&e).len() == 1);
        // indices out of range clamp
        let n = ColorNode::new("x");
        e.color = Some(Box::new(ColorGraph { versions: vec![GradeVersion { name: "A".into(), nodes: vec![n.clone(), n], selected: 99 }], active: 7 }));
        assert_eq!(active_nodes(&e).len(), 2);
        select_node(&mut e, 0).unwrap();
        // hostile JSON
        for json in [r#"{"versions":[{"name":"A","nodes":[{"mix":"x"}]}]}"#, r#"{"versions":5}"#] {
            assert!(serde_json::from_str::<ColorGraph>(json).is_err());
        }
        let g: ColorGraph = serde_json::from_str(r#"{"versions":[{"name":"A","nodes":[{}]}]}"#).unwrap();
        assert!(g.versions[0].nodes[0].enabled && g.versions[0].nodes[0].mix == 1.0);
        // caps
        let mut e = lumetri();
        for _ in 1..MAX_NODES {
            add_node(&mut e, None, "").unwrap();
        }
        assert!(add_node(&mut e, None, "").is_err());
        let mut e = lumetri();
        for _ in 1..MAX_VERSIONS {
            add_version(&mut e, None, false).unwrap();
        }
        assert!(add_version(&mut e, None, false).is_err());
    }
}
