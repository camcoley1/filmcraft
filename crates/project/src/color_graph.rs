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
//! Every graph read from a file is hostile. A file with more than [`MAX_NODES`] nodes in a version
//! or more than [`MAX_VERSIONS`] versions is refused when it is read (a grade is never silently cut
//! short); within the caps it is repaired without touching valid parts: indices are clamped, a
//! version without nodes gets one default node, mixes are clamped to 0..1 and labels and names
//! are shortened ([`repair`]). A graph with no version reads as no graph.
//!
//! Errors name nodes and versions by their 0-based index, as the commands take them.
//!
//! A graph that is just the single Lumetri again (one version "A" holding one plain, unlabelled
//! node) is dropped after every edit, so such a project still saves at the schema it had before.

use std::collections::BTreeMap;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
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

/// Longest node label kept.
const MAX_LABEL: usize = 64;
/// Longest grade version name kept.
const MAX_NAME: usize = 32;

/// A named grade: its nodes in order, and which of them the Lumetri controls edit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradeVersion {
    pub name: String,
    #[serde(deserialize_with = "capped_nodes")]
    pub nodes: Vec<ColorNode>,
    #[serde(default)]
    pub selected: usize,
}

/// The grade versions of a Lumetri Color effect and the active one. Reading one checks the caps
/// and [`repair`]s it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ColorGraph {
    pub versions: Vec<GradeVersion>,
    pub active: usize,
}

impl<'de> Deserialize<'de> for ColorGraph {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(deserialize_with = "capped_versions")]
            versions: Vec<GradeVersion>,
            #[serde(default)]
            active: usize,
        }
        let r = Raw::deserialize(d)?;
        let mut g = ColorGraph { versions: r.versions, active: r.active };
        repair(&mut g);
        Ok(g)
    }
}

/// A list of at most `max` items; a longer one is an error (read no further than the cap).
fn capped<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D, max: usize, what: &'static str) -> Result<Vec<T>, D::Error> {
    struct V<T> {
        max: usize,
        what: &'static str,
        t: std::marker::PhantomData<T>,
    }
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a list of at most {} {}", self.max, self.what)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::new();
            while let Some(x) = seq.next_element()? {
                if out.len() >= self.max {
                    return Err(de::Error::custom(format!("a Lumetri grade has more than {} {} (FilmCraft reads at most {})", self.max, self.what, self.max)));
                }
                out.push(x);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(V { max, what, t: std::marker::PhantomData })
}

fn capped_nodes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ColorNode>, D::Error> {
    capped(d, MAX_NODES, "nodes in a version")
}

fn capped_versions<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<GradeVersion>, D::Error> {
    capped(d, MAX_VERSIONS, "grade versions")
}

/// Make a graph (within the caps) consistent without touching its valid parts: clamp `active` and
/// every `selected`, give a version without nodes one node (the active version's takes the
/// effect's own parameters, as its selected node; another one Lumetri's defaults), clamp mixes to
/// 0..1 (not a number: 1) and shorten labels and names. Unnamed versions get a free letter.
pub fn repair(g: &mut ColorGraph) {
    g.active = g.active();
    let active = g.active;
    for (i, v) in g.versions.iter_mut().enumerate() {
        if v.nodes.is_empty() {
            let params = if i == active { BTreeMap::new() } else { default_params() };
            v.nodes.push(ColorNode { params, ..ColorNode::new("") });
        }
        v.selected = v.selected();
        v.name = v.name.trim().chars().take(MAX_NAME).collect();
        for n in &mut v.nodes {
            n.mix = if n.mix.is_finite() { n.mix.clamp(0.0, 1.0) } else { 1.0 };
            if n.label.chars().count() > MAX_LABEL {
                n.label = n.label.chars().take(MAX_LABEL).collect();
            }
        }
    }
    for i in 0..g.versions.len() {
        if g.versions.get(i).is_some_and(|v| v.name.is_empty()) {
            let name = next_version_name(&g.versions);
            if let Some(v) = g.versions.get_mut(i) {
                v.name = name;
            }
        }
    }
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
    ('A'..='Z').map(String::from).find(|n| !versions.iter().any(|v| &v.name == n)).unwrap_or_else(|| format!("{}", versions.len().saturating_add(1)))
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

    /// Whether the graph is the single Lumetri again: one version "A" with one plain, unlabelled node.
    fn is_trivial(&self) -> bool {
        match self.versions.as_slice() {
            [v] => v.name == "A" && matches!(v.nodes.as_slice(), [n] if n.is_plain() && n.label.is_empty()),
            _ => false,
        }
    }
}

/// Whether the effect is a Lumetri holding a real node graph (anything beyond the single Lumetri;
/// what needs schema v18).
pub fn has_graph(e: &EffectInstance) -> bool {
    e.effect == "lumetri" && e.color.as_deref().is_some_and(|g| !g.versions.is_empty() && !g.is_trivial())
}

/// Drop a graph that is the single Lumetri again (after an edit).
fn settle(e: &mut EffectInstance) {
    if e.color.as_deref().is_some_and(|g| g.versions.is_empty() || g.is_trivial()) {
        e.color = None;
    }
}

impl GradeVersion {
    /// The selected node's index (clamped).
    pub fn selected(&self) -> usize {
        self.selected.min(self.nodes.len().saturating_sub(1))
    }
}

/// The stored parameters of every node of every version (not the selected node's, which are the
/// effect's own): see `EffectInstance::all_params_mut`.
pub fn node_params_mut(color: &mut Option<Box<ColorGraph>>) -> impl Iterator<Item = (&String, &mut Param)> {
    color.iter_mut().flat_map(|g| &mut g.versions).flat_map(|v| &mut v.nodes).flat_map(|n| &mut n.params)
}

/// The effect's graph, if it is a Lumetri with a usable one (another effect's `color` is ignored).
pub fn graph(e: &EffectInstance) -> Option<&ColorGraph> {
    e.color.as_deref().filter(|g| e.effect == "lumetri" && g.usable())
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

/// A node as a plain Lumetri Color instance (its parameters over Lumetri's defaults, so a node
/// missing some keeps the defaults; no masks, no graph): what the renderer evaluates for it.
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
    for (k, p) in n.params {
        e.params.insert(k.clone(), p.clone());
    }
    e
}

/// Where a node applies.
#[derive(Clone, Copy, Debug)]
pub enum NodeMask<'a> {
    /// The whole frame (no node mask).
    Frame,
    /// Inside this mask of the effect.
    Inside(&'a Mask),
    /// Nowhere: the node names a mask the effect does not have.
    Nowhere,
}

/// Where node `n` applies: inside the first of the effect's masks with its name. A node naming a
/// mask the effect does not have applies nowhere, never to the whole frame.
pub fn node_mask<'a>(e: &'a EffectInstance, n: &NodeRef<'_>) -> NodeMask<'a> {
    match n.mask {
        None => NodeMask::Frame,
        Some(name) => e.masks.iter().find(|m| m.name == name).map_or(NodeMask::Nowhere, NodeMask::Inside),
    }
}

/// Whether the effect renders as the single Lumetri of its own parameters (no graph, or a graph
/// whose active version is one plain node).
pub fn is_single(e: &EffectInstance) -> bool {
    match graph(e).and_then(ColorGraph::active_version) {
        None => true,
        Some(v) => v.nodes.len() == 1 && v.nodes.first().is_some_and(ColorNode::is_plain),
    }
}

/// The effect's masks that limit the whole effect: all of them except the ones nodes of the active
/// version are limited to (the first mask of each name a node names, see [`node_mask`]).
pub fn effect_wide_masks(e: &EffectInstance) -> Vec<Mask> {
    let nodes = active_nodes(e);
    let taken: Vec<usize> = nodes.iter().filter_map(|n| n.mask).filter_map(|name| e.masks.iter().position(|m| m.name == name)).collect();
    e.masks.iter().enumerate().filter(|(i, _)| !taken.contains(i)).map(|(_, m)| m.clone()).collect()
}

/// Follow a mask rename (`to` = the new name) or removal (`to` = None: the nodes it limited become
/// unlimited, which the node strip shows) in every node of every version, so no node keeps a
/// dangling name a later mask could silently take over.
pub fn rename_mask(e: &mut EffectInstance, from: &str, to: Option<&str>) {
    if let Some(g) = e.color.as_deref_mut() {
        for n in g.versions.iter_mut().flat_map(|v| &mut v.nodes) {
            if n.mask.as_deref() == Some(from) {
                n.mask = to.map(str::to_string);
            }
        }
    }
}

/// The effect's graph for editing: created (as the single node it already is) when it has none,
/// else [`repair`]ed, which keeps every valid version.
fn graph_mut(e: &mut EffectInstance) -> &mut ColorGraph {
    let g = e.color.get_or_insert_with(|| Box::new(ColorGraph::single()));
    if g.versions.is_empty() {
        **g = ColorGraph::single();
    }
    repair(g);
    g
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
    if i < n { Ok(i) } else { Err(format!("no node {i} (the grade's nodes are 0 to {})", n.saturating_sub(1))) }
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
    settle(e);
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
    // an empty label shows as "Node <n>" (translated, by position)
    v.nodes.insert(at.min(v.nodes.len()), ColorNode::new(label.chars().take(MAX_LABEL).collect::<String>()));
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
    settle(e);
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
    settle(e);
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
    settle(e);
    Ok(())
}

/// Turn node `i` on or off (off = bypassed).
pub fn set_enabled(e: &mut EffectInstance, i: usize, on: bool) -> Result<(), String> {
    node_mut(e, i)?.enabled = on;
    settle(e);
    Ok(())
}

pub fn set_label(e: &mut EffectInstance, i: usize, label: &str) -> Result<(), String> {
    node_mut(e, i)?.label = label.chars().take(MAX_LABEL).collect();
    settle(e);
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
    settle(e);
    Ok(())
}

fn version_error(i: usize, n: usize) -> String {
    format!("no grade version {i} (the effect's versions are 0 to {})", n.saturating_sub(1))
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
    let mut v = if duplicate { active_grade(e) } else { GradeVersion { name: String::new(), nodes: vec![ColorNode::new("")], selected: 0 } };
    stash(e);
    let g = graph_mut(e);
    v.name =
        name.map(|n| n.trim().chars().take(MAX_NAME).collect()).filter(|n: &String| !n.trim().is_empty()).unwrap_or_else(|| next_version_name(&g.versions));
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
        return Err(version_error(i, n));
    }
    stash(e);
    graph_mut(e).active = i;
    load(e);
    settle(e);
    Ok(())
}

pub fn rename_version(e: &mut EffectInstance, i: usize, name: &str) -> Result<(), String> {
    let name: String = name.trim().chars().take(MAX_NAME).collect();
    if name.is_empty() {
        return Err("a grade version needs a name".into());
    }
    let n = versions(e).0.len();
    let g = graph_mut(e);
    let v = g.versions.get_mut(i).ok_or_else(|| version_error(i, n))?;
    v.name = name;
    settle(e);
    Ok(())
}

/// Delete grade version `i` (an effect keeps at least one).
pub fn delete_version(e: &mut EffectInstance, i: usize) -> Result<(), String> {
    let n = versions(e).0.len();
    if i >= n {
        return Err(version_error(i, n));
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
    settle(e);
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
    settle(e);
    Ok(())
}

/// Move every keyframe of `grade` by `ticks` (saturating): a grade pasted onto a clip whose media
/// starts elsewhere.
pub fn shift_keyframes(grade: &mut GradeVersion, ticks: i64) {
    for p in grade.nodes.iter_mut().flat_map(|n| n.params.values_mut()) {
        for k in &mut p.keyframes {
            k.time = filmcraft_time::Tick(k.time.0.saturating_add(ticks));
        }
    }
}

/// Reset the active version to one node with Lumetri's defaults.
pub fn reset_grade(e: &mut EffectInstance) {
    let fresh = GradeVersion { name: String::new(), nodes: vec![ColorNode::new("")], selected: 0 };
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

    fn graph_json(versions: usize, nodes: usize) -> String {
        let node = r#"{"params":{}}"#;
        let v = format!(r#"{{"name":"V","nodes":[{}]}}"#, vec![node; nodes].join(","));
        format!(r#"{{"versions":[{}],"active":0}}"#, vec![v.as_str(); versions].join(","))
    }

    #[test]
    fn files_over_the_caps_are_refused_when_read() {
        assert!(serde_json::from_str::<ColorGraph>(&graph_json(1, MAX_NODES)).is_ok());
        assert!(serde_json::from_str::<ColorGraph>(&graph_json(MAX_VERSIONS, 1)).is_ok());
        let err = serde_json::from_str::<ColorGraph>(&graph_json(1, MAX_NODES + 1)).unwrap_err().to_string();
        assert!(err.contains("at most 32"), "{err}");
        assert!(serde_json::from_str::<ColorGraph>(&graph_json(MAX_VERSIONS + 1, 1)).is_err());
        // a huge list is refused at the cap, not read whole
        assert!(serde_json::from_str::<ColorGraph>(&graph_json(1, 100_000)).is_err());
        // and through an effect instance (project files, OTIO metadata, presets)
        let mut e = serde_json::to_value(lumetri()).unwrap();
        e["color"] = serde_json::from_str(&graph_json(2, 40).replace(r#""params":{}"#, r#""mix":1.0"#)).unwrap();
        assert!(serde_json::from_value::<EffectInstance>(e).is_err());
    }

    #[test]
    fn a_read_graph_is_repaired_without_touching_valid_versions() {
        // version A valid (2 nodes, the second keyframe-free exposure 0.7), version B empty, active B
        let mut e = lumetri();
        set_exposure(&mut e, 0.25);
        add_node(&mut e, None, "").unwrap();
        set_exposure(&mut e, 0.7);
        add_version(&mut e, None, true).unwrap();
        let mut j = serde_json::to_value(&e).unwrap();
        j["color"]["versions"][1]["nodes"] = serde_json::json!([]);
        j["color"]["versions"][0]["nodes"][0]["mix"] = serde_json::json!(4.0);
        j["color"]["versions"][0]["nodes"][0]["label"] = serde_json::json!("x".repeat(500));
        j["color"]["versions"][0]["name"] = serde_json::json!("   ");
        j["color"]["active"] = serde_json::json!(1);
        let mut e: EffectInstance = serde_json::from_value(j).unwrap();
        let g = e.color.as_deref().unwrap();
        assert_eq!(g.versions[1].nodes.len(), 1, "the empty version got a node");
        assert_eq!(g.versions[0].nodes.len(), 2, "the valid version is untouched");
        assert_eq!(g.versions[0].nodes[0].mix, 1.0);
        assert_eq!(g.versions[0].nodes[0].label.chars().count(), MAX_LABEL);
        assert_eq!(g.versions[0].name, "A", "an unnamed version gets a free letter");
        // switching to the valid version keeps it (it used to be replaced by a fresh graph)
        switch_version(&mut e, 0).unwrap();
        let ex: Vec<f64> = active_nodes(&e).iter().map(|n| n.params.get("exposure").unwrap().value.as_f64().unwrap()).collect();
        assert_eq!(ex, vec![0.25, 0.7]);
        assert_eq!(versions(&e).0.len(), 2);
    }

    #[test]
    fn hostile_indices_are_errors_not_panics() {
        let mut e = lumetri();
        add_node(&mut e, None, "").unwrap();
        add_version(&mut e, None, true).unwrap();
        let m = usize::MAX;
        assert!(select_node(&mut e, m).unwrap_err().contains(&m.to_string()), "0-based index in the error");
        assert!(remove_node(&mut e, m).is_err());
        assert!(move_node(&mut e, m, 0).is_err() && move_node(&mut e, 0, m).is_err());
        assert!(set_mix(&mut e, m, 0.5).is_err() && set_enabled(&mut e, m, false).is_err() && set_label(&mut e, m, "x").is_err());
        assert!(set_mask(&mut e, m, None).is_err());
        assert!(switch_version(&mut e, m).is_err() && rename_version(&mut e, m, "x").is_err() && delete_version(&mut e, m).is_err());
        assert!(add_node(&mut e, Some(m), "").is_ok(), "after = past the end adds at the end");
        assert_eq!(select_node(&mut e, 9).unwrap_err(), "no node 9 (the grade's nodes are 0 to 2)");
    }

    #[test]
    fn a_graph_back_to_one_plain_node_is_dropped() {
        let mut e = lumetri();
        set_exposure(&mut e, 0.5);
        add_node(&mut e, None, "").unwrap();
        assert!(has_graph(&e));
        remove_node(&mut e, 1).unwrap();
        assert!(e.color.is_none() && !has_graph(&e), "one plain node again: no graph");
        assert_eq!(exposure(&e), 0.5);
        select_node(&mut e, 0).unwrap();
        assert!(e.color.is_none(), "selecting the only node makes no graph");
        // a label, a mix or a second version is a real graph
        set_label(&mut e, 0, "Base").unwrap();
        assert!(has_graph(&e));
        set_label(&mut e, 0, "").unwrap();
        assert!(e.color.is_none());
        set_mix(&mut e, 0, 0.5).unwrap();
        assert!(has_graph(&e));
        reset_grade(&mut e);
        assert!(e.color.is_none());
        // another effect's graph is never one
        let mut blur = find_effect("gaussian_blur").unwrap().instance();
        blur.color = Some(Box::new(ColorGraph::single()));
        add_version_raw(&mut blur);
        assert!(!has_graph(&blur) && graph(&blur).is_none());
    }

    fn add_version_raw(e: &mut EffectInstance) {
        if let Some(g) = e.color.as_deref_mut() {
            g.versions.push(GradeVersion { name: "B".into(), nodes: vec![ColorNode::new("")], selected: 0 });
        }
    }

    #[test]
    fn deleting_the_active_version_in_the_middle() {
        let mut e = lumetri();
        set_exposure(&mut e, 0.1);
        add_version(&mut e, None, false).unwrap();
        set_exposure(&mut e, 0.2);
        add_version(&mut e, None, false).unwrap();
        set_exposure(&mut e, 0.3);
        switch_version(&mut e, 1).unwrap();
        delete_version(&mut e, 1).unwrap();
        assert_eq!(versions(&e), (vec!["A".to_string(), "C".to_string()], 1), "the next version becomes active");
        assert_eq!(exposure(&e), 0.3);
        switch_version(&mut e, 0).unwrap();
        assert_eq!(exposure(&e), 0.1);
    }

    #[test]
    fn node_masks_resolve_to_the_first_mask_of_their_name_and_follow_renames() {
        let mut e = lumetri();
        let rect = crate::mask::MaskPath::rect(0.0, 0.0, 10.0, 10.0);
        e.masks = vec![Mask::new("m", rect.clone()), Mask::new("m", rect.clone()), Mask::new("sky", rect)];
        add_node(&mut e, None, "").unwrap();
        set_mask(&mut e, 1, Some("m")).unwrap();
        // the node takes the first "m"; the second "m" and "sky" still limit the whole effect
        assert_eq!(effect_wide_masks(&e).len(), 2);
        let n = active_nodes(&e)[1];
        assert!(matches!(node_mask(&e, &n), NodeMask::Inside(m) if std::ptr::eq(m, &e.masks[0])));
        // a dangling name applies nowhere
        rename_mask(&mut e, "m", Some("face"));
        assert_eq!(active_nodes(&e)[1].mask, Some("face"));
        let n = active_nodes(&e)[1];
        assert!(matches!(node_mask(&e, &n), NodeMask::Nowhere));
        rename_mask(&mut e, "face", None);
        assert_eq!(active_nodes(&e)[1].mask, None);
    }

    #[test]
    fn every_node_parameter_is_reachable_for_keyframe_upkeep() {
        let mut e = lumetri();
        add_node(&mut e, None, "").unwrap();
        add_version(&mut e, None, true).unwrap();
        let own = e.params.len();
        // own params (B's selected node), A's two nodes, and B's other node (B duplicated A)
        assert_eq!(e.all_params_mut().len(), own * 4);
        let k = crate::keyframe::Keyframe::new(filmcraft_time::Tick(10), ParamValue::Float(1.0));
        e.color.as_deref_mut().unwrap().versions[0].nodes[0].params.get_mut("exposure").unwrap().keyframes.push(k);
        assert!(e.is_animated(), "a keyframed node of another version animates the effect");
    }
}
