//! Argus's own object detection: decoding camera streams to pictures
//! ([`decode`]) and finding people, vehicles and animals in them
//! ([`model`]).

pub mod decode;
pub mod model;
pub mod pose;
pub mod testing;
pub mod track;
