//! The rotation engine: what a bot casts in a fight, and which skills it learns.
//!
//! * [`model`] compiles `data/rotations/*.json` once into an immutable [`Rotation`] that all bots
//!   of an archetype share through an `Arc` (see [`Archetype::rotation`](crate::archetype::Archetype::rotation)).
//! * [`eval`] picks the skill for one tick from the facts of the moment, with a tiny per-bot
//!   [`RotState`] for combo, chain, group cooldowns and the buffs seen so far.
//! * [`learn`] learns the archetype's skills as the level grows and adds its second and third
//!   ability.
//!
//! The combat controller ([`Combat`](super::combat::Combat)) owns the per-bot state, gathers the
//! [`Inputs`] and sends what [`decide`] returns.

pub mod eval;
pub mod learn;
pub mod model;

pub use eval::{
    aim_of, decide, gate, reach_of, Aim, Cast, CastKind, Decision, Gate, Inputs, PartyView,
    RotState, SkillBook, SkillFacts, TargetView, GCD_MS, MELEE_REACH, RANGE_SLACK,
};
pub use learn::{LearnCtx, Learner};
pub use model::Rotation;
