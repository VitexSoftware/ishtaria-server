use super::engine::{self, Choose, Op, PlayerState};
use super::placement::Ground;
use super::{disk::Disk, Story};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let number = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ishtaria-story-{}-{number}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn base(root: &Path, id: &str, version: &str, extra: &str) {
    write(
        root,
        "datadisk.yaml",
        &format!(
            "id: {id}\nversion: {version}\nname: Test\nrequires_ruleset: \"1\"\nlicense: CC0-1.0\nattribution: Test\nlanguages: [en, cs]\n{extra}"
        ),
    );
    write(
        root,
        "i18n/en.yaml",
        "place.hut: Hut\nnpc.hermit: Hermit\nhi: Hi\nbye: Bye\nq: Quest\nq.s1: One\nq.s2: Two\n",
    );
    write(root, "i18n/cs.yaml", "place.hut: Chata\nnpc.hermit: Poustevnik\nhi: Ahoj\nbye: Nashle\nq: Ukol\nq.s1: Jedna\nq.s2: Dva\n");
    write(
        root,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut}\n",
    );
    write(
        root,
        "npcs/n.yaml",
        "- {id: hermit, name_key: npc.hermit, place: hut, character: {pack: retro, skin: humanMaleA}, dialogue: hermit}\n",
    );
    write(
        root,
        "quests/q.yaml",
        "id: q\ntitle_key: q\nstart_stage: s1\nstages:\n  s1: {text_key: q.s1}\n  s2: {text_key: q.s2, final: true}\n",
    );
    write(
        root,
        "dialogues/hermit.yaml",
        "id: hermit\nstart: hi\nnodes:\n  hi:\n    text_key: hi\n    choices:\n      - {text_key: bye, if: {gold_at_least: 5}, effects: [{gold: -5}, {once: {key: reward, effects: [{gold: 20}, {set_stage: {quest: q, stage: s2}}]}}, {set_flag: met}]}\n      - {text_key: bye, if: {not: {flag: met}}}\n",
    );
}

#[test]
fn composes_disks_in_dependency_order_with_qualified_ids() {
    let a = scratch("a");
    let b = scratch("b");
    base(&a, "zeta", "1.2.0", "");
    base(
        &b,
        "alpha",
        "1.0.0",
        "requires:\n  - {disk: zeta, version: 1.0.0}\n",
    );
    let story = Story::load(&[b.clone(), a.clone()], false).unwrap();
    let order: Vec<&str> = story.disks.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(order, ["zeta", "alpha"], "dependencies come first");
    assert!(story.npcs.contains_key("zeta:hermit") && story.npcs.contains_key("alpha:hermit"));
    assert_eq!(story.strings["en"]["zeta:place.hut"], "Hut");
    assert_eq!(story.strings["cs"]["alpha:npc.hermit"], "Poustevnik");
    assert_eq!(story.npcs["alpha:hermit"].dialogue, "alpha:hermit");
    assert_eq!(story.disks[0].2.len(), 64);
}

#[test]
fn announces_the_cover_of_a_disk_and_rejects_a_missing_one() {
    let a = scratch("cover");
    base(&a, "covered", "1.0.0", "cover: media/cover/art.jpg\n");
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "a cover that is not shipped must be rejected"
    );
    write(&a, "media/cover/art.jpg", "jpeg bytes");
    let story = Story::load(&[a], false).unwrap();
    assert_eq!(story.infos.len(), 1);
    assert_eq!(story.infos[0].id, "covered");
    assert_eq!(story.infos[0].name, "Test");
    assert_eq!(
        story.infos[0].cover.as_deref(),
        Some("/story/media/covered/media/cover/art.jpg")
    );
    assert!(story.media.contains_key("covered/media/cover/art.jpg"));
}

#[test]
fn an_npc_model_is_shipped_validated_and_announced() {
    let a = scratch("model");
    base(&a, "zeta", "1.0.0", "");
    let with_model = |model: &str| {
        write(
            &a,
            "npcs/n.yaml",
            &format!("- {{id: hermit, name_key: npc.hermit, place: hut, character: {{pack: retro, skin: humanMaleA, model: {model}}}, dialogue: hermit}}\n"),
        );
    };
    with_model("media/models/hermit.glb");
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "a model that is not shipped must be rejected"
    );
    write(&a, "media/models/hermit.glb", "glTF");
    let story = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert!(story.media.contains_key("zeta/media/models/hermit.glb"));
    let big = vec![0u8; (super::disk::MAX_MODEL_BYTES + 1) as usize];
    fs::write(a.join("media/models/hermit.glb"), big).unwrap();
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "oversized model"
    );
    fs::write(a.join("media/models/hermit.glb"), "glTF").unwrap();
    write(&a, "media/other/hermit.glb", "glTF");
    with_model("media/other/hermit.glb");
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "models belong below media/models/"
    );
}

#[test]
fn spoken_lines_are_found_by_language_and_text_key_and_limited_in_size() {
    let a = scratch("voice");
    base(&a, "zeta", "1.0.0", "");
    write(&a, "media/voice/cs/hi.ogg", "OggS");
    write(&a, "media/voice/cs/unused.ogg", "OggS");
    let story = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert_eq!(
        story.voices[&("cs".to_owned(), "zeta:hi".to_owned())],
        "zeta/media/voice/cs/hi.ogg"
    );
    assert!(
        !story
            .voices
            .contains_key(&("en".to_owned(), "zeta:hi".to_owned())),
        "no English recording"
    );
    assert!(
        story.media.contains_key("zeta/media/voice/cs/hi.ogg")
            && !story.media.contains_key("zeta/media/voice/cs/unused.ogg"),
        "only lines a node shows are served"
    );
    let big = vec![0u8; (super::disk::MAX_VOICE_BYTES + 1) as usize];
    fs::write(a.join("media/voice/cs/hi.ogg"), big).unwrap();
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "oversized line"
    );
}

#[test]
fn rejects_missing_dependencies_conflicts_and_drafts() {
    let a = scratch("dep");
    base(
        &a,
        "needy",
        "1.0.0",
        "requires:\n  - {disk: ghost, version: 1.0.0}\n",
    );
    assert!(Story::load(std::slice::from_ref(&a), false)
        .unwrap_err()
        .to_string()
        .contains("requires ghost"));

    let b = scratch("c1");
    let c = scratch("c2");
    base(&b, "one", "1.0.0", "conflicts: [two]\n");
    base(&c, "two", "1.0.0", "");
    assert!(Story::load(&[b, c], false)
        .unwrap_err()
        .to_string()
        .contains("conflicts"));

    let d = scratch("old");
    let e = scratch("new");
    base(&d, "lib", "1.0.0", "");
    base(
        &e,
        "app",
        "1.0.0",
        "requires:\n  - {disk: lib, version: 2.0.0}\n",
    );
    assert!(Story::load(&[d, e], false)
        .unwrap_err()
        .to_string()
        .contains("same major"));

    let f = scratch("draft");
    base(&f, "drafty", "1.0.0", "");
    let text = fs::read_to_string(f.join("dialogues/hermit.yaml")).unwrap();
    write(
        &f,
        "dialogues/hermit.yaml",
        &text.replace("start: hi", "status: draft\nstart: hi"),
    );
    assert!(Disk::load(&f, false)
        .unwrap_err()
        .to_string()
        .contains("draft"));
    assert!(Disk::load(&f, true).is_ok());
}

#[test]
fn rejects_broken_references_and_cross_disk_leaks() {
    let a = scratch("goto");
    base(&a, "broken", "1.0.0", "");
    let text = fs::read_to_string(a.join("dialogues/hermit.yaml")).unwrap();
    write(
        &a,
        "dialogues/hermit.yaml",
        &text.replace(
            "{text_key: bye, if: {not: {flag: met}}}",
            "{text_key: bye, goto: nowhere}",
        ),
    );
    assert!(Disk::load(&a, false)
        .unwrap_err()
        .to_string()
        .contains("unknown node"));

    let b = scratch("leak");
    base(&b, "leaky", "1.0.0", "");
    write(&b, "npcs/n.yaml", "- {id: hermit, name_key: npc.hermit, place: \"other:hut\", character: {pack: retro, skin: humanMaleA}, dialogue: hermit}\n");
    let error = Story::load(&[b], false).unwrap_err().to_string();
    assert!(error.contains("not a declared dependency"), "{error}");
}

fn dialogue_story() -> Story {
    let a = scratch("play");
    base(&a, "play", "1.0.0", "");
    Story::load(&[a], false).unwrap()
}

#[test]
fn choices_apply_effects_once_and_hide_unavailable_options() {
    let story = dialogue_story();
    let dialogue = &story.dialogues["play:hermit"];
    let mut state = PlayerState::default();
    state.items.insert("gold".into(), 100);

    let mut ops = Vec::new();
    let shown = engine::enter(dialogue, "hi", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(shown.choices.len(), 2);

    let result = engine::choose(dialogue, "hi", 0, &mut state, &mut ops);
    assert_eq!(result, Choose::End);
    assert_eq!(state.items["gold"], 115, "-5 then +20 once");
    assert_eq!(state.stages["play:q"], "s2");
    assert!(ops.contains(&Op::Once("play:reward".into())));

    // Repeating the choice pays 5 again but grants the once-reward only the first time.
    let mut again = Vec::new();
    assert_eq!(
        engine::choose(dialogue, "hi", 0, &mut state, &mut again),
        Choose::End
    );
    assert_eq!(state.items["gold"], 110);
    assert!(!again.iter().any(|op| matches!(op, Op::Once(_))));

    // The second choice is hidden once the flag is set.
    let mut ops = Vec::new();
    let shown = engine::enter(dialogue, "hi", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(
        shown.choices.iter().map(|c| c.index).collect::<Vec<_>>(),
        [0]
    );
    assert_eq!(
        engine::choose(dialogue, "hi", 1, &mut state, &mut ops),
        Choose::Unavailable
    );
    assert_eq!(
        engine::choose(dialogue, "hi", 9, &mut state, &mut ops),
        Choose::Unavailable
    );
}

#[test]
fn unaffordable_effects_change_nothing() {
    let story = dialogue_story();
    let dialogue = &story.dialogues["play:hermit"];
    let mut state = PlayerState::default();
    state.items.insert("gold".into(), 3);
    let mut ops = Vec::new();
    // Choice 0 needs 5 gold to be visible at all.
    assert_eq!(
        engine::choose(dialogue, "hi", 0, &mut state, &mut ops),
        Choose::Unavailable
    );
    assert_eq!(state.items["gold"], 3);
    assert!(ops.is_empty());
}

#[test]
fn the_endland_disk_is_valid_and_its_graveyard_job_plays_through() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], false).unwrap();
    assert!(story
        .anchors
        .iter()
        .any(|a| a.id == "endland:old_graveyard"));
    let mut state = PlayerState::default();
    state.items.insert("gold".into(), 100);
    let mut ops = Vec::new();

    let faust = &story.dialogues["endland:faust"];
    let shown = engine::enter(faust, "route", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(shown.node, "greet");
    engine::choose(faust, "greet", 0, &mut state, &mut ops); // ask for work
    engine::choose(faust, "work", 0, &mut state, &mut ops); // sent to Fawn
    assert_eq!(state.stages["endland:graveyard_job"], "heard_rumour");

    let fawn = &story.dialogues["endland:fawn"];
    let shown = engine::enter(fawn, "route", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(shown.node, "offer");
    assert!(matches!(
        engine::choose(fawn, "offer", 0, &mut state, &mut ops),
        Choose::Next(_)
    ));
    assert_eq!(state.items["gold"], 90);
    assert_eq!(state.stages["endland:graveyard_job"], "have_key");

    let shown = engine::enter(faust, "route", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(shown.node, "has_key");
    assert!(matches!(
        engine::choose(faust, "has_key", 0, &mut state, &mut ops),
        Choose::Next(_)
    ));
    assert_eq!(state.items["gold"], 115);
    assert_eq!(state.items["cheese"], 2);
    assert_eq!(state.stages["endland:graveyard_job"], "paid_off");
}

/// A planet whose northern hemisphere is gentle grassland, the rest sea.
struct Flat;

impl super::placement::Ground for Flat {
    fn land_site(&self, d: [f64; 3]) -> Option<(u8, f64)> {
        (d[1] > 0.1).then_some((if d[0] > 0.0 { 4 } else { 5 }, 50.0 + 30.0 * d[1]))
    }
    fn height(&self, d: [f64; 3]) -> f64 {
        50.0 + 30.0 * d[1]
    }
    fn is_sea(&self, d: [f64; 3]) -> bool {
        d[1] <= 0.1
    }
}

#[test]
fn placement_is_deterministic_and_honours_distances() {
    use super::placement::{distance_m, place_anchors};
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], false).unwrap();
    let first = place_anchors(&story, &Flat, "42", &[]).unwrap();
    assert_eq!(first, place_anchors(&story, &Flat, "42", &[]).unwrap());
    assert_ne!(first, place_anchors(&story, &Flat, "43", &[]).unwrap());
    let site = |id: &str| first.iter().find(|p| p.id == id).unwrap();
    let city = site("endland:udrury");
    let tavern = site("endland:blackhorn_tavern");
    let graveyard = site("endland:old_graveyard");
    assert!(distance_m(city.direction, tavern.direction) <= 400.0 * 0.8 * 1.0001);
    let away = distance_m(city.direction, graveyard.direction);
    assert!(
        (1000.0..=3000.0).contains(&away),
        "graveyard is {away} m from the city"
    );

    // An already placed place never moves; only the rest is added.
    let kept = place_anchors(&story, &Flat, "42", &first[..1]).unwrap();
    assert_eq!(kept, first[1..]);
}

#[test]
fn placement_fails_clearly_when_nothing_fits() {
    use super::placement::place_anchors;
    struct Sea;
    impl super::placement::Ground for Sea {
        fn land_site(&self, _: [f64; 3]) -> Option<(u8, f64)> {
            None
        }
        fn height(&self, _: [f64; 3]) -> f64 {
            0.0
        }
        fn is_sea(&self, _: [f64; 3]) -> bool {
            true
        }
    }
    let story = dialogue_story();
    let error = place_anchors(&story, &Sea, "1", &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("play:hut"), "{error}");
}

#[test]
fn the_endland_intro_asks_the_name_and_hands_out_the_aetherglass_once() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], false).unwrap();
    let faust = &story.dialogues["endland:faust_intro"];
    for name_choice in 0..2 {
        let mut state = PlayerState::default();
        let mut ops = Vec::new();
        let shown = engine::enter(faust, "route", &mut state, &mut ops)
            .unwrap()
            .unwrap();
        assert_eq!(shown.node, "wake");
        assert_eq!(state.stages["endland:intro"], "woke");
        assert_eq!(shown.choices.len(), 4, "four ways to answer the wake-up");
        engine::choose(faust, "wake", 3, &mut state, &mut ops); // I wish I had stayed dead
        engine::choose(faust, "wish_dead", 0, &mut state, &mut ops);
        assert!(matches!(
            engine::choose(faust, "ask_name", name_choice, &mut state, &mut ops),
            Choose::Next(_)
        ));
        assert_eq!(state.stages["endland:intro"], "named");
        let reply = ["refused", "told"][name_choice];
        engine::choose(faust, reply, 0, &mut state, &mut ops);
        engine::choose(faust, "aether", 1, &mut state, &mut ops);
        assert_eq!(state.items["aetherglass"], 1);
        assert_eq!(state.stages["endland:intro"], "got_aether");
        // Meeting him again only gives directions, and never a second tablet.
        let shown = engine::enter(faust, "route", &mut state, &mut ops)
            .unwrap()
            .unwrap();
        assert_eq!(shown.node, "after");
        let veil_first = engine::choose(faust, "after", 0, &mut state, &mut ops);
        assert_eq!(veil_first, Choose::End);
        assert_eq!(state.items["aetherglass"], 1);
    }
}

#[test]
fn every_world_grows_towns_with_graveyards_and_harbours_with_shipwrights() {
    use super::{placement::place_anchors, settlements};
    let disk = settlements::world_disk("42", 6);
    assert_eq!(
        disk.places.len(),
        13,
        "a town and a graveyard each, and a fortress for every fourth town"
    );
    let mut story = Story::compose(vec![disk]).unwrap();
    let mut sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    assert_eq!(sites.len(), 13);
    let built = settlements::finish(&mut story, &mut sites, &Flat, "42").unwrap();
    let props = &built.props;
    assert!(
        built.colliders.len() > 100 && built.colliders.iter().all(|c| c.collision_radius_m > 0.0)
    );
    assert!(props.iter().any(|p| p.model == "graveyard.crypt-large"));
    assert!(props.iter().any(|p| p.model == "town.fountain-round"));
    assert!(props.iter().any(|p| p.model == "quaternius.castle_gate"));
    assert!(props.iter().any(|p| p.model == "quaternius.temple"));
    assert_eq!(
        story.strings["cs"]["world:fort_00.name"].split(' ').next(),
        Some("Pevnost")
    );
    let shipwrights: Vec<_> = story
        .npcs
        .keys()
        .filter(|id| id.contains("shipwright"))
        .collect();
    assert!(!shipwrights.is_empty(), "coastal towns have a shipwright");
    assert!(story.dialogues.contains_key("harbours:shipwright"));
    assert_eq!(story.strings["cs"]["harbours:shipwright.name"], "Loďmistr");
    // The shop is plain dialogue: ships for gold, hidden while the player is poor.
    let dialogue = &story.dialogues["harbours:shipwright"];
    let mut poor = PlayerState::default();
    poor.items.insert("gold".into(), 100);
    let shown = engine::enter(dialogue, "greet", &mut poor, &mut Vec::new())
        .unwrap()
        .unwrap();
    assert_eq!(shown.choices.len(), 2, "a small boat and leaving");
    let mut rich = PlayerState::default();
    rich.items.insert("gold".into(), 1000);
    let mut ops = Vec::new();
    assert!(matches!(
        engine::choose(dialogue, "greet", 0, &mut rich, &mut ops),
        Choose::Next(_)
    ));
    assert_eq!(rich.items["gold"], 940);
    assert_eq!(rich.items["boat_row_small"], 1);
}

#[test]
fn the_spawn_point_of_a_disk_is_a_free_spot_inside_its_place() {
    use super::{
        placement::place_anchors,
        settlements,
        world::{find_spawn, npc_descriptors},
    };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let mut story = Story::load(&[path], false).unwrap();
    let mut sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    let built = settlements::finish(&mut story, &mut sites, &Flat, "42").unwrap();
    let npcs = npc_descriptors(&story, &sites, &Flat);
    // The model of Faust is announced under the id of its disk, like his portrait, or the client
    // asks for a file that does not exist and keeps the pack character.
    let faust = npcs
        .iter()
        .find(|npc| npc.id == "endland:faust_graveyard")
        .expect("Faust stands in the graveyard");
    assert_eq!(
        faust.model.as_deref(),
        Some("/story/media/endland/media/models/faust.glb")
    );
    assert_eq!(
        faust.portrait.as_deref(),
        Some("/story/media/endland/media/portraits/faust.jpg")
    );
    let spawn =
        find_spawn(&story, &sites, &built.colliders, &npcs, &Flat, "42").expect("a spawn place");
    let distance =
        |a: [f64; 3], b: [f64; 3]| (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt();
    let site = &sites["endland:old_graveyard"];
    let centre = crate::scenery::world_from_local(
        site.direction,
        0.0,
        0.0,
        |d| Flat.height(d).max(0.0),
        crate::movement::RADIUS,
    )
    .1;
    assert!(
        distance(spawn, centre) < 13.0,
        "inside the fence: {} m",
        distance(spawn, centre)
    );
    for obstacle in &built.colliders {
        assert!(
            distance(spawn, obstacle.position) > obstacle.collision_radius_m + 1.0,
            "clear of {}",
            obstacle.id
        );
    }
    for npc in &npcs {
        assert!(distance(spawn, npc.position) > 1.9, "clear of {}", npc.id);
    }
    // The same world always gives the same spot.
    assert_eq!(
        Some(spawn),
        find_spawn(&story, &sites, &built.colliders, &npcs, &Flat, "42")
    );
}
