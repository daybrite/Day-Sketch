//! The editor canvas: rendering, hit testing, and the move/resize drag machine.
//!
//! The draw closure walks the scene bottom→top; every field it touches is a TRACKED read, so a
//! moved shape repaints with no wiring at all. Interaction comes in through the canvas's own
//! gesture decorators — `on_tap_at` for selection and tool placement, `on_drag` for move and
//! resize — and every live gesture writes the model through PREVIEWS: the store (and therefore
//! the canvas and the status readouts) follows the pointer, while nothing durable fires until
//! the pointer lifts, when committing all touched fields in ONE turn makes the whole drag one
//! undo step and one SQL statement per row (https://daybrite.dev/docs/model).

use crate::model::{self, Node, NodeFields, NodeKind, Tool};
use day::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const HANDLE: f64 = 12.0;
const SELECTION: Color = Color::hex(0x2563EB);

// ---------------------------------------------------------------------------
// Drag machine
// ---------------------------------------------------------------------------

/// A corner handle, named by which frame edges it moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

enum DragOp {
    Idle,
    /// Every affected SHAPE with its starting origin (a group drags its descendants).
    Move {
        starts: Vec<(u64, f64, f64)>,
    },
    /// One shape, one corner, its starting frame.
    Resize {
        id: u64,
        corner: Corner,
        start: (f64, f64, f64, f64),
    },
}

fn field_previews_move(dx: f64, dy: f64, starts: &[(u64, f64, f64)]) {
    let store = model::nodes();
    for (id, sx, sy) in starts {
        store.elem(*id).x().write_preview(sx + dx);
        store.elem(*id).y().write_preview(sy + dy);
    }
}

fn field_commits_move(dx: f64, dy: f64, starts: &[(u64, f64, f64)]) {
    let store = model::nodes();
    for (id, sx, sy) in starts {
        store.elem(*id).x().write_commit(sx + dx);
        store.elem(*id).y().write_commit(sy + dy);
    }
}

fn resized(start: (f64, f64, f64, f64), corner: Corner, dx: f64, dy: f64) -> (f64, f64, f64, f64) {
    let (sx, sy, sw, sh) = start;
    let (mut x, mut y, mut w, mut h) = match corner {
        Corner::TopLeft => (sx + dx, sy + dy, sw - dx, sh - dy),
        Corner::TopRight => (sx, sy + dy, sw + dx, sh - dy),
        Corner::BottomLeft => (sx + dx, sy, sw - dx, sh + dy),
        Corner::BottomRight => (sx, sy, sw + dx, sh + dy),
    };
    // Clamp at the minimum by pinning the OPPOSITE edge, so the shape never flips.
    if w < model::MIN_SIZE {
        if matches!(corner, Corner::TopLeft | Corner::BottomLeft) {
            x = sx + sw - model::MIN_SIZE;
        }
        w = model::MIN_SIZE;
    }
    if h < model::MIN_SIZE {
        if matches!(corner, Corner::TopLeft | Corner::TopRight) {
            y = sy + sh - model::MIN_SIZE;
        }
        h = model::MIN_SIZE;
    }
    (x, y, w, h)
}

fn apply_resize(id: u64, frame: (f64, f64, f64, f64), commit: bool) {
    let store = model::nodes();
    let e = store.elem(id);
    if commit {
        // Four commits in ONE turn: one undo unit, one UPDATE of four columns.
        e.x().write_commit(frame.0);
        e.y().write_commit(frame.1);
        e.w().write_commit(frame.2);
        e.h().write_commit(frame.3);
    } else {
        e.x().write_preview(frame.0);
        e.y().write_preview(frame.1);
        e.w().write_preview(frame.2);
        e.h().write_preview(frame.3);
    }
}

// ---------------------------------------------------------------------------
// Hit testing
// ---------------------------------------------------------------------------

fn point_in_shape(store: Store<Keyed<Node>>, id: u64, px: f64, py: f64) -> bool {
    let e = store.elem(id);
    let (x, y, w, h) = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
    if px < x || py < y || px > x + w || py > y + h {
        return false;
    }
    match e.kind().peek() {
        NodeKind::Oval => {
            let (cx, cy) = (x + w / 2.0, y + h / 2.0);
            let (nx, ny) = ((px - cx) / (w / 2.0), (py - cy) / (h / 2.0));
            nx * nx + ny * ny <= 1.0
        }
        _ => true,
    }
}

/// The topmost top-level node under the point — z order honored, groups hit through any of
/// their shape descendants.
fn hit_top_level(px: f64, py: f64) -> Option<u64> {
    let store = model::nodes();
    for top in model::children_of(None).into_iter().rev() {
        let hit = model::shape_descendants(top)
            .into_iter()
            .any(|s| point_in_shape(store, s, px, py));
        if hit {
            return Some(top);
        }
    }
    None
}

fn corner_points(b: (f64, f64, f64, f64)) -> [(Corner, f64, f64); 4] {
    let (x, y, w, h) = b;
    [
        (Corner::TopLeft, x, y),
        (Corner::TopRight, x + w, y),
        (Corner::BottomLeft, x, y + h),
        (Corner::BottomRight, x + w, y + h),
    ]
}

/// A handle of the SINGLE selected shape under the point, if any.
fn hit_handle(px: f64, py: f64) -> Option<(u64, Corner)> {
    let sel = model::selection().get_untracked();
    let [only] = sel.as_slice() else {
        return None;
    };
    let store = model::nodes();
    if store.elem(*only).kind().peek() == NodeKind::Group {
        return None; // MVP: groups move; only shapes resize
    }
    let b = model::node_bounds(*only)?;
    for (corner, cx, cy) in corner_points(b) {
        if (px - cx).abs() <= HANDLE && (py - cy).abs() <= HANDLE {
            return Some((*only, corner));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The piece
// ---------------------------------------------------------------------------

fn shape_of(node: &Node) -> Shape {
    match node.kind {
        NodeKind::Oval => {
            // An ellipse as a scaled circle: build the unit circle at the frame's center and
            // let the path's own cubics carry the aspect via control-point scaling.
            oval_shape(node.x, node.y, node.w, node.h)
        }
        _ => PathBuilder::new()
            .move_to(Point::new(node.x, node.y))
            .line_to(Point::new(node.x + node.w, node.y))
            .line_to(Point::new(node.x + node.w, node.y + node.h))
            .line_to(Point::new(node.x, node.y + node.h))
            .close()
            .build(),
    }
}

/// A full ellipse from four cubic arcs (the standard 0.5523 kappa).
fn oval_shape(x: f64, y: f64, w: f64, h: f64) -> Shape {
    const K: f64 = 0.552_284_749_830_793_4;
    let (rx, ry) = (w / 2.0, h / 2.0);
    let (cx, cy) = (x + rx, y + ry);
    PathBuilder::new()
        .move_to(Point::new(cx + rx, cy))
        .cubic_to(
            Point::new(cx + rx, cy + ry * K),
            Point::new(cx + rx * K, cy + ry),
            Point::new(cx, cy + ry),
        )
        .cubic_to(
            Point::new(cx - rx * K, cy + ry),
            Point::new(cx - rx, cy + ry * K),
            Point::new(cx - rx, cy),
        )
        .cubic_to(
            Point::new(cx - rx, cy - ry * K),
            Point::new(cx - rx * K, cy - ry),
            Point::new(cx, cy - ry),
        )
        .cubic_to(
            Point::new(cx + rx * K, cy - ry),
            Point::new(cx + rx, cy - ry * K),
            Point::new(cx + rx, cy),
        )
        .close()
        .build()
}

fn fill_color(hex: &str) -> Color {
    u32::from_str_radix(hex.trim_start_matches('#'), 16)
        .map(Color::hex)
        .unwrap_or(Color::hex(0x888888))
}

fn draw_scene(d: &mut Draw) {
    let store = model::nodes();
    // TRACKED walk: shape + z reads through the collection, field reads per shape.
    let _shape_of_collection = store.keys();
    fn draw_children(d: &mut Draw, store: Store<Keyed<Node>>, parent: Option<u64>) {
        // TRACKED order: an arrange (a plain z write) must repaint NOW, not when the
        // selection next changes.
        for id in crate::model::children_of_tracked(parent) {
            let e = store.elem(id);
            match e.kind().with(|k| k.copied().unwrap_or_default()) {
                NodeKind::Group => draw_children(d, store, Some(id)),
                kind => {
                    let node = Node {
                        id,
                        parent: None,
                        z: 0.0,
                        kind,
                        x: e.x().read(),
                        y: e.y().read(),
                        w: e.w().read(),
                        h: e.h().read(),
                        fill: e.fill().read(),
                    };
                    d.fill(shape_of(&node), fill_color(&node.fill));
                    d.stroke(shape_of(&node), Color::rgba(0.0, 0.0, 0.0, 0.35), 1.0);
                }
            }
        }
    }
    draw_children(d, store, None);

    // Selection outlines + handles, above everything.
    let sel = model::selection().get();
    let single = sel.len() == 1;
    for id in &sel {
        let Some(b) = model::node_bounds(*id) else {
            continue;
        };
        let outline = PathBuilder::new()
            .move_to(Point::new(b.0, b.1))
            .line_to(Point::new(b.0 + b.2, b.1))
            .line_to(Point::new(b.0 + b.2, b.1 + b.3))
            .line_to(Point::new(b.0, b.1 + b.3))
            .close()
            .build();
        d.stroke(outline, SELECTION, 1.5);
        let is_shape = store.elem(*id).kind().with(|k| k.copied()) != Some(NodeKind::Group);
        if single && is_shape {
            for (_, cx, cy) in corner_points(b) {
                let half = HANDLE / 2.0;
                let handle = PathBuilder::new()
                    .move_to(Point::new(cx - half, cy - half))
                    .line_to(Point::new(cx + half, cy - half))
                    .line_to(Point::new(cx + half, cy + half))
                    .line_to(Point::new(cx - half, cy + half))
                    .close()
                    .build();
                d.fill(handle.clone(), Color::hex(0xFFFFFF));
                d.stroke(handle, SELECTION, 1.5);
            }
        }
    }
}

fn on_tap(p: Point) {
    match model::tool().get_untracked() {
        Tool::Rect => {
            let id = model::place_shape(NodeKind::Rect, p.x, p.y);
            model::selection().set(vec![id]);
            model::tool().set(Tool::Select);
        }
        Tool::Oval => {
            let id = model::place_shape(NodeKind::Oval, p.x, p.y);
            model::selection().set(vec![id]);
            model::tool().set(Tool::Select);
        }
        Tool::Select => select_at(p, day::modifiers()),
    }
}

/// The select tool's tap rule, with the modifiers that were held: shift or the platform's
/// command key toggles membership (the desktop multi-select idiom); a plain tap replaces.
pub(crate) fn select_at(p: Point, mods: day::Modifiers) {
    match hit_top_level(p.x, p.y) {
        None => model::selection().set(Vec::new()),
        Some(id) => {
            let sel = model::selection();
            if mods.shift || mods.primary {
                sel.update(|s| {
                    if let Some(i) = s.iter().position(|x| *x == id) {
                        s.remove(i);
                    } else {
                        s.push(id);
                    }
                });
            } else {
                sel.set(vec![id]);
            }
        }
    }
}

fn on_drag(drag: Drag, op: &Rc<RefCell<DragOp>>) {
    let store = model::nodes();
    match drag.phase {
        DragPhase::Began => {
            let p = drag.location;
            let next = if let Some((id, corner)) = hit_handle(p.x, p.y) {
                let e = store.elem(id);
                DragOp::Resize {
                    id,
                    corner,
                    start: (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek()),
                }
            } else if let Some(top) = hit_top_level(p.x, p.y) {
                // Dragging an unselected shape selects it first (single), then moves the
                // WHOLE selection if the hit is part of it.
                let mut sel = model::selection().get_untracked();
                if !sel.contains(&top) {
                    sel = vec![top];
                    model::selection().set(sel.clone());
                }
                let mut starts = Vec::new();
                for t in &sel {
                    for s in model::shape_descendants(*t) {
                        let e = store.elem(s);
                        starts.push((s, e.x().peek(), e.y().peek()));
                    }
                }
                DragOp::Move { starts }
            } else {
                DragOp::Idle
            };
            *op.borrow_mut() = next;
        }
        DragPhase::Changed => match &*op.borrow() {
            DragOp::Move { starts } => {
                field_previews_move(drag.translation.x, drag.translation.y, starts)
            }
            DragOp::Resize { id, corner, start } => {
                let f = resized(*start, *corner, drag.translation.x, drag.translation.y);
                apply_resize(*id, f, false);
            }
            DragOp::Idle => {}
        },
        DragPhase::Ended => {
            let finished = std::mem::replace(&mut *op.borrow_mut(), DragOp::Idle);
            match finished {
                DragOp::Move { starts } => model::undo_stack().grouped("move", || {
                    field_commits_move(drag.translation.x, drag.translation.y, &starts)
                }),
                DragOp::Resize { id, corner, start } => {
                    let f = resized(start, corner, drag.translation.x, drag.translation.y);
                    model::undo_stack().grouped("resize", || apply_resize(id, f, true));
                }
                DragOp::Idle => {}
            }
        }
    }
}

pub(crate) fn editor_canvas() -> AnyPiece {
    let op: Rc<RefCell<DragOp>> = Rc::new(RefCell::new(DragOp::Idle));
    let op2 = op.clone();
    canvas(move |d, _size| draw_scene(d))
        .on_tap_at(on_tap)
        .on_drag(move |drag| on_drag(drag, &op2))
        .context_menu(crate::context_menu_entries())
        .id("canvas")
        .grow()
        .any()
}
