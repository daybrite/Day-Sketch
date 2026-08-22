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
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Hit tolerance around a corner handle — deliberately larger than the DRAWN handle, so the
/// grab target stays comfortable while the visuals stay small.
const HANDLE: f64 = 12.0;
/// The drawn handle's edge length: small and slightly translucent, so handles read as
/// affordances over the artwork rather than as artwork.
const HANDLE_DRAW: f64 = 8.0;
const SELECTION: Color = Color::hex(0x2563EB);
const HANDLE_FILL: Color = Color::rgba(1.0, 1.0, 1.0, 0.85);
const HANDLE_STROKE: Color = Color::rgba(0.145, 0.388, 0.922, 0.9);

// ---------------------------------------------------------------------------
// The view transform
// ---------------------------------------------------------------------------

/// The zoom range: far enough out to see a whole drawing, close enough in for pixel work.
const ZOOM_MIN: f64 = 0.25;
const ZOOM_MAX: f64 = 4.0;

thread_local! {
    /// The canvas's last laid-out size — the anchor for menu/toolbar zoom (its center).
    static VIEWPORT: Cell<(f64, f64)> = const { Cell::new((800.0, 600.0)) };
}

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    /// The last click this canvas acted on, and which recognizer reported it — see
    /// [`handle_click`].
    static LAST_CLICK: Cell<Option<(std::time::Instant, f64, f64, ClickSource)>> =
        const { Cell::new(None) };
}

/// The canvas magnification: screen = model × zoom + pan. Transient view state — never
/// persisted and never undoable; unlike the selection it is not even restored by undo.
pub(crate) fn zoom() -> Signal<f64> {
    thread_local! {
        static Z: Signal<f64> = Signal::global(1.0);
    }
    Z.with(|s| *s)
}

/// The canvas translation, in screen pixels — where the model's origin sits in the viewport.
fn pan() -> Signal<Point> {
    thread_local! {
        static P: Signal<Point> = Signal::global(Point::ZERO);
    }
    P.with(|s| *s)
}

fn viewport_center() -> Point {
    let (w, h) = VIEWPORT.with(|v| v.get());
    Point::new(w / 2.0, h / 2.0)
}

/// Set the zoom, keeping the model point under `anchor` (screen coords) fixed on screen —
/// the pinch stays under the fingers, the menu zoom stays centered.
fn zoom_to(target: f64, anchor: Point) {
    let z0 = zoom().get_untracked();
    let z1 = target.clamp(ZOOM_MIN, ZOOM_MAX);
    let p0 = pan().get_untracked();
    let f = z1 / z0;
    pan().set(Point::new(
        anchor.x - (anchor.x - p0.x) * f,
        anchor.y - (anchor.y - p0.y) * f,
    ));
    zoom().set(z1);
}

/// The menu/toolbar zoom step, anchored at the viewport center. In (×1.25) and out (×0.8)
/// are exact inverses, so a step each way lands back on the starting view.
pub(crate) fn zoom_step(factor: f64) {
    zoom_to(zoom().get_untracked() * factor, viewport_center());
}

pub(crate) fn zoom_reset() {
    zoom_to(1.0, viewport_center());
}

fn to_model(p: Point) -> Point {
    let z = zoom().get_untracked();
    let pn = pan().get_untracked();
    Point::new((p.x - pn.x) / z, (p.y - pn.y) / z)
}

/// A model-space frame's screen-space frame, under the CURRENT (untracked) transform.
fn screen_bounds(b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let z = zoom().get_untracked();
    let pn = pan().get_untracked();
    (b.0 * z + pn.x, b.1 * z + pn.y, b.2 * z, b.3 * z)
}

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

/// A corner handle of ANY selected shape under the SCREEN point: every selected shape
/// carries its own handles, and dragging one resizes THAT shape alone (a multi-select drag
/// anywhere else moves the whole selection). Groups have no handles — groups move; only
/// shapes resize. Later selections are checked first, matching the draw order (last drawn
/// sits on top). Handles live in screen space — their size and grab target stay constant at
/// every zoom, so the test transforms the corners, not the pointer.
fn hit_handle(px: f64, py: f64) -> Option<(u64, Corner)> {
    let sel = model::selection().get_untracked();
    let store = model::nodes();
    for id in sel.iter().rev() {
        if store.elem(*id).kind().peek() == NodeKind::Group {
            continue;
        }
        let Some(b) = model::node_bounds(*id) else {
            continue;
        };
        for (corner, cx, cy) in corner_points(screen_bounds(b)) {
            if (px - cx).abs() <= HANDLE && (py - cy).abs() <= HANDLE {
                return Some((*id, corner));
            }
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
        _ => rect_shape(node.x, node.y, node.w, node.h),
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

fn rect_shape(x: f64, y: f64, w: f64, h: f64) -> Shape {
    PathBuilder::new()
        .move_to(Point::new(x, y))
        .line_to(Point::new(x + w, y))
        .line_to(Point::new(x + w, y + h))
        .line_to(Point::new(x, y + h))
        .close()
        .build()
}

fn draw_scene(d: &mut Draw, size: Size) {
    // Everything clips to the viewport: panned/zoomed content otherwise escapes the canvas
    // in OFFSCREEN captures (the live window clips it; cacheDisplayInRect does not).
    d.clip(rect_shape(0.0, 0.0, size.width, size.height));
    // The document's background, under everything and across the whole viewport — the canvas
    // is an unbounded plane, so the background is not part of the zoomable content. TRACKED:
    // the Canvas tab's well repaints it live.
    d.fill(
        rect_shape(0.0, 0.0, size.width, size.height),
        fill_color(&model::background()),
    );

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
                        fill_opacity: e.fill_opacity().read(),
                        stroke: e.stroke().read(),
                        stroke_width: e.stroke_width().read(),
                        stroke_opacity: e.stroke_opacity().read(),
                    };
                    d.fill(
                        shape_of(&node),
                        fill_color(&node.fill).with_alpha(node.fill_opacity),
                    );
                    if node.stroke_width > 0.0 && node.stroke_opacity > 0.0 {
                        d.stroke(
                            shape_of(&node),
                            fill_color(&node.stroke).with_alpha(node.stroke_opacity),
                            node.stroke_width,
                        );
                    }
                }
            }
        }
    }
    // The scene under the view transform; TRACKED zoom/pan reads, so a gesture repaints.
    let z = zoom().get();
    let pn = pan().get();
    d.transformed(
        Affine::scale(z, z).then(Affine::translate(pn.x, pn.y)),
        |d| draw_children(d, store, None),
    );

    // Selection outlines + handles, above everything, drawn in SCREEN space at the
    // transformed positions — constant weight and size at every zoom. Every selected SHAPE
    // carries its own handles — multi-selections included; a group shows only its union
    // outline.
    let sel = model::selection().get();
    for id in &sel {
        let Some(b) = model::node_bounds(*id).map(screen_bounds) else {
            continue;
        };
        d.stroke(rect_shape(b.0, b.1, b.2, b.3), SELECTION, 1.5);
        let is_shape = store.elem(*id).kind().with(|k| k.copied()) != Some(NodeKind::Group);
        if is_shape {
            for (_, cx, cy) in corner_points(b) {
                let half = HANDLE_DRAW / 2.0;
                let handle = rect_shape(cx - half, cy - half, HANDLE_DRAW, HANDLE_DRAW);
                d.fill(handle.clone(), HANDLE_FILL);
                d.stroke(handle, HANDLE_STROKE, 1.0);
            }
        }
    }
}

/// Which recognizer reported a click — the dedup key in [`handle_click`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClickSource {
    Tap,
    DragEnd,
}

/// One physical click, from whichever recognizer reported it. On desktop a click that
/// wiggles a pixel arrives as a DRAG, not a tap — the pan recognizer claims it and the click
/// recognizer fails — so [`on_drag`] routes a drag that never really moved here too. Some
/// backends (qt) deliver BOTH events for one stationary click; the guard drops the second
/// report of the same press. It only ever pairs ACROSS sources — two taps are two clicks
/// however close together (a double-click, a fast scripted run) — and a dropped report is
/// not recorded, so it cannot chain into dropping the next press's.
fn handle_click(p_screen: Point, source: ClickSource) {
    // On wasm the guard compiles out: std::time is unavailable there, and the dom shim's 4px
    // slop already makes tap and drag exclusive, so a double report cannot happen.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let now = std::time::Instant::now();
        if let Some((t, x, y, s)) = LAST_CLICK.with(|c| c.get())
            && s != source
            && now.duration_since(t).as_millis() < 30
            && (p_screen.x - x).abs() < 5.0
            && (p_screen.y - y).abs() < 5.0
        {
            return;
        }
        LAST_CLICK.with(|c| c.set(Some((now, p_screen.x, p_screen.y, source))));
    }
    #[cfg(target_arch = "wasm32")]
    let _ = source;
    on_tap(to_model(p_screen));
}

/// `p` is in MODEL space — callers convert from screen coordinates first.
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
/// `p` is in model space.
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
    // The drag arrives in screen pixels; the model lives behind the view transform. Handles
    // are hit in screen space (constant grab target), shapes in model space, and every
    // translation is divided by the zoom so the shape tracks the pointer 1:1 on screen.
    let zf = zoom().get_untracked();
    match drag.phase {
        DragPhase::Began => {
            let p = drag.location;
            let m = to_model(p);
            let next = if let Some((id, corner)) = hit_handle(p.x, p.y) {
                let e = store.elem(id);
                DragOp::Resize {
                    id,
                    corner,
                    start: (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek()),
                }
            } else if let Some(top) = hit_top_level(m.x, m.y) {
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
                field_previews_move(drag.translation.x / zf, drag.translation.y / zf, starts)
            }
            DragOp::Resize { id, corner, start } => {
                let f = resized(
                    *start,
                    *corner,
                    drag.translation.x / zf,
                    drag.translation.y / zf,
                );
                apply_resize(*id, f, false);
            }
            DragOp::Idle => {}
        },
        DragPhase::Ended => {
            let finished = std::mem::replace(&mut *op.borrow_mut(), DragOp::Idle);
            match finished {
                DragOp::Move { starts } => model::undo_stack().grouped("move", || {
                    field_commits_move(drag.translation.x / zf, drag.translation.y / zf, &starts)
                }),
                DragOp::Resize { id, corner, start } => {
                    let f = resized(
                        start,
                        corner,
                        drag.translation.x / zf,
                        drag.translation.y / zf,
                    );
                    model::undo_stack().grouped("resize", || apply_resize(id, f, true));
                }
                // A blank-canvas press that never really moved is a CLICK the tap recognizer
                // lost to the pan recognizer — act on it, or deselection (and placement)
                // silently fails on about half of real desktop clicks.
                DragOp::Idle => {
                    if drag.translation.x.hypot(drag.translation.y) <= 3.0 {
                        handle_click(drag.location, ClickSource::DragEnd);
                    }
                }
            }
        }
    }
}

pub(crate) fn editor_canvas() -> impl Piece {
    let op: Rc<RefCell<DragOp>> = Rc::new(RefCell::new(DragOp::Idle));
    let op2 = op.clone();
    // The pinch scales against the zoom captured at Began (Pinch.scale is cumulative), and
    // anchors at the gesture's START point — a moving anchor would feed the transform back
    // into itself.
    let pinch_base: Rc<Cell<(f64, Point)>> = Rc::new(Cell::new((1.0, Point::ZERO)));
    canvas(move |d, size| {
        VIEWPORT.with(|v| v.set((size.width, size.height)));
        draw_scene(d, size)
    })
    .on_tap_at(|p| handle_click(p, ClickSource::Tap))
    .on_drag(move |drag| on_drag(drag, &op2))
    .on_pinch(move |g| {
        if g.phase == DragPhase::Began {
            pinch_base.set((zoom().get_untracked(), g.location));
        }
        let (z0, anchor) = pinch_base.get();
        zoom_to(z0 * g.scale, anchor);
    })
    .on_pan(|g| {
        pan().update(|p| {
            p.x += g.delta.x;
            p.y += g.delta.y;
        });
    })
    .context_menu(crate::context_menu_entries())
    .id("canvas")
    .grow()
}
