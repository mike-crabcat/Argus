//! Commands: rules, atomicity, and the undo/redo property (applying a
//! command's inverse restores the plan; applying that inverse's inverse
//! redoes it), checked over thousands of random commands.

use argus_plan::fixture::demo_house;
use argus_plan::*;

fn level(p: &Plan) -> Id {
    p.levels[0].id
}

fn add_wall(p: &mut Plan, a: Point, b: Point) -> Result<Command, PlanError> {
    p.apply(&Command::AddWall {
        level: level(p),
        id: None,
        a: End::At(a),
        b: End::At(b),
        thickness: 100.0,
    })
}

#[test]
fn walls_join_at_ends_crossings_and_t_junctions() {
    let mut p = Plan::default();
    add_wall(&mut p, pt(0.0, 0.0), pt(4000.0, 0.0)).unwrap();
    add_wall(&mut p, pt(4000.0, 0.0), pt(4000.0, 3000.0)).unwrap();
    let l = &p.levels[0];
    assert_eq!(l.nodes.len(), 3, "the corner is one node");
    // A T onto the middle of the first wall splits it.
    add_wall(&mut p, pt(2000.0, 0.0), pt(2000.0, -2000.0)).unwrap();
    assert_eq!(p.levels[0].walls.len(), 4);
    // Crossing the T's stem makes an X: both split.
    add_wall(&mut p, pt(1000.0, -1000.0), pt(3000.0, -1000.0)).unwrap();
    let l = &p.levels[0];
    assert_eq!(l.walls.len(), 7);
    assert!(validate::validate(&p).is_empty());
    // Same wall again: refused, plan unchanged.
    let before = p.clone();
    assert!(add_wall(&mut p, pt(0.0, 0.0), pt(2000.0, 0.0)).is_err());
    assert_eq!(p, before);
    assert!(add_wall(&mut p, pt(5.0, 5.0), pt(5.0, 5.4)).is_err());
}

#[test]
fn openings_must_fit_and_follow_splits() {
    let mut p = Plan::default();
    let lv = level(&p);
    add_wall(&mut p, pt(0.0, 0.0), pt(5000.0, 0.0)).unwrap();
    let wall = p.levels[0].walls[0].id;
    let door = |offset, width| Command::AddOpening {
        level: lv,
        id: None,
        wall,
        kind: OpeningKind::Door,
        offset,
        width,
        swing: Some(Swing {
            hinge: Hinge::Start,
            side: Side::Left,
        }),
    };
    assert!(p.apply(&door(300.0, 900.0)).is_err(), "past the start");
    p.apply(&door(1000.0, 900.0)).unwrap();
    assert!(p.apply(&door(1500.0, 900.0)).is_err(), "overlaps");
    p.apply(&door(4000.0, 900.0)).unwrap();
    assert!(
        p.apply(&Command::AddOpening {
            level: lv,
            id: None,
            wall,
            kind: OpeningKind::Window,
            offset: 2500.0,
            width: 900.0,
            swing: Some(Swing {
                hinge: Hinge::End,
                side: Side::Right
            }),
        })
        .is_err(),
        "windows don't swing"
    );
    // Splitting through a door is refused; elsewhere the far door moves.
    let split = |offset| Command::SplitWall {
        level: lv,
        wall,
        offset,
        node: None,
        new_wall: None,
    };
    assert!(p.apply(&split(1100.0)).is_err());
    p.apply(&split(2500.0)).unwrap();
    let l = &p.levels[0];
    let far = &l.openings[1];
    assert_ne!(far.wall, wall);
    assert_eq!(far.offset, 1500.0);
    assert!(validate::validate(&p).is_empty());
}

#[test]
fn deleting_a_node_takes_its_walls_and_openings() {
    let mut p = demo_house();
    let lv = level(&p);
    let l = &p.levels[0];
    let corner = l.node_near(pt(16000.0, 0.0), 1.0).unwrap().id;
    let n_walls = l.walls.len();
    p.apply(&Command::DeleteEntity {
        level: lv,
        id: corner,
    })
    .unwrap();
    let l = &p.levels[0];
    assert_eq!(l.walls.len(), n_walls - 2);
    assert!(l.openings.iter().all(|o| l.wall(o.wall).is_some()));
    assert!(l.node(corner).is_none());
}

#[test]
fn ids_are_stable_and_never_reused() {
    let mut p = Plan::default();
    let lv = level(&p);
    let place = |id| Command::PlaceCamera {
        level: lv,
        id,
        camera: "door".into(),
        at: pt(0.0, 0.0),
        heading: 0.0,
        fov: 90.0,
        range: 8000.0,
        mount_height: 2400.0,
    };
    p.apply(&place(Some(Id(500)))).unwrap();
    assert_eq!(
        p.apply(&place(Some(Id(500)))),
        Err(PlanError::DuplicateId(Id(500)))
    );
    let inv = p.apply(&place(None)).unwrap();
    let Command::DeleteEntity { id, .. } = inv else {
        panic!("{inv:?}")
    };
    assert_eq!(id, Id(501));
    p.apply(&inv).unwrap();
    p.apply(&place(None)).unwrap();
    assert_eq!(p.levels[0].cameras[1].id, Id(502), "501 is not reused");
}

#[test]
fn batches_are_all_or_nothing_and_levels_stay() {
    let mut p = demo_house();
    let lv = level(&p);
    let before = p.clone();
    let r = p.apply_all(&[
        Command::SetRoomName {
            level: lv,
            room: p.levels[0].rooms[0].id,
            name: "Main bedroom".into(),
        },
        Command::MoveNode {
            level: lv,
            node: Id(99_999),
            to: pt(0.0, 0.0),
        },
    ]);
    assert_eq!(r, Err(PlanError::NoEntity(Id(99_999))));
    assert_eq!(p, before);
    assert!(p.apply(&Command::DeleteLevel { level: lv }).is_err());
    let inv = p
        .apply(&Command::AddLevel {
            id: None,
            name: "Yard".into(),
        })
        .unwrap();
    assert_eq!(p.levels.len(), 2);
    p.apply(&inv).unwrap();
    assert_eq!(p.levels, before.levels);
}

#[test]
fn update_entity_checks_and_undoes() {
    let mut p = demo_house();
    let lv = level(&p);
    let mut cam = p.levels[0].cameras[0].clone();
    cam.fov = 0.0;
    assert!(
        p.apply(&Command::UpdateEntity {
            level: lv,
            entity: Entity::Camera(cam.clone())
        })
        .is_err()
    );
    cam.fov = 120.0;
    cam.heading = 250.0;
    let before = p.clone();
    let inv = p
        .apply(&Command::UpdateEntity {
            level: lv,
            entity: Entity::Camera(cam.clone()),
        })
        .unwrap();
    assert_eq!(p.levels[0].cameras[0], cam);
    p.apply(&inv).unwrap();
    assert_eq!(p, before);
    // A wall can't become a room.
    let wall = p.levels[0].walls[0].id;
    let room = Room {
        id: wall,
        name: "x".into(),
        at: pt(0.0, 0.0),
        polygon: None,
    };
    assert!(matches!(
        p.apply(&Command::UpdateEntity {
            level: lv,
            entity: Entity::Room(room)
        }),
        Err(PlanError::WrongType(..))
    ));
}

/// A small deterministic random source (xorshift).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn f(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (self.next() % 10_000) as f64 / 10_000.0 * (hi - lo)
    }
    fn grid(&mut self, lo: f64, hi: f64) -> f64 {
        (self.f(lo, hi) / 100.0).round() * 100.0
    }
    fn point(&mut self) -> Point {
        pt(self.grid(-2000.0, 18000.0), self.grid(-2000.0, 13000.0))
    }
}

fn some_id(r: &mut Rng, l: &Level) -> Id {
    let mut ids: Vec<Id> = Vec::new();
    ids.extend(l.nodes.iter().map(|e| e.id));
    ids.extend(l.walls.iter().map(|e| e.id));
    ids.extend(l.openings.iter().map(|e| e.id));
    ids.extend(l.rooms.iter().map(|e| e.id));
    ids.extend(l.dimensions.iter().map(|e| e.id));
    ids.extend(l.cameras.iter().map(|e| e.id));
    if ids.is_empty() || r.below(20) == 0 {
        return Id(r.next() % 1000); // sometimes a missing one
    }
    ids[r.below(ids.len())]
}

fn random_command(r: &mut Rng, p: &Plan) -> Command {
    let li = r.below(p.levels.len());
    let l = &p.levels[li];
    let level = l.id;
    let end = |r: &mut Rng| {
        if !l.nodes.is_empty() && r.below(2) == 0 {
            End::Node(l.nodes[r.below(l.nodes.len())].id)
        } else {
            End::At(r.point())
        }
    };
    match r.below(17) {
        0..=2 => Command::AddWall {
            level,
            id: None,
            a: end(r),
            b: end(r),
            thickness: [90.0, 230.0, 5.0][r.below(3)],
        },
        3 => Command::MoveNode {
            level,
            node: some_id(r, l),
            to: r.point(),
        },
        4 => {
            let wall = l.walls.get(r.below(l.walls.len())).map_or(Id(1), |w| w.id);
            Command::SplitWall {
                level,
                wall,
                offset: r.grid(0.0, 6000.0),
                node: None,
                new_wall: None,
            }
        }
        5 | 6 => {
            let wall = l.walls.get(r.below(l.walls.len())).map_or(Id(1), |w| w.id);
            let door = r.below(2) == 0;
            Command::AddOpening {
                level,
                id: None,
                wall,
                kind: if door {
                    OpeningKind::Door
                } else {
                    OpeningKind::Window
                },
                offset: r.grid(0.0, 6000.0),
                width: r.grid(600.0, 2500.0),
                swing: door.then_some(Swing {
                    hinge: Hinge::End,
                    side: Side::Left,
                }),
            }
        }
        7 => Command::AddRoom {
            level,
            id: None,
            name: "Room".into(),
            at: r.point(),
            polygon: None,
        },
        8 => Command::SetRoomName {
            level,
            room: some_id(r, l),
            name: format!("Room {}", r.below(100)),
        },
        9 => Command::AddDimension {
            level,
            id: None,
            a: r.point(),
            b: r.point(),
            offset: r.grid(-1000.0, 1000.0),
        },
        10 => Command::PlaceCamera {
            level,
            id: None,
            camera: "cam".into(),
            at: r.point(),
            heading: r.f(0.0, 360.0),
            fov: r.f(0.0, 400.0),
            range: r.grid(1000.0, 15000.0),
            mount_height: 2400.0,
        },
        11 | 12 => Command::DeleteEntity {
            level,
            id: some_id(r, l),
        },
        13 => Command::MoveEntities {
            level,
            ids: (0..r.below(4)).map(|_| some_id(r, l)).collect(),
            by: pt(r.grid(-500.0, 500.0), r.grid(-500.0, 500.0)),
        },
        14 => {
            // Change one entity a little.
            let entity = match l.entity(some_id(r, l)) {
                Some(Entity::Camera(mut c)) => {
                    c.heading += 15.0;
                    Entity::Camera(c)
                }
                Some(Entity::Opening(mut o)) => {
                    o.offset += 100.0;
                    Entity::Opening(o)
                }
                Some(Entity::Wall(mut w)) => {
                    w.thickness = r.grid(0.0, 400.0);
                    Entity::Wall(w)
                }
                Some(Entity::Node(mut n)) => {
                    n.at = r.point();
                    Entity::Node(n)
                }
                Some(e) => e,
                None => Entity::Dimension(Dimension {
                    id: Id(77_777),
                    a: r.point(),
                    b: r.point(),
                    offset: 0.0,
                }),
            };
            Command::UpdateEntity { level, entity }
        }
        15 => match r.below(3) {
            0 => Command::AddLevel {
                id: None,
                name: "Yard".into(),
            },
            1 => Command::DeleteLevel { level },
            _ => Command::RenameLevel {
                level,
                name: "Upstairs".into(),
            },
        },
        _ => Command::SetBackground {
            level,
            background: (r.below(2) == 0).then(|| Background {
                asset: "abc123.png".into(),
                mm_per_px: r.f(-1.0, 30.0),
                offset: r.point(),
                rotation: r.f(-10.0, 10.0),
                opacity: r.f(0.0, 1.2),
            }),
        },
    }
}

#[test]
fn inverse_undoes_and_its_inverse_redoes() {
    let (mut ok, mut refused) = (0, 0);
    for seed in 1..=40u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut p = if seed % 2 == 0 {
            demo_house()
        } else {
            Plan::default()
        };
        for _ in 0..150 {
            let cmd = random_command(&mut r, &p);
            // Commands survive JSON unchanged.
            let json = serde_json::to_string(&cmd).unwrap();
            assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
            let before = p.clone();
            let inv = match p.apply(&cmd) {
                Ok(inv) => inv,
                Err(_) => {
                    assert_eq!(p, before, "a refused command changed the plan: {json}");
                    refused += 1;
                    continue;
                }
            };
            ok += 1;
            let after = p.clone();
            let redo = p
                .apply(&inv)
                .unwrap_or_else(|e| panic!("undo failed ({e}) for {json}"));
            assert_eq!(p.levels, before.levels, "undo didn't restore: {json}");
            p.apply(&redo)
                .unwrap_or_else(|e| panic!("redo failed ({e}) for {json}"));
            assert_eq!(p.levels, after.levels, "redo didn't reapply: {json}");
            // Ids only go up.
            assert!(p.next_id >= after.next_id);
        }
    }
    assert!(ok > 2000 && refused > 500, "ok {ok}, refused {refused}");
}

/// docs/plan-commands.schema.json matches the code. Regenerate with
/// `ARGUS_WRITE_SCHEMA=1 cargo test -p argus-plan schema`.
#[test]
fn schema_file_is_current() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/plan-commands.schema.json"
    );
    let want = serde_json::to_string_pretty(&command_schema()).unwrap() + "\n";
    if std::env::var_os("ARGUS_WRITE_SCHEMA").is_some() {
        std::fs::write(path, &want).unwrap();
    }
    let have = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        have == want,
        "docs/plan-commands.schema.json is stale: ARGUS_WRITE_SCHEMA=1 cargo test -p argus-plan schema"
    );
}

/// A batch naming its walls with consecutive ids works, though each wall's
/// ends are numbered as it is added (models write batches like this).
#[test]
fn batches_can_number_their_walls_consecutively() {
    let mut p = Plan::default();
    let lv = level(&p);
    let w = |id: u64, a: (f64, f64), b: (f64, f64)| Command::AddWall {
        level: lv,
        id: Some(Id(id)),
        a: End::At(pt(a.0, a.1)),
        b: End::At(pt(b.0, b.1)),
        thickness: 230.0,
    };
    let batch = vec![
        w(2, (0.0, 0.0), (4000.0, 0.0)),
        w(3, (4000.0, 0.0), (4000.0, 3000.0)),
        w(4, (4000.0, 3000.0), (0.0, 3000.0)),
        w(5, (0.0, 3000.0), (0.0, 0.0)),
        Command::AddOpening {
            level: lv,
            id: Some(Id(6)),
            wall: Id(3),
            kind: OpeningKind::Window,
            offset: 1500.0,
            width: 900.0,
            swing: None,
        },
    ];
    let before = p.clone();
    let inv = p.apply_all(&batch).unwrap();
    let l = &p.levels[0];
    assert_eq!(
        l.walls.iter().map(|w| w.id.0).collect::<Vec<_>>(),
        vec![2, 3, 4, 5]
    );
    assert_eq!(l.openings[0].wall, Id(3));
    assert!(
        l.nodes.iter().all(|n| n.id.0 > 6),
        "wall ends skip the batch's ids"
    );
    // The same batch on a copy gives the same plan (browser and server agree).
    let mut again = before.clone();
    again.apply_all(&batch).unwrap();
    assert_eq!(again, p);
    p.apply(&inv).unwrap();
    assert_eq!(p.levels, before.levels);
    // Checked application gives the same result, or names the command.
    let mut c = before.clone();
    c.apply_all_checked(&batch).unwrap();
    assert_eq!(c, again);
    let mut bad = batch.clone();
    bad.push(Command::DeleteEntity {
        level: lv,
        id: Id(999),
    });
    let mut d = before.clone();
    assert_eq!(d.apply_all_checked(&bad).unwrap_err().0, 5);
    assert_eq!(d, before, "nothing applied");
}

#[test]
fn whole_plan_check() {
    let mut p = demo_house();
    let want = p.next_id;
    p.next_id = 1;
    p.check().unwrap();
    assert_eq!(p.next_id, want, "raised past the ids in use");
    let mut bad = demo_house();
    let dup = bad.levels[0].walls[0].id;
    bad.levels[0].cameras[0].id = dup;
    assert_eq!(bad.check(), Err(PlanError::DuplicateId(dup)));
    let mut bad = demo_house();
    bad.levels[0].nodes.remove(0);
    assert!(bad.check().is_err());
    assert!(Plan::empty(1).check().is_err());
}
