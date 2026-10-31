//! #275 (SNV, 14.10.2026): whether SongPlayer's clip runs in Arena — pure.
//!
//! After an Arena relaunch no clip runs until the operator presses a Stream
//! Deck scene; the wall was black for 5.5 h while songs played on program,
//! and the wall report (it reads WHERE Arena draws) said `ok`. The Resolume
//! host driver already fetches `/api/v1/composition`; [`track`] reads, from
//! that same body, every clip whose source is SongPlayer's Spout sender
//! (`sp_core::wall_clip::SPOUT_SOURCES`) or an NDI source, with the ids of
//! the params that decide whether it shows: the clip's `connected`, its
//! layer's `bypassed`, `video.opacity` and `master` (the layer's fader), the
//! same three of the layer group holding the layer (if any), and the
//! composition's `video.opacity` and `master` (each when the body carries
//! it). Their values are in the body too, so a full refresh evaluates at
//! once; between refreshes the driver reads only those params by id
//! (`driver_sp_clips.rs`, every [`POLL_EVERY`]).
//!
//! [`evaluate`] gives the fact: `running` = at least one SongPlayer clip is
//! connected (`Connected`, `Connected & previewing`) on a layer that is not
//! bypassed with its opacity and its master above 0, in a group (if any)
//! likewise, under a composition whose opacity and master are above 0;
//! `none` = no such clip (also when the composition holds no SongPlayer
//! clip); `unknown` = the composition is not loaded (no layers: Arena's REST
//! answers before it has loaded, #217), a param the rule needs carries no
//! id, or a value did not read. Visible NDI clips are listed by name (Arena
//! does not name an NDI source). Not read (unset on SNV, 15.10.2026): a
//! layer's or a group's `solo` and the composition's crossfader, so a solo'd
//! other layer, or a crossfader away from an A/B-assigned SongPlayer layer,
//! still reads `running`.
//!
//! Read only: nothing here (nor in the driver's poll) triggers, connects or
//! restores a clip, nor changes any setting (the owner's rules, #223,
//! #259 6142937258).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sp_core::wall_clip::{ClipState, ClipView, NDI_SOURCE, SPOUT_SOURCES};

use crate::resolume::sp_clip_store::SpClipStore;

/// How often the driver reads the tracked params by id between full
/// refreshes (a full refresh is a read too). About 39 GETs of ~140 B on
/// SNV (19 clips' `connected`, 4 layers and 2 groups × 3, the composition's
/// 2): the design's light cadence (#157 hangs Arena's REST under the 14 MB
/// composition, never under these).
pub const POLL_EVERY: Duration = Duration::from_secs(30);

/// The most one poll may take (review round 1): the reads run inside the
/// driver's step, so a slow Arena REST would hold the title and lyric
/// pushes behind it. ~39 reads are expected to take well under a second
/// (not measured yet: the poll's DEBUG line carries `elapsed_ms`); past
/// this the poll is unknown and the next one tries again.
pub const POLL_BUDGET: Duration = Duration::from_secs(2);

/// An Arena param's id (`parameter/by-id/{id}`); Arena gives every param a
/// new one on each relaunch.
pub type ParamId = i64;

/// A param's value as Arena gives it: a `ParamBoolean` (`bypassed`), a
/// `ParamRange` (`opacity`) or a `ParamState` (`connected`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ParamValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

/// The values read, by param id.
pub type Values = BTreeMap<ParamId, ParamValue>;

/// One `GET /api/v1/parameter/by-id/{id}` answer: only its value is read
/// (a typed struct: never `serde_json::Value`'s own `Deserialize`,
/// `rust-workspace.md`); the other fields are skipped.
#[derive(Debug, Deserialize)]
pub struct ParamAnswer {
    pub value: ParamValue,
}

/// The three params that decide whether a layer or a layer group shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visibility {
    pub bypassed: ParamId,
    pub opacity: ParamId,
    /// The layer's or group's master fader (a `ParamRange` 0..1).
    pub master: ParamId,
}

/// One clip to watch: what it is and every param on its way to the wall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedClip {
    pub view: ClipView,
    /// An NDI clip: listed when it shows, never counted as SongPlayer's.
    pub ndi: bool,
    pub connected: ParamId,
    pub layer: Visibility,
    pub group: Option<Visibility>,
}

/// The clips of a loaded composition, and its own levels: its
/// `video.opacity` and its `master`, each when the body carries it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Watched {
    pub clips: Vec<TrackedClip>,
    pub composition: Vec<ParamId>,
}

/// What the driver watches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Tracked {
    /// No composition read yet, an empty one (Arena still loading), or the
    /// outage forgot it.
    #[default]
    NotLoaded,
    /// A param the rule needs carries no id: nothing can be read by id.
    Unreadable(String),
    /// The clips to watch.
    Watched(Watched),
}

impl Tracked {
    /// Every param to read, each once, in id order.
    pub fn params(&self) -> Vec<ParamId> {
        let Tracked::Watched(watched) = self else {
            return Vec::new();
        };
        let mut ids = BTreeSet::new();
        for clip in &watched.clips {
            ids.insert(clip.connected);
            for vis in std::iter::once(clip.layer).chain(clip.group) {
                ids.insert(vis.bypassed);
                ids.insert(vis.opacity);
                ids.insert(vis.master);
            }
        }
        ids.extend(&watched.composition);
        ids.into_iter().collect()
    }
}

/// What a full refresh found: the clips to watch and their values in the
/// same body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Read {
    pub tracked: Tracked,
    pub values: Values,
}

/// The fact for one Arena host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fact {
    pub state: ClipState,
    /// SongPlayer's clips that show (empty unless running).
    pub clips: Vec<ClipView>,
    /// The NDI clips that show.
    pub ndi_clips: Vec<ClipView>,
    /// Why it is unknown (English, for the log and the gate).
    pub why: Option<String>,
    /// Unknown because SongPlayer's own reader cannot read Arena's answer (a
    /// param with no id, a value of another shape, a by-id answer that does
    /// not decode), never because Arena did not answer: the post-deploy gate
    /// fails on it (review round 1).
    pub reader_fault: bool,
}

impl Fact {
    /// Unknown: Arena did not answer, or has not loaded its composition.
    pub fn unknown(why: &str) -> Self {
        Self {
            why: Some(why.to_string()),
            ..Self::default()
        }
    }

    /// Unknown: SongPlayer's own reader cannot read Arena's answer.
    pub fn reader_fault(why: &str) -> Self {
        Self {
            reader_fault: true,
            ..Self::unknown(why)
        }
    }
}

/// A param's value in the body, when it has one of the three shapes.
fn value_of(param: &serde_json::Value) -> Option<ParamValue> {
    match &param["value"] {
        serde_json::Value::Bool(b) => Some(ParamValue::Bool(*b)),
        serde_json::Value::Number(n) => n.as_f64().map(ParamValue::Number),
        serde_json::Value::String(s) => Some(ParamValue::Text(s.clone())),
        _ => None,
    }
}

/// A param the rule needs: its id (else why not), its value noted.
fn watch_param(
    param: &serde_json::Value,
    what: &str,
    values: &mut Values,
) -> Result<ParamId, String> {
    let id = param["id"]
        .as_i64()
        .ok_or_else(|| format!("{what} carries no param id"))?;
    if let Some(value) = value_of(param) {
        values.insert(id, value);
    }
    Ok(id)
}

/// A layer's or a group's two params.
fn watch_visibility(
    item: &serde_json::Value,
    what: &str,
    values: &mut Values,
) -> Result<Visibility, String> {
    Ok(Visibility {
        bypassed: watch_param(&item["bypassed"], &format!("{what}'s bypassed"), values)?,
        opacity: watch_param(
            &item["video"]["opacity"],
            &format!("{what}'s opacity"),
            values,
        )?,
        master: watch_param(&item["master"], &format!("{what}'s master"), values)?,
    })
}

/// An item's `name.value` ("" when it has none).
fn name_of(item: &serde_json::Value) -> String {
    item["name"]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The layer group holding each layer, by the layer's id.
fn groups_of(composition: &serde_json::Value) -> HashMap<i64, &serde_json::Value> {
    let mut groups = HashMap::new();
    for group in composition["layergroups"].as_array().into_iter().flatten() {
        for layer in group["layers"].as_array().into_iter().flatten() {
            if let Some(id) = layer["id"].as_i64() {
                groups.insert(id, group);
            }
        }
    }
    groups
}

/// A clip's source when it is one to watch: SongPlayer's Spout sender or an
/// NDI source.
fn watched_source(clip: &serde_json::Value) -> Option<&str> {
    let source = clip["video"]["description"].as_str()?;
    (SPOUT_SOURCES.contains(&source) || source == NDI_SOURCE).then_some(source)
}

/// The clips to watch in a loaded composition, and their values; `None`
/// while it is not loaded (no layers).
fn watch(composition: &serde_json::Value, values: &mut Values) -> Result<Option<Watched>, String> {
    let Some(layers) = composition["layers"].as_array().filter(|l| !l.is_empty()) else {
        return Ok(None);
    };
    let groups = groups_of(composition);
    let mut levels = Vec::new();
    if let Some(opacity) = composition["video"].get("opacity") {
        levels.push(watch_param(opacity, "the composition's opacity", values)?);
    }
    if let Some(master) = composition.get("master") {
        levels.push(watch_param(master, "the composition's master", values)?);
    }
    let mut clips = Vec::new();
    for layer in layers {
        let sources: Vec<(&serde_json::Value, &str)> = layer["clips"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|clip| watched_source(clip).map(|source| (clip, source)))
            .collect();
        if sources.is_empty() {
            continue;
        }
        let layer_name = name_of(layer);
        let layer_vis = watch_visibility(layer, &format!("layer {layer_name:?}"), values)?;
        let group = match layer["id"].as_i64().and_then(|id| groups.get(&id)) {
            Some(group) => {
                let what = format!("layer group {:?}", name_of(group));
                Some(watch_visibility(group, &what, values)?)
            }
            None => None,
        };
        for (clip, source) in sources {
            let clip_name = name_of(clip);
            let what = format!("clip {clip_name:?}'s connected");
            clips.push(TrackedClip {
                connected: watch_param(&clip["connected"], &what, values)?,
                view: ClipView {
                    layer: layer_name.clone(),
                    clip: clip_name,
                    source: source.to_string(),
                },
                ndi: source == NDI_SOURCE,
                layer: layer_vis,
                group,
            });
        }
    }
    Ok(Some(Watched {
        clips,
        composition: levels,
    }))
}

/// What to watch in a full refresh's composition body, and the values it
/// already carries.
pub fn track(composition: &serde_json::Value) -> Read {
    let mut values = Values::new();
    match watch(composition, &mut values) {
        Ok(Some(watched)) => Read {
            tracked: Tracked::Watched(watched),
            values,
        },
        Ok(None) => Read::default(),
        Err(why) => Read {
            tracked: Tracked::Unreadable(why),
            values: Values::new(),
        },
    }
}

/// A param's value, or why it did not read.
fn value(values: &Values, id: ParamId) -> Result<&ParamValue, String> {
    values
        .get(&id)
        .ok_or_else(|| format!("param {id} did not read"))
}

/// A `ParamState` `connected`: `Connected` or `Connected & previewing`.
fn connected(values: &Values, id: ParamId) -> Result<bool, String> {
    match value(values, id)? {
        ParamValue::Text(state) => Ok(state.starts_with("Connected")),
        other => Err(format!("param {id} is no clip state: {other:?}")),
    }
}

/// An opacity above 0.
fn opaque(values: &Values, id: ParamId) -> Result<bool, String> {
    match value(values, id)? {
        ParamValue::Number(level) => Ok(*level > 0.0),
        other => Err(format!("param {id} is no opacity: {other:?}")),
    }
}

/// A layer or a group shows: not bypassed, and opaque.
fn shows(values: &Values, vis: Visibility) -> Result<bool, String> {
    let bypassed = match value(values, vis.bypassed)? {
        ParamValue::Bool(bypassed) => *bypassed,
        other => return Err(format!("param {} is no bypass: {other:?}", vis.bypassed)),
    };
    let seen = opaque(values, vis.opacity)?;
    let fader = opaque(values, vis.master)?;
    Ok(!bypassed && seen && fader)
}

/// The SongPlayer and the NDI clips that show; every value is read (one
/// that did not read is the answer, whatever the others say).
fn visible(watched: &Watched, values: &Values) -> Result<(Vec<ClipView>, Vec<ClipView>), String> {
    let mut master_shows = true;
    for &id in &watched.composition {
        master_shows &= opaque(values, id)?;
    }
    let (mut spout, mut ndi) = (Vec::new(), Vec::new());
    for clip in &watched.clips {
        let is_connected = connected(values, clip.connected)?;
        let layer_shows = shows(values, clip.layer)?;
        let group_shows = match clip.group {
            Some(group) => shows(values, group)?,
            None => true,
        };
        if is_connected && layer_shows && group_shows && master_shows {
            let list = if clip.ndi { &mut ndi } else { &mut spout };
            list.push(clip.view.clone());
        }
    }
    Ok((spout, ndi))
}

/// The fact: what runs, given the values read.
pub fn evaluate(tracked: &Tracked, values: &Values) -> Fact {
    let watched = match tracked {
        Tracked::NotLoaded => return Fact::unknown("Arena's composition is not loaded"),
        Tracked::Unreadable(why) => return Fact::reader_fault(why),
        Tracked::Watched(watched) => watched,
    };
    match visible(watched, values) {
        Ok((clips, ndi_clips)) => Fact {
            state: if clips.is_empty() {
                ClipState::NotRunning
            } else {
                ClipState::Running
            },
            clips,
            ndi_clips,
            why: None,
            reader_fault: false,
        },
        // A value of another shape, or one the body lacks (a poll stops at
        // its first unread param, so only a body can lack one).
        Err(why) => Fact::reader_fault(&why),
    }
}

/// The driver's watch of one host: what it tracks, when it last read the
/// values (a full refresh or a poll), when a full refresh last evaluated
/// its body, the last fact, the store it publishes into (production:
/// `sp_clip_store::global()`; none: the fact stays here), whether the poll
/// holds until the next full refresh, and the poll's budget (`None`:
/// [`POLL_BUDGET`]; a test shortens it).
#[derive(Debug, Default)]
pub struct Watch {
    pub tracked: Tracked,
    pub read_at: Option<Instant>,
    pub body_at: Option<Instant>,
    pub fact: Fact,
    pub store: Option<&'static SpClipStore>,
    /// A param answered 404 while the composition still lists it (a
    /// refused id, never the steady state): reading it again would only
    /// refetch the 14 MB composition, so the poll waits for the next full
    /// refresh, whose body is read (review round 1).
    pub held: bool,
    pub budget: Option<Duration>,
}

impl Watch {
    /// A poll is due: not held, something to read, and the last read is
    /// [`POLL_EVERY`] old (or none yet).
    pub fn poll_due(&self, now: Instant) -> bool {
        !self.held
            && !self.tracked.params().is_empty()
            && self
                .read_at
                .is_none_or(|at| now.duration_since(at) >= POLL_EVERY)
    }

    /// The most one poll may take.
    pub fn budget(&self) -> Duration {
        self.budget.unwrap_or(POLL_BUDGET)
    }
}

#[cfg(test)]
#[path = "sp_clips_tests.rs"]
mod tests;
