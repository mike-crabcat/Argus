//! Tracking: links one camera's detections from picture to picture into
//! tracks, and says when something is *moving*.
//!
//! Matching is greedy by box overlap (IoU), best pairs first, within a
//! label. A track is confirmed once it has been seen in `min_hits`
//! pictures and is lost after `max_gap` without a sighting.
//!
//! Parked cars and bicycles in the garage are detected in every picture,
//! so a track on its own is no event. A track counts as moving when its
//! box centre gets far enough from where it last settled (a share of the
//! box's size, so it works near and far), and as still again once it
//! hasn't moved for `hold`. Events are made from the moving spells.

/// A box: left, top, right, bottom in pixels.
pub type BBox = [f32; 4];

pub fn iou(a: &BBox, b: &BBox) -> f32 {
    let ix = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let iy = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = ix * iy;
    let area = |r: &BBox| (r[2] - r[0]).max(0.0) * (r[3] - r[1]).max(0.0);
    let union = area(a) + area(b) - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// How near `b`'s centre is to `a`'s, 0..1 (1: same place), if within
/// `jump` times `a`'s larger side, and the boxes are of a similar size.
fn nearness(a: &BBox, b: &BBox, jump: f32) -> Option<f32> {
    let (ax, ay) = centre(a);
    let (bx, by) = centre(b);
    let size = |r: &BBox| (r[2] - r[0]).max(r[3] - r[1]).max(1.0);
    let (sa, sb) = (size(a), size(b));
    if sa.max(sb) / sa.min(sb) > 2.0 {
        return None;
    }
    let d = ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt() / (jump * sa);
    (d < 1.0).then_some(1.0 - d)
}

fn centre(b: &BBox) -> (f32, f32) {
    ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0)
}

/// One detection, as the tracker takes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    pub label: &'static str,
    pub class: &'static str,
    pub score: f32,
    pub bbox: BBox,
}

#[derive(Clone, Debug)]
pub struct Params {
    /// Least overlap for a detection to continue a track.
    pub min_iou: f32,
    /// Or, failing that, how far it may have gone (times the box's size).
    pub max_jump: f32,
    /// A track must have been seen this sure at least once to count as
    /// moving: unsure boxes flicker between neighbours (bicycles hung on a
    /// garage wall), which looks like movement.
    pub min_moving_score: f32,
    /// Pictures a moving spell needs before it counts.
    pub min_moving_hits: u32,
    /// Sightings before a track is real.
    pub min_hits: u32,
    /// A track unseen this long is over (µs).
    pub max_gap_us: i64,
    /// How far the centre must go, as a share of the box's larger side, to
    /// count as moving.
    pub move_share: f32,
    /// ...and at least this many pixels (jitter on small, far boxes).
    pub move_min_px: f32,
    /// A moving track that hasn't moved for this long is still again (µs).
    pub hold_us: i64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            min_iou: 0.3,
            max_jump: 2.5,
            min_moving_score: 0.6,
            min_moving_hits: 3,
            min_hits: 3,
            max_gap_us: 2_000_000,
            move_share: 0.25,
            move_min_px: 12.0,
            hold_us: 5_000_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub id: u64,
    pub label: &'static str,
    pub class: &'static str,
    pub first_us: i64,
    pub last_us: i64,
    pub bbox: BBox,
    pub best_score: f32,
    pub hits: u32,
    /// Where it last settled, and when it last moved (None: still).
    anchor: (f32, f32),
    pub moved_us: Option<i64>,
    /// When the current moving spell started (once it has been reported).
    pub moving_since_us: Option<i64>,
    /// A spell not yet reported: when it started, where from, and how many
    /// pictures it has had.
    pending: Option<(i64, (f32, f32), u32)>,
}

impl Track {
    pub fn confirmed(&self, p: &Params) -> bool {
        self.hits >= p.min_hits
    }
    pub fn moving(&self) -> bool {
        self.moving_since_us.is_some()
    }

    fn centre(&self) -> (f32, f32) {
        centre(&self.bbox)
    }
}

/// What changed with one update.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// A confirmed track started moving (a new event).
    Moving(Track),
    /// A moving track was seen again (extend its event to `last_us`).
    StillMoving(Track),
    /// A moving track stopped, or was lost; `last_us` is when it last moved.
    Stopped(Track),
    /// A confirmed track is over (lost from view).
    Ended(Track),
}

pub struct Tracker {
    pub params: Params,
    tracks: Vec<Track>,
    next_id: u64,
}

impl Tracker {
    pub fn new(params: Params) -> Self {
        Self {
            params,
            tracks: Vec::new(),
            next_id: 1,
        }
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Feeds one picture's detections, taken at `now_us`.
    pub fn update(&mut self, now_us: i64, seen: &[Seen]) -> Vec<Change> {
        let p = self.params.clone();
        let mut out = Vec::new();
        // Candidate pairs, best overlap first.
        let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            for (si, s) in seen.iter().enumerate() {
                if s.label == t.label {
                    let o = iou(&t.bbox, &s.bbox);
                    if o >= p.min_iou {
                        pairs.push((1.0 + o, ti, si));
                    } else if let Some(near) = nearness(&t.bbox, &s.bbox, p.max_jump) {
                        // A fast, small thing (a car passing far off) can
                        // move further than its own width between
                        // pictures: match by distance, after any overlap.
                        pairs.push((near, ti, si));
                    }
                }
            }
        }
        pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut track_used = vec![false; self.tracks.len()];
        let mut seen_used = vec![false; seen.len()];
        for (_, ti, si) in pairs {
            if track_used[ti] || seen_used[si] {
                continue;
            }
            track_used[ti] = true;
            seen_used[si] = true;
            let s = &seen[si];
            let t = &mut self.tracks[ti];
            t.bbox = s.bbox;
            t.last_us = now_us;
            t.hits += 1;
            if s.score > t.best_score {
                t.best_score = s.score;
                t.class = s.class;
            }
            let (cx, cy) = centre(&s.bbox);
            let size = (s.bbox[2] - s.bbox[0]).max(s.bbox[3] - s.bbox[1]);
            let far = (p.move_share * size).max(p.move_min_px);
            let d = ((cx - t.anchor.0).powi(2) + (cy - t.anchor.1).powi(2)).sqrt();
            let from = t.anchor;
            if d >= far {
                t.anchor = (cx, cy);
                t.moved_us = Some(now_us);
            }
            if !t.confirmed(&p) || t.best_score < p.min_moving_score {
                continue;
            }
            let active = t.moved_us.is_some_and(|m| now_us - m < p.hold_us);
            if t.moving() {
                if active {
                    out.push(Change::StillMoving(t.clone()));
                } else {
                    t.moving_since_us = None;
                    let mut stopped = t.clone();
                    stopped.last_us = t.moved_us.unwrap_or(t.last_us);
                    out.push(Change::Stopped(stopped));
                }
                continue;
            }
            if !active {
                t.pending = None;
                continue;
            }
            // A spell is reported once it has lasted a few pictures and got
            // somewhere: a box flickering between two places never does.
            let (since, origin, n) = match t.pending {
                Some((since, origin, n)) => (since, origin, n + 1),
                None => (t.moved_us.unwrap_or(now_us), from, 1),
            };
            let (cx, cy) = t.centre();
            let gone = ((cx - origin.0).powi(2) + (cy - origin.1).powi(2)).sqrt();
            if n >= p.min_moving_hits && gone >= far {
                t.pending = None;
                t.moving_since_us = Some(since);
                out.push(Change::Moving(t.clone()));
            } else {
                t.pending = Some((since, origin, n));
            }
        }
        // New tracks for detections nothing continued.
        for (si, s) in seen.iter().enumerate() {
            if seen_used[si] {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            self.tracks.push(Track {
                id,
                label: s.label,
                class: s.class,
                first_us: now_us,
                last_us: now_us,
                bbox: s.bbox,
                best_score: s.score,
                hits: 1,
                anchor: centre(&s.bbox),
                moved_us: None,
                moving_since_us: None,
                pending: None,
            });
        }
        // Lost tracks.
        let mut kept = Vec::with_capacity(self.tracks.len());
        for t in std::mem::take(&mut self.tracks) {
            if now_us - t.last_us <= p.max_gap_us {
                kept.push(t);
                continue;
            }
            if t.moving() {
                let mut stopped = t.clone();
                stopped.moving_since_us = None;
                out.push(Change::Stopped(stopped));
            }
            if t.confirmed(&p) {
                out.push(Change::Ended(t));
            }
        }
        self.tracks = kept;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(x: f32, y: f32) -> Seen {
        Seen {
            label: "person",
            class: "person",
            score: 0.9,
            bbox: [x, y, x + 50.0, y + 120.0],
        }
    }

    fn car(x: f32) -> Seen {
        Seen {
            label: "vehicle",
            class: "car",
            score: 0.95,
            bbox: [x, 100.0, x + 300.0, 260.0],
        }
    }

    const S: i64 = 1_000_000;

    fn kinds(c: &[Change]) -> Vec<(&'static str, u64)> {
        c.iter()
            .map(|c| match c {
                Change::Moving(t) => ("moving", t.id),
                Change::StillMoving(t) => ("still-moving", t.id),
                Change::Stopped(t) => ("stopped", t.id),
                Change::Ended(t) => ("ended", t.id),
            })
            .collect()
    }

    #[test]
    fn iou_basics() {
        assert_eq!(iou(&[0.0, 0.0, 10.0, 10.0], &[0.0, 0.0, 10.0, 10.0]), 1.0);
        assert_eq!(iou(&[0.0, 0.0, 10.0, 10.0], &[20.0, 0.0, 30.0, 10.0]), 0.0);
        let half = iou(&[0.0, 0.0, 10.0, 10.0], &[5.0, 0.0, 15.0, 10.0]);
        assert!((half - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn a_parked_car_is_tracked_but_never_moving() {
        let mut t = Tracker::new(Params::default());
        let mut all = Vec::new();
        // Ten minutes at 5 pictures a second, with a little box jitter.
        for i in 0..3000 {
            let jitter = (i % 3) as f32 * 2.0;
            all.extend(t.update(i * S / 5, &[car(200.0 + jitter)]));
        }
        assert!(all.is_empty(), "{:?}", kinds(&all));
        assert_eq!(t.tracks().len(), 1);
        assert_eq!(t.tracks()[0].hits, 3000);
    }

    #[test]
    fn a_person_walking_past_is_one_moving_spell() {
        let mut t = Tracker::new(Params::default());
        let mut all = Vec::new();
        // Walks 10 px a picture for 6 s (a box 50 wide moves 12.5 px to count).
        for i in 0..30 {
            all.extend(t.update(i * S / 5, &[person(100.0 + 10.0 * i as f32, 300.0)]));
        }
        // Then out of view: lost after 2 s.
        for i in 30..45 {
            all.extend(t.update(i * S / 5, &[]));
        }
        let k = kinds(&all);
        assert_eq!(k[0], ("moving", 1));
        assert!(
            k[1..k.len() - 2].iter().all(|c| c.0 == "still-moving"),
            "{k:?}"
        );
        assert_eq!(k[k.len() - 2..], [("stopped", 1), ("ended", 1)]);
        let Change::Moving(m) = &all[0] else { panic!() };
        assert!(m.hits >= 3, "only once confirmed");
        assert!(t.tracks().is_empty());
    }

    #[test]
    fn a_car_arrives_parks_and_stops_moving() {
        let mut t = Tracker::new(Params::default());
        let mut all = Vec::new();
        // Drives in over 3 s (30 px a picture), then sits for 20 s.
        let mut x = 0.0;
        for i in 0..115 {
            if i < 15 {
                x = 30.0 * i as f32;
            }
            all.extend(t.update(i * S / 5, &[car(x)]));
        }
        let k = kinds(&all);
        assert_eq!(k.first(), Some(&("moving", 1)));
        assert_eq!(k.last(), Some(&("stopped", 1)), "{k:?}");
        assert_eq!(k.iter().filter(|c| c.0 == "moving").count(), 1);
        // It stopped when it last moved a quarter of its length (it moves
        // 30 px a picture; 75 px counts), 5 s before it was called still.
        let Change::Stopped(s) = all.last().unwrap() else {
            panic!()
        };
        assert_eq!(s.last_us, 12 * S / 5);
        assert!(t.tracks()[0].moving_since_us.is_none());
    }

    #[test]
    fn two_people_keep_their_own_tracks() {
        let mut t = Tracker::new(Params::default());
        for i in 0..10 {
            let f = i as f32 * 15.0;
            t.update(
                i * S / 5,
                &[person(100.0 + f, 300.0), person(600.0 - f, 300.0)],
            );
        }
        let ids: Vec<u64> = t.tracks().iter().map(|t| t.id).collect();
        assert_eq!(ids, [1, 2]);
        assert!(t.tracks()[0].bbox[0] > 200.0 && t.tracks()[1].bbox[0] < 500.0);
        // A dog where a person was doesn't continue the person's track.
        let dog = Seen {
            label: "animal",
            class: "dog",
            ..person(235.0, 300.0)
        };
        t.update(10 * S / 5, &[dog]);
        assert_eq!(t.tracks().len(), 3);
    }

    #[test]
    fn a_car_passing_fast_far_off_is_followed() {
        let mut t = Tracker::new(Params::default());
        let mut all = Vec::new();
        // A 40 px car 60 px further on each picture: its boxes never
        // overlap.
        for i in 0..8 {
            let x = 60.0 * i as f32;
            let s = Seen {
                label: "vehicle",
                class: "car",
                score: 0.7,
                bbox: [x, 20.0, x + 40.0, 45.0],
            };
            all.extend(t.update(i * S / 5, &[s]));
        }
        assert_eq!(t.tracks().len(), 1, "one track, not eight");
        assert_eq!(kinds(&all)[0], ("moving", 1));
    }

    #[test]
    fn unsure_boxes_flickering_are_not_movement() {
        let mut t = Tracker::new(Params::default());
        let mut all = Vec::new();
        // A bicycle on a wall, seen as two overlapping boxes in turn: unsure
        // (0.55), then surer (0.62) but going nowhere.
        for i in 0..50 {
            let bbox = if i % 2 == 0 {
                [42.0, 2.0, 187.0, 121.0]
            } else {
                [1.0, 2.0, 189.0, 182.0]
            };
            let s = Seen {
                label: "vehicle",
                class: "bicycle",
                score: if i < 25 { 0.55 } else { 0.62 },
                bbox,
            };
            all.extend(t.update(i * S / 5, &[s]));
        }
        assert!(all.is_empty(), "{:?}", kinds(&all));
    }

    #[test]
    fn a_flicker_is_not_a_track() {
        let mut t = Tracker::new(Params::default());
        let mut all = Vec::new();
        // Seen twice, then gone: never confirmed, so nothing is reported.
        all.extend(t.update(0, &[person(0.0, 0.0)]));
        all.extend(t.update(S / 5, &[person(40.0, 0.0)]));
        for i in 2..20 {
            all.extend(t.update(i * S / 5, &[]));
        }
        assert!(all.is_empty(), "{:?}", kinds(&all));
    }
}
