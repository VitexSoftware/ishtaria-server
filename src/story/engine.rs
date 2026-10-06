//! Pure evaluation of conditions, effects and dialogue trees against a snapshot of
//! a player's story state. Persistence and atomicity live in `db.rs`.

use super::disk::{Condition, Dialogue, Effect, Node};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

pub const GOLD: &str = "gold";
/// Branch nodes may chain; this bounds the walk even for data that slipped past validation.
const MAX_HOPS: usize = 64;

/// What the engine needs to know about one player.
#[derive(Clone, Debug, Default)]
pub struct PlayerState {
    pub flags: HashSet<String>,
    /// Qualified quest id -> current stage.
    pub stages: HashMap<String, String>,
    /// Item id -> quantity (gold is the item `gold`).
    pub items: HashMap<String, i64>,
    /// Qualified `once` keys already used.
    pub once_done: HashSet<String>,
}

impl PlayerState {
    fn owned(&self, item: &str) -> i64 {
        self.items.get(item).copied().unwrap_or(0)
    }
}

pub fn holds(condition: &Condition, state: &PlayerState) -> bool {
    if let Some(all) = &condition.all {
        return all.iter().all(|c| holds(c, state));
    }
    if let Some(any) = &condition.any {
        return any.iter().any(|c| holds(c, state));
    }
    if let Some(not) = &condition.not {
        return !holds(not, state);
    }
    if let Some(flag) = &condition.flag {
        return state.flags.contains(flag);
    }
    if let Some(stage) = &condition.quest_stage {
        return state.stages.get(&stage.quest) == Some(&stage.stage);
    }
    if let Some(item) = &condition.has_item {
        return state.owned(&item.item) >= item.quantity();
    }
    if let Some(gold) = condition.gold_at_least {
        return state.owned(GOLD) >= gold;
    }
    false
}

/// A missing condition always passes.
fn passes(condition: Option<&Condition>, state: &PlayerState) -> bool {
    match condition {
        Some(condition) => holds(condition, state),
        None => true,
    }
}

/// A change the database layer must apply atomically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Give(String, i64),
    Take(String, i64),
    Flag(String),
    Stage(String, String),
    /// Records that a `once` key was used (unique per player).
    Once(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reject {
    /// The player cannot pay for a `take_item` / negative `gold` effect.
    Insufficient,
}

/// Applies effects to the snapshot and records the operations. Nothing is committed here:
/// the caller discards `state` and `ops` if this returns an error.
pub fn apply(effects: &[Effect], state: &mut PlayerState, ops: &mut Vec<Op>) -> Result<(), Reject> {
    for effect in effects {
        if let Some(item) = &effect.give_item {
            let quantity = item.quantity();
            let total = state
                .owned(&item.item)
                .checked_add(quantity)
                .ok_or(Reject::Insufficient)?;
            state.items.insert(item.item.clone(), total);
            ops.push(Op::Give(item.item.clone(), quantity));
        }
        if let Some(item) = &effect.take_item {
            take(state, ops, &item.item, item.quantity())?;
        }
        if let Some(gold) = effect.gold {
            if gold > 0 {
                let total = state
                    .owned(GOLD)
                    .checked_add(gold)
                    .ok_or(Reject::Insufficient)?;
                state.items.insert(GOLD.to_owned(), total);
                ops.push(Op::Give(GOLD.to_owned(), gold));
            } else {
                take(
                    state,
                    ops,
                    GOLD,
                    gold.checked_neg().ok_or(Reject::Insufficient)?,
                )?;
            }
        }
        if let Some(flag) = &effect.set_flag {
            state.flags.insert(flag.clone());
            ops.push(Op::Flag(flag.clone()));
        }
        if let Some(stage) = &effect.set_stage {
            state
                .stages
                .insert(stage.quest.clone(), stage.stage.clone());
            ops.push(Op::Stage(stage.quest.clone(), stage.stage.clone()));
        }
        if let Some(once) = &effect.once {
            if state.once_done.insert(once.key.clone()) {
                ops.push(Op::Once(once.key.clone()));
                apply(&once.effects, state, ops)?;
            }
        }
    }
    Ok(())
}

fn take(
    state: &mut PlayerState,
    ops: &mut Vec<Op>,
    item: &str,
    quantity: i64,
) -> Result<(), Reject> {
    let left = state.owned(item) - quantity;
    if left < 0 {
        return Err(Reject::Insufficient);
    }
    state.items.insert(item.to_owned(), left);
    ops.push(Op::Take(item.to_owned(), quantity));
    Ok(())
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct VisibleChoice {
    /// Position in the node's choice list; this is what the client sends back.
    pub index: usize,
    pub text_key: String,
}

/// A node ready to show to the player.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Shown {
    pub node: String,
    pub text_key: String,
    /// Media URL of the spoken line in the language the client asked for, if the disk has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    pub choices: Vec<VisibleChoice>,
}

fn node<'a>(dialogue: &'a Dialogue, id: &str) -> Option<&'a Node> {
    dialogue.nodes.get(id)
}

/// Walks branch nodes from `id` to the first node with text, applying node effects on
/// the way. Returns the node to show.
pub fn enter(
    dialogue: &Dialogue,
    id: &str,
    state: &mut PlayerState,
    ops: &mut Vec<Op>,
) -> Result<Option<Shown>, Reject> {
    let mut current = id.to_owned();
    for _ in 0..MAX_HOPS {
        let Some(found) = node(dialogue, &current) else {
            return Ok(None);
        };
        apply(&found.effects, state, ops)?;
        if let Some(branch) = &found.branch {
            let Some(next) = branch
                .iter()
                .find(|entry| passes(entry.condition.as_ref(), state))
            else {
                return Ok(None);
            };
            current = next.goto.clone();
            continue;
        }
        let choices = found
            .choices
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, choice)| passes(choice.condition.as_ref(), state))
            .map(|(index, choice)| VisibleChoice {
                index,
                text_key: choice.text_key.clone(),
            })
            .collect();
        return Ok(Some(Shown {
            node: current,
            text_key: found.text_key.clone().unwrap_or_default(),
            voice: None,
            choices,
        }));
    }
    Ok(None)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Choose {
    /// The conversation continues at this node.
    Next(Shown),
    /// The conversation ended.
    End,
    /// The choice is not available (hidden by its condition, or out of range).
    Unavailable,
    /// The effects cannot be paid; nothing changes.
    Rejected(Reject),
}

/// Chooses `index` at `at`. On success `state` and `ops` hold the changes to commit.
pub fn choose(
    dialogue: &Dialogue,
    at: &str,
    index: usize,
    state: &mut PlayerState,
    ops: &mut Vec<Op>,
) -> Choose {
    let Some(choice) = node(dialogue, at).and_then(|n| n.choices.as_ref()?.get(index)) else {
        return Choose::Unavailable;
    };
    if choice.condition.as_ref().is_some_and(|c| !holds(c, state)) {
        return Choose::Unavailable;
    }
    if let Err(reject) = apply(&choice.effects, state, ops) {
        return Choose::Rejected(reject);
    }
    let Some(next) = &choice.goto else {
        return Choose::End;
    };
    match enter(dialogue, next, state, ops) {
        Ok(Some(shown)) => Choose::Next(shown),
        Ok(None) => Choose::End,
        Err(reject) => Choose::Rejected(reject),
    }
}
