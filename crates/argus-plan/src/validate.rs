//! Problems in a plan worth showing: commands keep plans consistent, but a
//! moved node can leave an opening hanging off its wall, walls can cross,
//! and so on.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::geom::segments_cross;
use crate::model::{Id, Plan};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Issue {
    pub level: Id,
    /// The entities involved.
    pub ids: Vec<Id>,
    pub message: String,
}

pub fn validate(p: &Plan) -> Vec<Issue> {
    let mut out = Vec::new();
    for l in &p.levels {
        let mut issue = |ids: Vec<Id>, message: String| {
            out.push(Issue {
                level: l.id,
                ids,
                message,
            })
        };
        for w in &l.walls {
            match l.wall_ends(w) {
                None => issue(vec![w.id], format!("wall {} has a missing end", w.id)),
                Some((a, b)) if a.dist(b) < 1.0 => {
                    issue(vec![w.id], format!("wall {} has no length", w.id))
                }
                _ => {}
            }
        }
        for (i, w) in l.walls.iter().enumerate() {
            let Some((a, b)) = l.wall_ends(w) else {
                continue;
            };
            for v in &l.walls[i + 1..] {
                if [v.a, v.b].contains(&w.a) || [v.a, v.b].contains(&w.b) {
                    continue;
                }
                let Some((c, d)) = l.wall_ends(v) else {
                    continue;
                };
                if segments_cross(a, b, c, d) {
                    issue(
                        vec![w.id, v.id],
                        format!("walls {} and {} cross without a joint", w.id, v.id),
                    );
                }
            }
        }
        for o in &l.openings {
            let Some(w) = l.wall(o.wall) else {
                issue(vec![o.id], format!("opening {} is in no wall", o.id));
                continue;
            };
            let len = l.wall_len(w);
            if o.offset - o.width / 2.0 < -1e-6 || o.offset + o.width / 2.0 > len + 1e-6 {
                issue(
                    vec![o.id, w.id],
                    format!("opening {} runs past the end of wall {}", o.id, w.id),
                );
            }
        }
        for (i, o) in l.openings.iter().enumerate() {
            for q in &l.openings[i + 1..] {
                if q.wall == o.wall
                    && (o.offset - q.offset).abs() < (o.width + q.width) / 2.0 - 1e-6
                {
                    issue(
                        vec![o.id, q.id],
                        format!("openings {} and {} overlap", o.id, q.id),
                    );
                }
            }
        }
        for r in &l.rooms {
            if crate::rooms::room_polygon(l, r).is_none() {
                issue(
                    vec![r.id],
                    format!("room \"{}\" isn't enclosed by walls", r.name),
                );
            }
        }
    }
    let mut names: Vec<(&str, Id)> = p
        .levels
        .iter()
        .flat_map(|l| l.cameras.iter().map(|c| (c.camera.as_str(), c.id)))
        .collect();
    names.sort();
    for w in names.windows(2) {
        if w[0].0 == w[1].0 {
            out.push(Issue {
                level: p.level_of(w[1].1).map_or(Id(0), |l| l.id),
                ids: vec![w[0].1, w[1].1],
                message: format!("camera \"{}\" is placed twice", w[0].0),
            });
        }
    }
    out
}
