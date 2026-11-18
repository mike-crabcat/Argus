//! Floor plans for Argus: levels with walls, doors and windows, rooms,
//! dimensions, cameras and a background image, in millimetres.
//!
//! A plan only changes through [`Command`]s, applied with [`Plan::apply`],
//! which returns the inverse command (undo). The editor, the server and
//! later an import agent all speak these commands; they are JSON, with a
//! schema from [`command_schema`] and a guide in `docs/plan-commands.md`.
//!
//! No UI here: this crate builds for native and wasm alike.

pub mod command;
pub mod context;
pub mod fixture;
pub mod geom;
pub mod model;
pub mod outline;
pub mod rooms;
pub mod snap;
pub mod validate;

pub use command::{Command, End, PlanError, command_schema};
pub use geom::{Point, pt};
pub use model::*;

/// What `/api/plan` serves: the plan and its version, which goes up by one
/// with every change.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Versioned {
    pub version: u64,
    pub plan: Plan,
}

/// The body of `POST /api/plan/commands`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandBatch {
    /// The version these commands were made against. If given and the plan
    /// has moved on, nothing is applied (HTTP 409). Leave it out to apply
    /// to whatever is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    pub commands: Vec<Command>,
}
