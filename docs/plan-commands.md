# Floor plan commands

A floor plan changes only through commands. The editor sends them, the
server applies them, and an import tool (for example an agent reading a
sketch) can send the same ones. The exact shapes are in
[`plan-commands.schema.json`](plan-commands.schema.json), generated from
the code in `crates/argus-plan` and checked by its tests.

## Conventions

- Lengths are **millimetres**. +x is east (right), +y is north (up).
- Angles are **degrees counter-clockwise from +x** (east = 0, north = 90).
- Every entity (level, node, wall, opening, room, dimension, camera) has a
  numeric `id`, unique across the plan and never reused. Commands that add
  something take an optional `id`. Give one (unused) to refer to the new
  entity later in the same batch. Leave it out and you get the next free id.
  Ids a batch gives are kept free while it is applied: wall ends and split
  walls, which Argus numbers itself, skip them, so a batch can number its
  walls 2, 3, 4... and refer to them.
- Walls run along their centreline from node `a` to node `b`. Walls that
  share a node are joined there, so corners mitre. A wall end given as a
  point joins any node within 1 mm, or splits any wall it lands on (a
  T-junction). A wall drawn across another splits both where they cross.
- An opening's `offset` is the distance from its wall's node `a` to the
  opening's **centre**. Doors may have a `swing`: `hinge` is the `start`
  end (towards `a`) or the `end` (towards `b`). `side` is `left` or
  `right`, looking from `a` towards `b`. Windows don't swing.
- A room is a name and a point inside it. Its outline is the smallest
  area enclosed by walls around that point, unless `polygon` gives one.

## The API

- `GET /api/plan` returns `{"version": 7, "plan": {...}}`.
- `POST /api/plan/commands` with `{"version": 7, "commands": [...]}` applies
  the commands as one batch: all of them or none. If `version` is given and
  the plan has moved on, the server answers 409 and applies nothing. Leave
  `version` out to apply against whatever is current. The answer is
  `{"version": 8}`. A rejected command gives 400 with the reason.
- `PUT /api/plan` with `{"version": 7, "plan": {...}}` replaces the whole
  plan, with the same version check.
- `POST /api/plan/assets` with a PNG or JPEG body (at most 20 MB) stores a
  background image and returns `{"id": "...", "width": W, "height": H}`.
  `GET /api/plan/assets/{id}` serves it.

- `POST /api/plan/import` with `{"asset": "...", "level": 1}` and
  `POST /api/plan/ask` with `{"text": "...", "level": 1}` have the agent
  (if configured) draft a command batch. It gets this schema as its one
  tool; Argus applies its batch to a copy of the plan and sends refusals
  back for it to fix. The answer is `{"commands": [...], "summary": "...",
  "warnings": [...], "run": {...}}`. Nothing is saved: send the commands
  to `/api/plan/commands` to keep them.

Reading needs any valid token, including a phone's. Changing the plan needs
the main token.

## Commands

| `op` | What it does |
| --- | --- |
| `add_level`, `rename_level`, `delete_level` | Levels such as "Ground" or "Yard". The last level can't be deleted. |
| `add_wall` | A wall from `a` to `b`. Each end is `{"node": id}` or `{"at": {"x": .., "y": ..}}`. |
| `move_node` | Moves a wall joint, and so every wall ending there. |
| `split_wall` | Adds a joint `offset` mm along a wall. Openings follow their part. |
| `add_opening` | A `door` or `window` in a wall. It must fit and not overlap another opening. |
| `add_room`, `set_room_name` | Named rooms. |
| `add_dimension` | A measured line from `a` to `b`, drawn `offset` mm to its left. |
| `place_camera` | A camera by its name in `argus.toml`: position, `heading`, `fov`, `range`, `mount_height`. |
| `update_entity` | Replaces any entity's fields (same id and type). |
| `delete_entity` | A node takes its walls with it, and a wall takes its openings. Joints left with no walls go too. |
| `move_entities` | Moves nodes, walls, rooms, dimensions and cameras by `by`. |
| `set_calibration` | A placed camera's calibration from point pairs: `image` (fractions of its picture, from the top-left) and `plan` (mm) for 4 or more spots on the ground. Argus fits a homography (from 6 pairs up, setting aside any that disagree with the rest) and stores it with each pair's error; `null` removes it. |
| `set_background` | A level's background image: `asset`, `mm_per_px`, `offset`, `rotation`, `opacity` (0 to 1), or `null` to remove it. |
| `replace_level`, `restore_level` | Whole-level replacement (undo uses these; also useful for bulk import). |
| `batch` | Several commands as one. |

## Example: a room with a door, from nothing

```json
{"commands": [
  {"op": "add_wall", "level": 1, "id": 100, "a": {"at": {"x": 0, "y": 0}},    "b": {"at": {"x": 4000, "y": 0}},    "thickness": 230},
  {"op": "add_wall", "level": 1,            "a": {"at": {"x": 4000, "y": 0}}, "b": {"at": {"x": 4000, "y": 3000}}, "thickness": 230},
  {"op": "add_wall", "level": 1,            "a": {"at": {"x": 4000, "y": 3000}}, "b": {"at": {"x": 0, "y": 3000}}, "thickness": 230},
  {"op": "add_wall", "level": 1,            "a": {"at": {"x": 0, "y": 3000}}, "b": {"at": {"x": 0, "y": 0}},       "thickness": 230},
  {"op": "add_opening", "level": 1, "wall": 100, "kind": "door", "offset": 1000, "width": 820,
   "swing": {"hinge": "start", "side": "left"}},
  {"op": "add_room", "level": 1, "name": "Study", "at": {"x": 2000, "y": 1500}},
  {"op": "place_camera", "level": 1, "camera": "doorbell", "at": {"x": 1000, "y": -200},
   "heading": 270, "fov": 150, "range": 7000, "mount_height": 1300}
]}
```

Level 1 is the "Ground" level every new plan starts with. The corners join
because each wall starts where the last one ended.

## Undo

Applying a command returns its inverse. Applying the inverse restores the
plan exactly and returns the redo command. The editor keeps these on its
undo and redo stacks. Structural edits (walls, splits, deletes, moves)
undo by restoring the level as it was. Ids are never reused, so redo
brings back the same ids.
