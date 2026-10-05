//! Loading and validating story datadisks (see `ishtaria-protocol/schemas/datadisk.schema.json`).
//!
//! A datadisk is a directory of YAML files. Every id is local to its disk in the files and
//! becomes `<disk>:<id>` once several disks are composed into one [`Story`].

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// Biome names a datadisk may require, as numbered by `environment::generate`.
pub const BIOMES: [(&str, u8); 8] = [
    ("ocean", 0),
    ("lake", 1),
    ("river", 2),
    ("lowland", 3),
    ("grassland", 4),
    ("temperate_forest", 5),
    ("mountain", 6),
    ("polar", 7),
];

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FILES: usize = 512;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Mirrors the file format; not every field is used by the server yet.
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub requires_ruleset: String,
    pub license: String,
    pub attribution: String,
    #[serde(default)]
    pub rating: Option<String>,
    #[serde(default = "default_languages")]
    pub languages: Vec<String>,
    #[serde(default)]
    pub requires: Vec<Requirement>,
    #[serde(default)]
    pub conflicts: Vec<String>,
}

fn default_languages() -> Vec<String> {
    vec!["en".into()]
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub disk: String,
    pub version: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Near {
    pub place: String,
    pub min_m: f64,
    pub max_m: f64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeightRange {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    #[serde(default)]
    pub biome: Vec<String>,
    pub height_m: Option<HeightRange>,
    pub max_slope_deg: Option<f64>,
    /// `true`: the sea is within a few hundred metres (a harbour place); `false`: it is not.
    pub coast: Option<bool>,
    #[serde(default)]
    pub near: Vec<Near>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    pub id: String,
    pub kind: String,
    pub name_key: String,
    pub parent: Option<String>,
    pub radius_m: Option<f64>,
    /// Minimum distance to other places of the same kind.
    pub spacing_m: Option<f64>,
    /// Buildings and props the engine builds at the place.
    pub scenery: Option<Scenery>,
    /// New characters of the world appear here (the first such place of the first disk wins).
    #[serde(default)]
    pub spawn: bool,
    #[serde(default)]
    pub requires: Requires,
}

/// A generated layout of models from the bundled Kenney kits.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenery {
    /// `town` (houses, plaza, walls; harbour when by the sea) or `graveyard`.
    pub preset: String,
    /// `hamlet`, `village` or `town` (presets that build settlements).
    pub size: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Character {
    pub pack: String,
    pub skin: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Mirrors the file format; not every field is used by the server yet.
pub struct Npc {
    pub id: String,
    pub name_key: String,
    pub place: String,
    pub character: Character,
    pub dialogue: String,
    pub bio_key: Option<String>,
    /// Image below `media/` shown beside the dialogue text.
    pub portrait: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestStageRef {
    pub quest: String,
    pub stage: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemQty {
    pub item: String,
    pub qty: Option<i64>,
}

impl ItemQty {
    pub fn quantity(&self) -> i64 {
        self.qty.unwrap_or(1)
    }
}

/// A structured condition: exactly one field is set.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    pub all: Option<Vec<Condition>>,
    pub any: Option<Vec<Condition>>,
    pub not: Option<Box<Condition>>,
    pub flag: Option<String>,
    pub quest_stage: Option<QuestStageRef>,
    pub has_item: Option<ItemQty>,
    pub gold_at_least: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Once {
    pub key: String,
    pub effects: Vec<Effect>,
}

/// A structured effect: exactly one field is set.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    pub give_item: Option<ItemQty>,
    pub take_item: Option<ItemQty>,
    pub gold: Option<i64>,
    pub set_flag: Option<String>,
    pub set_stage: Option<QuestStageRef>,
    pub once: Option<Once>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub text_key: String,
    #[serde(rename = "if")]
    pub condition: Option<Condition>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    pub goto: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Branch {
    #[serde(rename = "if")]
    pub condition: Option<Condition>,
    pub goto: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub text_key: Option<String>,
    pub branch: Option<Vec<Branch>>,
    pub choices: Option<Vec<Choice>>,
    #[serde(default)]
    pub effects: Vec<Effect>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Mirrors the file format; not every field is used by the server yet.
pub struct Dialogue {
    pub id: String,
    pub status: Option<String>,
    pub generated_by: Option<String>,
    /// Track from `music/*.yaml` played during the conversation.
    pub music: Option<String>,
    pub start: String,
    pub nodes: BTreeMap<String, Node>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub text_key: String,
    #[serde(default)]
    pub final_stage: bool,
    pub reach: Option<Reach>,
}

/// Moves the quest on when the player comes within `radius_m` of a place.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reach {
    pub place: String,
    pub radius_m: f64,
    pub goto: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageFile {
    pub text_key: String,
    #[serde(rename = "final", default)]
    pub final_stage: bool,
    pub reach: Option<Reach>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestFile {
    pub id: String,
    pub title_key: String,
    pub start_stage: String,
    pub stages: BTreeMap<String, StageFile>,
}

#[derive(Clone, Debug)]
pub struct Quest {
    pub id: String,
    pub title_key: String,
    pub start_stage: String,
    pub stages: BTreeMap<String, Stage>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Mirrors the file format; not every field is used by the server yet.
pub struct Lore {
    pub id: String,
    pub category: Option<String>,
    pub title_key: String,
    pub text_key: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Music {
    pub id: String,
    pub file: String,
    pub title_key: String,
    #[serde(rename = "loop", default)]
    pub looped: bool,
}

/// Largest portrait / track a datadisk may ship.
pub const MAX_PORTRAIT_BYTES: u64 = 1 << 20;
pub const MAX_MUSIC_BYTES: u64 = 8 << 20;

/// Content type for a media path, by extension (the only types a disk may ship).
pub fn media_type(path: &str) -> Option<&'static str> {
    match path.rsplit_once('.')?.1 {
        "png" => Some("image/png"),
        "jpg" => Some("image/jpeg"),
        "ogg" => Some("audio/ogg"),
        _ => None,
    }
}

/// A media path is `media/<dirs>/<name>.<ext>` of lowercase ASCII, with no traversal.
pub fn is_media_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("media/") else {
        return false;
    };
    media_type(path).is_some()
        && path.len() <= 160
        && !rest.is_empty()
        && rest.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
                })
        })
}

/// One disk exactly as read from its directory (ids still local).
#[derive(Clone, Debug)]
pub struct Disk {
    pub manifest: Manifest,
    pub sha256: String,
    pub places: Vec<Place>,
    pub npcs: Vec<Npc>,
    pub dialogues: Vec<Dialogue>,
    pub quests: Vec<Quest>,
    pub lore: Vec<Lore>,
    pub music: Vec<Music>,
    /// Media files (path below the disk -> absolute file) with their sha256.
    pub media: BTreeMap<String, (PathBuf, String)>,
    pub strings: BTreeMap<String, BTreeMap<String, String>>,
}

fn is_id(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_disk_id(text: &str) -> bool {
    text.len() >= 2 && is_id(text, 32)
}

fn yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "yaml") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn read_limited(path: &Path, hasher: &mut Sha256, root: &Path) -> Result<String> {
    let meta = fs::metadata(path)?;
    ensure!(
        meta.len() <= MAX_FILE_BYTES,
        "{} is larger than {MAX_FILE_BYTES} bytes",
        path.display()
    );
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    hasher.update((relative.len() as u64).to_le_bytes());
    hasher.update(relative.as_bytes());
    hasher.update((text.len() as u64).to_le_bytes());
    hasher.update(text.as_bytes());
    Ok(text)
}

fn parse<T: for<'de> Deserialize<'de>>(path: &Path, text: &str) -> Result<T> {
    serde_yaml::from_str(text).with_context(|| format!("invalid {}", path.display()))
}

impl Disk {
    /// Reads and checks one disk. `allow_draft` admits unreviewed generated dialogues.
    pub fn load(dir: &Path, allow_draft: bool) -> Result<Self> {
        let mut hasher = Sha256::new();
        let manifest_path = dir.join("datadisk.yaml");
        let manifest: Manifest = parse(
            &manifest_path,
            &read_limited(&manifest_path, &mut hasher, dir)?,
        )?;
        ensure!(is_disk_id(&manifest.id), "invalid disk id {}", manifest.id);
        parse_version(&manifest.version)?;
        let mut count = 1;
        let mut load_list = |sub: &str| -> Result<Vec<(PathBuf, String)>> {
            let mut out = Vec::new();
            for path in yaml_files(&dir.join(sub))? {
                count += 1;
                ensure!(count <= MAX_FILES, "too many files in the datadisk");
                let text = read_limited(&path, &mut hasher, dir)?;
                out.push((path, text));
            }
            Ok(out)
        };
        let mut places = Vec::new();
        for (path, text) in load_list("places")? {
            places.extend(parse::<Vec<Place>>(&path, &text)?);
        }
        let mut npcs = Vec::new();
        for (path, text) in load_list("npcs")? {
            npcs.extend(parse::<Vec<Npc>>(&path, &text)?);
        }
        let mut lore = Vec::new();
        for (path, text) in load_list("lore")? {
            lore.extend(parse::<Vec<Lore>>(&path, &text)?);
        }
        let mut music = Vec::new();
        for (path, text) in load_list("music")? {
            music.extend(parse::<Vec<Music>>(&path, &text)?);
        }
        let mut dialogues = Vec::new();
        for (path, text) in load_list("dialogues")? {
            dialogues.push(parse::<Dialogue>(&path, &text)?);
        }
        let mut quests = Vec::new();
        for (path, text) in load_list("quests")? {
            let file: QuestFile = parse(&path, &text)?;
            quests.push(Quest {
                id: file.id,
                title_key: file.title_key,
                start_stage: file.start_stage,
                stages: file
                    .stages
                    .into_iter()
                    .map(|(id, stage)| {
                        (
                            id,
                            Stage {
                                text_key: stage.text_key,
                                final_stage: stage.final_stage,
                                reach: stage.reach,
                            },
                        )
                    })
                    .collect(),
            });
        }
        let mut strings = BTreeMap::new();
        for language in &manifest.languages {
            ensure!(
                language.len() == 2 && language.chars().all(|c| c.is_ascii_lowercase()),
                "invalid language {language}"
            );
            let path = dir.join("i18n").join(format!("{language}.yaml"));
            ensure!(path.is_file(), "i18n/{language}.yaml is missing");
            let text = read_limited(&path, &mut hasher, dir)?;
            let table: BTreeMap<String, String> = parse(&path, &text)?;
            ensure!(
                table.values().all(|value| value.len() <= 2000),
                "{language}: a string is longer than 2000 bytes"
            );
            strings.insert(language.clone(), table);
        }
        // Only files that an NPC or a track names are shipped; they are part of the hash.
        let mut wanted: Vec<&str> = npcs.iter().filter_map(|n| n.portrait.as_deref()).collect();
        wanted.extend(music.iter().map(|m| m.file.as_str()));
        wanted.sort_unstable();
        wanted.dedup();
        let mut media = BTreeMap::new();
        for relative in wanted {
            ensure!(is_media_path(relative), "invalid media path {relative}");
            let path = dir.join(relative);
            let limit = if relative.ends_with(".ogg") {
                MAX_MUSIC_BYTES
            } else {
                MAX_PORTRAIT_BYTES
            };
            let meta = fs::symlink_metadata(&path)
                .with_context(|| format!("media file {relative} is missing"))?;
            ensure!(meta.is_file(), "media {relative} is not a regular file");
            ensure!(
                meta.len() <= limit,
                "media {relative} is larger than {limit} bytes"
            );
            let bytes = fs::read(&path)?;
            let digest = format!("{:x}", Sha256::digest(&bytes));
            hasher.update((relative.len() as u64).to_le_bytes());
            hasher.update(relative.as_bytes());
            hasher.update(digest.as_bytes());
            media.insert(relative.to_owned(), (path, digest));
        }
        let disk = Self {
            manifest,
            sha256: format!("{:x}", hasher.finalize()),
            places,
            npcs,
            dialogues,
            quests,
            lore,
            music,
            media,
            strings,
        };
        disk.check(allow_draft)?;
        Ok(disk)
    }

    fn check(&self, allow_draft: bool) -> Result<()> {
        let id = &self.manifest.id;
        let places: HashSet<&str> = unique(self.places.iter().map(|p| p.id.as_str()), "place")?;
        unique(self.npcs.iter().map(|n| n.id.as_str()), "npc")?;
        let dialogues: HashSet<&str> =
            unique(self.dialogues.iter().map(|d| d.id.as_str()), "dialogue")?;
        let quests: HashMap<&str, &Quest> =
            self.quests.iter().map(|q| (q.id.as_str(), q)).collect();
        ensure!(
            quests.len() == self.quests.len(),
            "{id}: duplicate quest id"
        );
        let local = |reference: &str| !reference.contains(':');
        let key_ok = |key: &str| -> Result<()> {
            for (language, table) in &self.strings {
                ensure!(
                    table.contains_key(key),
                    "{id}: text key {key} missing in {language}"
                );
            }
            Ok(())
        };
        for place in &self.places {
            ensure!(is_id(&place.id, 48), "{id}: invalid place id {}", place.id);
            key_ok(&place.name_key)?;
            if let Some(parent) = &place.parent {
                ensure!(
                    !local(parent) || places.contains(parent.as_str()),
                    "{id}: place {} has unknown parent {parent}",
                    place.id
                );
            }
            for near in &place.requires.near {
                ensure!(
                    !local(&near.place) || places.contains(near.place.as_str()),
                    "{id}: place {} is near unknown {}",
                    place.id,
                    near.place
                );
                ensure!(
                    near.min_m >= 0.0 && near.max_m > near.min_m && near.max_m.is_finite(),
                    "{id}: place {} has an invalid distance range",
                    place.id
                );
            }
            for biome in &place.requires.biome {
                ensure!(
                    BIOMES.iter().any(|(name, _)| name == biome),
                    "{id}: unknown biome {biome}"
                );
            }
            if let Some(scenery) = &place.scenery {
                ensure!(
                    matches!(scenery.preset.as_str(), "town" | "graveyard"),
                    "{id}: unknown scenery preset {}",
                    scenery.preset
                );
                ensure!(
                    scenery
                        .size
                        .as_deref()
                        .map_or(true, |size| ["hamlet", "village", "town"].contains(&size)),
                    "{id}: unknown settlement size"
                );
            }
            ensure!(
                matches!(
                    place.kind.as_str(),
                    "city" | "building" | "graveyard" | "landmark" | "camp"
                ),
                "{id}: unknown place kind {}",
                place.kind
            );
        }
        ensure!(
            self.places.iter().filter(|place| place.spawn).count() <= 1,
            "{id}: at most one place may be the spawn point"
        );
        // Parent chains must end.
        let parent_of: HashMap<&str, &str> = self
            .places
            .iter()
            .filter_map(|p| {
                p.parent
                    .as_deref()
                    .filter(|r| local(r))
                    .map(|r| (p.id.as_str(), r))
            })
            .collect();
        for place in &self.places {
            let mut current = place.id.as_str();
            for _ in 0..=self.places.len() {
                match parent_of.get(current) {
                    Some(next) => current = next,
                    None => break,
                }
            }
            ensure!(
                !parent_of.contains_key(current),
                "{id}: place parent cycle at {}",
                place.id
            );
        }
        for npc in &self.npcs {
            ensure!(is_id(&npc.id, 48), "{id}: invalid npc id {}", npc.id);
            key_ok(&npc.name_key)?;
            if let Some(bio) = &npc.bio_key {
                key_ok(bio)?;
            }
            ensure!(
                !local(&npc.place) || places.contains(npc.place.as_str()),
                "{id}: npc {} stands at unknown place {}",
                npc.id,
                npc.place
            );
            ensure!(
                !local(&npc.dialogue) || dialogues.contains(npc.dialogue.as_str()),
                "{id}: npc {} has unknown dialogue {}",
                npc.id,
                npc.dialogue
            );
            ensure!(
                matches!(
                    npc.character.pack.as_str(),
                    "protagonists" | "retro" | "survivors"
                ),
                "{id}: unknown character pack {}",
                npc.character.pack
            );
        }
        let tracks: HashSet<&str> = unique(self.music.iter().map(|m| m.id.as_str()), "music")?;
        for track in &self.music {
            key_ok(&track.title_key)?;
            ensure!(
                track.file.ends_with(".ogg"),
                "{id}: track {} must be an .ogg file",
                track.id
            );
        }
        for npc in &self.npcs {
            if let Some(portrait) = &npc.portrait {
                ensure!(
                    portrait.ends_with(".png") || portrait.ends_with(".jpg"),
                    "{id}: portrait {portrait} must be a png or jpg"
                );
            }
        }
        for dialogue in &self.dialogues {
            if let Some(track) = &dialogue.music {
                ensure!(
                    tracks.contains(track.as_str()),
                    "{id}: dialogue {} plays unknown music {track}",
                    dialogue.id
                );
            }
        }
        for entry in &self.lore {
            key_ok(&entry.title_key)?;
            key_ok(&entry.text_key)?;
        }
        for quest in self.quests.iter() {
            key_ok(&quest.title_key)?;
            ensure!(
                quest.stages.contains_key(&quest.start_stage),
                "{id}: quest {} has an unknown start stage",
                quest.id
            );
            for stage in quest.stages.values() {
                key_ok(&stage.text_key)?;
                if let Some(reach) = &stage.reach {
                    ensure!(
                        quest.stages.contains_key(&reach.goto),
                        "{id}: quest {} reaches unknown stage {}",
                        quest.id,
                        reach.goto
                    );
                    ensure!(
                        !local(&reach.place) || places.contains(reach.place.as_str()),
                        "{id}: quest {} reaches unknown place {}",
                        quest.id,
                        reach.place
                    );
                    ensure!(
                        reach.radius_m >= 5.0 && reach.radius_m <= 2000.0,
                        "{id}: quest {} has an invalid reach radius",
                        quest.id
                    );
                }
            }
        }
        let stage_ok = |reference: &QuestStageRef| -> Result<()> {
            if local(&reference.quest) {
                let quest = quests
                    .get(reference.quest.as_str())
                    .ok_or_else(|| anyhow!("{id}: unknown quest {}", reference.quest))?;
                ensure!(
                    quest.stages.contains_key(&reference.stage),
                    "{id}: quest {} has no stage {}",
                    reference.quest,
                    reference.stage
                );
            }
            Ok(())
        };
        for dialogue in &self.dialogues {
            if dialogue.status.as_deref() == Some("draft") {
                ensure!(
                    allow_draft,
                    "{id}: dialogue {} is an unreviewed draft",
                    dialogue.id
                );
            }
            ensure!(
                dialogue.nodes.contains_key(&dialogue.start),
                "{id}: dialogue {} has an unknown start node",
                dialogue.id
            );
            ensure!(
                dialogue.nodes.len() <= 200,
                "{id}: dialogue {} has too many nodes",
                dialogue.id
            );
            let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
            for (name, node) in &dialogue.nodes {
                ensure!(is_id(name, 48), "{id}: invalid node id {name}");
                let shape = (
                    node.text_key.is_some(),
                    node.branch.is_some(),
                    node.choices.is_some(),
                );
                ensure!(
                    matches!(shape, (true, false, _) | (false, true, false)),
                    "{id}: node {}/{name} must hold either text (with optional choices) or a branch",
                    dialogue.id
                );
                if let Some(text) = &node.text_key {
                    key_ok(text)?;
                }
                let mut targets = Vec::new();
                if let Some(choices) = &node.choices {
                    ensure!(
                        !choices.is_empty() && choices.len() <= 8,
                        "{id}: node {name} needs 1-8 choices"
                    );
                    for choice in choices {
                        key_ok(&choice.text_key)?;
                        if let Some(condition) = &choice.condition {
                            check_condition(condition, &stage_ok)?;
                        }
                        check_effects(&choice.effects, &stage_ok)?;
                        targets.extend(choice.goto.as_deref());
                    }
                }
                check_effects(&node.effects, &stage_ok)?;
                if let Some(branch) = &node.branch {
                    ensure!(!branch.is_empty(), "{id}: node {name} has an empty branch");
                    ensure!(
                        branch.last().is_some_and(|entry| entry.condition.is_none()),
                        "{id}: the last branch entry of {name} needs no condition"
                    );
                    for entry in branch {
                        if let Some(condition) = &entry.condition {
                            check_condition(condition, &stage_ok)?;
                        }
                        targets.push(&entry.goto);
                    }
                }
                for target in &targets {
                    ensure!(
                        dialogue.nodes.contains_key(*target),
                        "{id}: node {name} jumps to unknown node {target}"
                    );
                }
                edges.insert(name, targets);
            }
            let mut reachable = HashSet::new();
            let mut todo = vec![dialogue.start.as_str()];
            while let Some(current) = todo.pop() {
                if reachable.insert(current) {
                    todo.extend(edges[current].iter().copied());
                }
            }
            for name in dialogue.nodes.keys() {
                ensure!(
                    reachable.contains(name.as_str()),
                    "{id}: node {}/{name} is unreachable",
                    dialogue.id
                );
            }
            // A chain of branch nodes must reach a node with text.
            for (name, node) in &dialogue.nodes {
                if node.branch.is_none() {
                    continue;
                }
                let mut seen = HashSet::new();
                let mut current = name.as_str();
                while let Some(branch) = dialogue.nodes[current].branch.as_ref() {
                    ensure!(
                        seen.insert(current),
                        "{id}: branch cycle at {}/{name}",
                        dialogue.id
                    );
                    current = &branch.last().expect("checked above").goto;
                }
            }
        }
        Ok(())
    }
}

fn unique<'a>(ids: impl Iterator<Item = &'a str>, what: &str) -> Result<HashSet<&'a str>> {
    let mut set = HashSet::new();
    for id in ids {
        ensure!(set.insert(id), "duplicate {what} id {id}");
    }
    Ok(set)
}

fn check_condition(
    condition: &Condition,
    stage_ok: &dyn Fn(&QuestStageRef) -> Result<()>,
) -> Result<()> {
    let set = [
        condition.all.is_some(),
        condition.any.is_some(),
        condition.not.is_some(),
        condition.flag.is_some(),
        condition.quest_stage.is_some(),
        condition.has_item.is_some(),
        condition.gold_at_least.is_some(),
    ]
    .iter()
    .filter(|set| **set)
    .count();
    ensure!(set == 1, "a condition must have exactly one key");
    for child in condition
        .all
        .iter()
        .chain(condition.any.iter())
        .flatten()
        .chain(condition.not.as_deref())
    {
        check_condition(child, stage_ok)?;
    }
    if let Some(stage) = &condition.quest_stage {
        stage_ok(stage)?;
    }
    if let Some(item) = &condition.has_item {
        ensure!(
            (1..=1_000_000).contains(&item.quantity()),
            "invalid item quantity"
        );
    }
    if let Some(gold) = condition.gold_at_least {
        ensure!((0..=1_000_000_000).contains(&gold), "invalid gold amount");
    }
    Ok(())
}

fn check_effects(
    effects: &[Effect],
    stage_ok: &dyn Fn(&QuestStageRef) -> Result<()>,
) -> Result<()> {
    for effect in effects {
        let set = [
            effect.give_item.is_some(),
            effect.take_item.is_some(),
            effect.gold.is_some(),
            effect.set_flag.is_some(),
            effect.set_stage.is_some(),
            effect.once.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count();
        ensure!(set == 1, "an effect must have exactly one key");
        for item in [&effect.give_item, &effect.take_item].into_iter().flatten() {
            ensure!(
                (1..=1_000_000).contains(&item.quantity()),
                "invalid item quantity"
            );
        }
        if let Some(gold) = effect.gold {
            ensure!(
                gold != 0 && gold.abs() <= 1_000_000_000,
                "invalid gold amount"
            );
        }
        if let Some(stage) = &effect.set_stage {
            stage_ok(stage)?;
        }
        if let Some(once) = &effect.once {
            ensure!(is_id(&once.key, 48), "invalid once key {}", once.key);
            ensure!(
                once.effects.iter().all(|inner| inner.once.is_none()),
                "once effects cannot nest"
            );
            check_effects(&once.effects, stage_ok)?;
        }
    }
    Ok(())
}

pub fn parse_version(text: &str) -> Result<(u64, u64, u64)> {
    let parts: Vec<&str> = text.split('.').collect();
    if let [major, minor, patch] = parts[..] {
        if let (Ok(a), Ok(b), Ok(c)) = (major.parse(), minor.parse(), patch.parse()) {
            return Ok((a, b, c));
        }
    }
    bail!("invalid version {text}")
}

/// Orders disks by dependencies, then by id, and rejects missing dependencies,
/// conflicts, duplicates and incompatible versions.
pub fn order(disks: Vec<Disk>) -> Result<Vec<Disk>> {
    let mut by_id: BTreeMap<String, Disk> = BTreeMap::new();
    for disk in disks {
        let id = disk.manifest.id.clone();
        ensure!(
            by_id.insert(id.clone(), disk).is_none(),
            "datadisk {id} is selected twice"
        );
    }
    for disk in by_id.values() {
        let id = &disk.manifest.id;
        for requirement in &disk.manifest.requires {
            let other = by_id
                .get(&requirement.disk)
                .ok_or_else(|| anyhow!("datadisk {id} requires {}", requirement.disk))?;
            let have = parse_version(&other.manifest.version)?;
            let need = parse_version(&requirement.version)?;
            ensure!(
                have.0 == need.0 && have >= need,
                "datadisk {id} needs {} {} (same major)",
                requirement.disk,
                requirement.version
            );
        }
        for conflict in &disk.manifest.conflicts {
            ensure!(
                !by_id.contains_key(conflict),
                "datadisk {id} conflicts with {conflict}"
            );
        }
    }
    let mut ordered = Vec::new();
    let mut placed: BTreeSet<String> = BTreeSet::new();
    while ordered.len() < by_id.len() {
        let next = by_id
            .iter()
            .find(|(id, disk)| {
                !placed.contains(*id)
                    && disk
                        .manifest
                        .requires
                        .iter()
                        .all(|requirement| placed.contains(&requirement.disk))
            })
            .map(|(id, _)| id.clone())
            .ok_or_else(|| anyhow!("datadisk dependencies form a cycle"))?;
        placed.insert(next.clone());
        ordered.push(next);
    }
    Ok(ordered
        .into_iter()
        .map(|id| by_id.remove(&id).expect("listed above"))
        .collect())
}
