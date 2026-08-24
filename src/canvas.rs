//! The editor canvas: rendering, hit testing, and the move/resize drag machine.
//!
//! The draw closure walks the scene bottom→top; every field it touches is a TRACKED read, so a
//! moved shape repaints with no wiring at all. Interaction comes in through the canvas's own
//! gesture decorators — `on_tap_at` for selection and tool placement, `on_drag` for move and
//! resize — and every live gesture writes the model through PREVIEWS: the store (and therefore
//! the canvas and the status readouts) follows the pointer, while nothing durable fires until
//! the pointer lifts, when committing all touched fields in ONE turn makes the whole drag one
//! undo step and one SQL statement per row (https://daybrite.dev/docs/model).

use crate::model::{self, Node, NodeFields, NodeKind};
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

/// Place a shape centered in the VIEWPORT — the visible middle of the scrolled canvas, in
/// model coordinates, so a zoomed or panned view still puts the new shape where the user is
/// looking. Selects it, the way every placement does.
pub(crate) fn place_centered(kind: NodeKind) {
    let c = to_model(viewport_center());
    // Offset by half the kind's own starting extent, so what lands is CENTERED rather than
    // hung off the middle by a rectangle's proportions.
    let (w, h) = match kind {
        NodeKind::Line => (model::DEFAULT_LINE, 0.0),
        NodeKind::Rect | NodeKind::Oval | NodeKind::Group => (model::DEFAULT_W, model::DEFAULT_H),
    };
    let id = model::place_shape(kind, c.x - w / 2.0, c.y - h / 2.0);
    model::selection().set(vec![id]);
}

fn to_model(p: Point) -> Point {
    let z = zoom().get_untracked();
    let pn = pan().get_untracked();
    Point::new((p.x - pn.x) / z, (p.y - pn.y) / z)
}

/// A model point's place on screen — the inverse of [`to_model`], for the selection overlay,
/// which is drawn in screen space so its weight and handle size stay constant at every zoom.
fn to_screen(p: Point) -> Point {
    let z = zoom().get_untracked();
    let pn = pan().get_untracked();
    Point::new(p.x * z + pn.x, p.y * z + pn.y)
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

/// Which end of a line a handle drives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineEnd {
    Start,
    End,
}

/// What the pointer grabbed: a rectangle's corner, or one end of a line. The two shapes of
/// handle a selection can wear — a line has no corners to resize, only two points to move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Handle {
    Corner(Corner),
    End(LineEnd),
}

enum DragOp {
    Idle,
    /// Every affected SHAPE with its starting origin (a group drags its descendants).
    Move {
        starts: Vec<(u64, f64, f64)>,
    },
    /// One shape, one corner, its starting frame, and the rotation that frame is drawn under.
    Resize {
        id: u64,
        corner: Corner,
        start: (f64, f64, f64, f64),
        rotation: f64,
    },
    /// One END of a line, with the line's starting fields. Moving a point, not resizing a
    /// frame: the other end stays exactly where it is.
    Endpoint {
        id: u64,
        end: LineEnd,
        start: (f64, f64, f64, f64),
    },
}

/// A line's fields after one of its ends has moved by (dx, dy) in model space. Moving the
/// START also moves the origin, and the deltas absorb the difference so the far end holds
/// still; moving the END is the deltas alone.
fn line_dragged(
    start: (f64, f64, f64, f64),
    end: LineEnd,
    dx: f64,
    dy: f64,
) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = start;
    match end {
        LineEnd::Start => (x + dx, y + dy, w - dx, h - dy),
        LineEnd::End => (x, y, w + dx, h + dy),
    }
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

/// The corner OPPOSITE `corner`, in the frame's own (unrotated) coordinates — the point a
/// resize holds still.
fn opposite_point(b: (f64, f64, f64, f64), corner: Corner) -> Point {
    let (x, y, w, h) = b;
    match corner {
        Corner::TopLeft => Point::new(x + w, y + h),
        Corner::TopRight => Point::new(x, y + h),
        Corner::BottomLeft => Point::new(x + w, y),
        Corner::BottomRight => Point::new(x, y),
    }
}

/// [`resized`] for a shape that is drawn TURNED.
///
/// Two corrections, both required for the handles to track the pointer once the frame no
/// longer lies along the screen axes. First the drag is read in the shape's OWN frame — a pull
/// along the shape's long edge lengthens it whatever angle that edge is at on screen. Then the
/// result is re-anchored: `resized` holds the opposite corner still in the frame's coordinates,
/// but the rotation is about the frame's CENTER, and the center moves as the frame grows — so
/// the shape would creep out from under the pointer. Shifting the new origin by the difference
/// between where that corner was and where it would land puts it back.
fn resized_under_rotation(
    start: (f64, f64, f64, f64),
    corner: Corner,
    dx: f64,
    dy: f64,
    rotation: f64,
) -> (f64, f64, f64, f64) {
    if rotation.abs() <= f64::EPSILON {
        return resized(start, corner, dx, dy);
    }
    let (sin, cos) = rotation.to_radians().sin_cos();
    // R(-θ) · (dx, dy): the pointer's travel in the shape's own frame.
    let f = resized(start, corner, dx * cos + dy * sin, -dx * sin + dy * cos);
    let held = |b: (f64, f64, f64, f64)| {
        let c = Point::new(b.0 + b.2 / 2.0, b.1 + b.3 / 2.0);
        let p = opposite_point(b, corner);
        let (ox, oy) = (p.x - c.x, p.y - c.y);
        Point::new(c.x + ox * cos - oy * sin, c.y + ox * sin + oy * cos)
    };
    let (was, now) = (held(start), held(f));
    (f.0 + was.x - now.x, f.1 + was.y - now.y, f.2, f.3)
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

/// How near the pointer must come to a line to count as on it, in MODEL units at 1:1 — a
/// stroke is thin, so the grab is the tolerance rather than the geometry.
const LINE_TOLERANCE: f64 = 6.0;

/// The distance from `p` to the segment `a`–`b`.
fn distance_to_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let len2 = vx * vx + vy * vy;
    // A zero-length line is a point.
    let t = if len2 <= f64::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / len2).clamp(0.0, 1.0)
    };
    (p.0 - (a.0 + vx * t)).hypot(p.1 - (a.1 + vy * t))
}

fn point_in_shape(store: Store<Keyed<Node>>, id: u64, px: f64, py: f64) -> bool {
    let e = store.elem(id);
    let (x, y, w, h) = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
    // A rotated shape is drawn through a transform, so the POINT rides the inverse back into
    // the shape's own upright space — where the frame tests below are the geometry again.
    let rot = e.rotation().peek();
    let (px, py) = if rot.abs() > f64::EPSILON {
        let p = rotation_about_center(x, y, w, h, -rot).apply(Point::new(px, py));
        (p.x, p.y)
    } else {
        (px, py)
    };
    match e.kind().peek() {
        // A line has no interior: near the segment IS on it, and the tolerance shrinks as the
        // view zooms in so the grab stays the same size under the pointer.
        NodeKind::Line => {
            let ((ax, ay), (bx, by)) = model::line_ends(id);
            distance_to_segment((px, py), (ax, ay), (bx, by))
                <= LINE_TOLERANCE / zoom().get_untracked()
        }
        NodeKind::Oval => {
            if px < x || py < y || px > x + w || py > y + h {
                return false;
            }
            let (cx, cy) = (x + w / 2.0, y + h / 2.0);
            let (nx, ny) = ((px - cx) / (w / 2.0), (py - cy) / (h / 2.0));
            nx * nx + ny * ny <= 1.0
        }
        // A group is never tested directly — `hit_top_level` walks its shape descendants —
        // but the arm is written out so a new kind must decide its own hit shape.
        NodeKind::Rect | NodeKind::Group => px >= x && py >= y && px <= x + w && py <= y + h,
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

/// A node's own rotation in degrees — 0 for a group, whose frame is the axis-aligned union of
/// its members and turns with none of them.
fn rotation_of(id: u64) -> f64 {
    let e = model::nodes().elem(id);
    match e.kind().peek() {
        NodeKind::Rect | NodeKind::Oval => e.rotation().peek(),
        // A group's frame is the axis-aligned union of its members and turns with none of
        // them; a line's direction IS its two endpoints, so it carries no separate angle
        // (and the inspector offers it none).
        NodeKind::Group | NodeKind::Line => 0.0,
    }
}

/// A line's two ends in SCREEN space — where its handles are drawn and grabbed.
fn screen_ends(id: u64) -> [(LineEnd, f64, f64); 2] {
    let ((ax, ay), (bx, by)) = model::line_ends(id);
    let a = to_screen(Point::new(ax, ay));
    let b = to_screen(Point::new(bx, by));
    [(LineEnd::Start, a.x, a.y), (LineEnd::End, b.x, b.y)]
}

/// A selected node's four corners in SCREEN space, turned by the shape's own rotation about
/// its center: the selection outline connects these, the handles sit on them, and
/// [`hit_handle`] grabs them — so all three follow the shape as it turns.
fn screen_corners(b: (f64, f64, f64, f64), rotation: f64) -> [(Corner, f64, f64); 4] {
    let mut pts = corner_points(b);
    let turn = (rotation.abs() > f64::EPSILON)
        .then(|| rotation_about_center(b.0, b.1, b.2, b.3, rotation));
    for c in pts.iter_mut() {
        let p = Point::new(c.1, c.2);
        let p = match &turn {
            Some(m) => m.apply(p),
            None => p,
        };
        let s = to_screen(p);
        (c.1, c.2) = (s.x, s.y);
    }
    pts
}

/// A corner handle of ANY selected shape under the SCREEN point: every selected shape
/// carries its own handles, and dragging one resizes THAT shape alone (a multi-select drag
/// anywhere else moves the whole selection). Groups have no handles — groups move; only
/// shapes resize. Later selections are checked first, matching the draw order (last drawn
/// sits on top). Handles live in screen space — their size and grab target stay constant at
/// every zoom, so the test transforms the corners, not the pointer.
fn hit_handle(px: f64, py: f64) -> Option<(u64, Handle)> {
    let sel = model::selection().get_untracked();
    let store = model::nodes();
    let near = |cx: f64, cy: f64| (px - cx).abs() <= HANDLE && (py - cy).abs() <= HANDLE;
    for id in sel.iter().rev() {
        match store.elem(*id).kind().peek() {
            // Groups move; only shapes carry handles.
            NodeKind::Group => continue,
            NodeKind::Line => {
                for (end, cx, cy) in screen_ends(*id) {
                    if near(cx, cy) {
                        return Some((*id, Handle::End(end)));
                    }
                }
            }
            NodeKind::Rect | NodeKind::Oval => {
                let Some(b) = model::node_bounds(*id) else {
                    continue;
                };
                for (corner, cx, cy) in screen_corners(b, rotation_of(*id)) {
                    if near(cx, cy) {
                        return Some((*id, Handle::Corner(corner)));
                    }
                }
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
        // Signed deltas: origin to far point, whichever way it runs.
        NodeKind::Line => segment_shape((node.x, node.y), (node.x + node.w, node.y + node.h)),
        // A group draws nothing of its own — its members draw themselves — but the arm is
        // written out so a new kind must say what it looks like.
        NodeKind::Rect | NodeKind::Group => {
            round_rect_shape(node.x, node.y, node.w, node.h, node.corner_radius)
        }
    }
}

/// An open path from `a` to `b` — a line's whole geometry.
fn segment_shape(a: (f64, f64), b: (f64, f64)) -> Shape {
    PathBuilder::new()
        .move_to(Point::new(a.0, a.1))
        .line_to(Point::new(b.0, b.1))
        .build()
}

/// A rectangle with rounded corners — plain when the radius is 0, and the radius clamped to
/// half the shorter side so the corners can never cross.
fn round_rect_shape(x: f64, y: f64, w: f64, h: f64, r: f64) -> Shape {
    let r = r.min(w / 2.0).min(h / 2.0);
    if r <= 0.0 {
        return rect_shape(x, y, w, h);
    }
    // The circular-arc kappa, as the ellipse below uses: a corner is a quarter circle.
    const K: f64 = 0.552_284_749_830_793_4;
    let c = r * K;
    let (r1, b1) = (x + w, y + h);
    PathBuilder::new()
        .move_to(Point::new(x + r, y))
        .line_to(Point::new(r1 - r, y))
        .cubic_to(
            Point::new(r1 - r + c, y),
            Point::new(r1, y + r - c),
            Point::new(r1, y + r),
        )
        .line_to(Point::new(r1, b1 - r))
        .cubic_to(
            Point::new(r1, b1 - r + c),
            Point::new(r1 - r + c, b1),
            Point::new(r1 - r, b1),
        )
        .line_to(Point::new(x + r, b1))
        .cubic_to(
            Point::new(x + r - c, b1),
            Point::new(x, b1 - r + c),
            Point::new(x, b1 - r),
        )
        .line_to(Point::new(x, y + r))
        .cubic_to(
            Point::new(x, y + r - c),
            Point::new(x + r - c, y),
            Point::new(x + r, y),
        )
        .close()
        .build()
}

/// A shape's rotation as an affine about its own center — the transform the draw concats and
/// the hit test inverts.
fn rotation_about_center(x: f64, y: f64, w: f64, h: f64, degrees: f64) -> Affine {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    Affine::translate(-cx, -cy)
        .then(Affine::rotate(degrees.to_radians()))
        .then(Affine::translate(cx, cy))
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

/// One drag handle: a small square centered on a corner, turned by the shape's rotation so
/// the four read as the corners of one turned frame rather than as loose pins. Screen space,
/// so its SIZE is the same at every zoom.
fn handle_shape(cx: f64, cy: f64, rotation: f64) -> Shape {
    let half = HANDLE_DRAW / 2.0;
    let (sin, cos) = rotation.to_radians().sin_cos();
    let at = |dx: f64, dy: f64| Point::new(cx + dx * cos - dy * sin, cy + dx * sin + dy * cos);
    PathBuilder::new()
        .move_to(at(-half, -half))
        .line_to(at(half, -half))
        .line_to(at(half, half))
        .line_to(at(-half, half))
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
            let kind = e.kind().with(|k| k.copied().unwrap_or_default());
            match kind {
                NodeKind::Group => draw_children(d, store, Some(id)),
                NodeKind::Rect | NodeKind::Oval | NodeKind::Line => {
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
                        rotation: e.rotation().read(),
                        corner_radius: e.corner_radius().read(),
                    };
                    let paint = |d: &mut Draw| {
                        // A line has no interior to fill — it IS its stroke. Everything else
                        // fills, then strokes its outline.
                        let fills = match node.kind {
                            NodeKind::Rect | NodeKind::Oval | NodeKind::Group => true,
                            NodeKind::Line => false,
                        };
                        if fills {
                            d.fill(
                                shape_of(&node),
                                fill_color(&node.fill).with_alpha(node.fill_opacity),
                            );
                        }
                        if node.stroke_width > 0.0 && node.stroke_opacity > 0.0 {
                            d.stroke(
                                shape_of(&node),
                                fill_color(&node.stroke).with_alpha(node.stroke_opacity),
                                node.stroke_width,
                            );
                        }
                    };
                    // A rotation is a transform about the shape's own center, so the stored
                    // frame stays axis-aligned — what the inspector edits and the selection
                    // outline draws.
                    if node.rotation.abs() > f64::EPSILON {
                        d.transformed(
                            rotation_about_center(node.x, node.y, node.w, node.h, node.rotation),
                            paint,
                        );
                    } else {
                        paint(d);
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
    // transformed corners — constant weight and size at every zoom, and TURNED with the shape
    // so a rotated rectangle wears a rotated outline rather than its bounding box. Every
    // selected SHAPE carries its own handles — multi-selections included; a group shows only
    // its union outline, which turns with nothing (its members rotate, it does not).
    let sel = model::selection().get();
    for id in &sel {
        let Some(b) = model::node_bounds(*id) else {
            continue;
        };
        // TRACKED, like every other field the scene draws from: turning a shape (or dragging
        // an end of a line) must repaint its outline in the same frame.
        let rotation = store.elem(*id).rotation().read();
        let kind = store
            .elem(*id)
            .kind()
            .with(|k| k.copied().unwrap_or_default());
        // What a selection LOOKS like is per kind, so a new one has to say for itself.
        let handles = match kind {
            // A line wears its own selection: the segment itself, marked at both ends. A
            // frame around it would say "resize me" about a shape that has no frame to
            // resize.
            NodeKind::Line => {
                // TRACKED reads of the raw fields — a dragged endpoint previews through
                // them, so the overlay must follow the same frame the segment does.
                let e = store.elem(*id);
                let (x, y, w, h) = (e.x().read(), e.y().read(), e.w().read(), e.h().read());
                let a = to_screen(Point::new(x, y));
                let z = to_screen(Point::new(x + w, y + h));
                let ends = [(a.x, a.y), (z.x, z.y)];
                d.stroke(segment_shape(ends[0], ends[1]), SELECTION, 1.5);
                for (cx, cy) in ends {
                    let handle = handle_shape(cx, cy, 0.0);
                    d.fill(handle.clone(), HANDLE_FILL);
                    d.stroke(handle, HANDLE_STROKE, 1.0);
                }
                continue;
            }
            // A frame, with corner handles to resize by.
            NodeKind::Rect | NodeKind::Oval => true,
            // A group shows the union it occupies and no handles: groups move, their members
            // resize.
            NodeKind::Group => false,
        };
        let pts = screen_corners(b, if handles { rotation } else { 0.0 });
        let p = |i: usize| Point::new(pts[i].1, pts[i].2);
        // Ring order: the corners come back TL, TR, BL, BR.
        d.stroke(
            PathBuilder::new()
                .move_to(p(0))
                .line_to(p(1))
                .line_to(p(3))
                .line_to(p(2))
                .close()
                .build(),
            SELECTION,
            1.5,
        );
        if handles {
            for (_, cx, cy) in pts {
                let handle = handle_shape(cx, cy, rotation);
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

/// `p` is in MODEL space — callers convert from screen coordinates first. A canvas tap always
/// SELECTS: shapes are placed from the toolbar's shape menu (centered in the viewport), so
/// there is no armed-tool mode to be in.
fn on_tap(p: Point) {
    select_at(p, day::modifiers());
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
            let next = if let Some((id, handle)) = hit_handle(p.x, p.y) {
                let e = store.elem(id);
                let start = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
                match handle {
                    Handle::Corner(corner) => DragOp::Resize {
                        id,
                        corner,
                        start,
                        rotation: rotation_of(id),
                    },
                    Handle::End(end) => DragOp::Endpoint { id, end, start },
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
            DragOp::Resize {
                id,
                corner,
                start,
                rotation,
            } => {
                let f = resized_under_rotation(
                    *start,
                    *corner,
                    drag.translation.x / zf,
                    drag.translation.y / zf,
                    *rotation,
                );
                apply_resize(*id, f, false);
            }
            DragOp::Endpoint { id, end, start } => {
                let f = line_dragged(
                    *start,
                    *end,
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
                DragOp::Resize {
                    id,
                    corner,
                    start,
                    rotation,
                } => {
                    let f = resized_under_rotation(
                        start,
                        corner,
                        drag.translation.x / zf,
                        drag.translation.y / zf,
                        rotation,
                    );
                    model::undo_stack().grouped("resize", || apply_resize(id, f, true));
                }
                DragOp::Endpoint { id, end, start } => {
                    let f =
                        line_dragged(start, end, drag.translation.x / zf, drag.translation.y / zf);
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

/// Whether the canvas holds the keyboard (docs/focus.md). Bound two-way: it takes focus when
/// the editor mounts and on every press, and gives it up the moment a text field or the
/// inspector takes it — which is exactly when the arrows have to stop nudging shapes and go
/// back to moving a caret.
pub(crate) fn canvas_focused() -> Signal<bool> {
    thread_local! {
        static F: Signal<bool> = Signal::global(true);
    }
    F.with(|s| *s)
}

/// Nudge the selection with the arrow keys: 1px, or 10 with shift (docs/menus.md). Hung on the
/// canvas rather than the window, so it can only fire while the canvas is the focused piece.
fn nudge_by_key(ev: &day::KeyEvent) {
    let (dx, dy) = match ev.key.as_str() {
        "ArrowLeft" => (-1.0, 0.0),
        "ArrowRight" => (1.0, 0.0),
        "ArrowUp" => (0.0, -1.0),
        "ArrowDown" => (0.0, 1.0),
        _ => return,
    };
    let step = if ev.shift() { 10.0 } else { 1.0 };
    model::nudge_selection(dx * step, dy * step);
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
    .on_key(nudge_by_key)
    .focused(canvas_focused())
    .context_menu(crate::context_menu_entries())
    .id("canvas")
    .grow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::install_test_doc;

    /// A 96×64 rectangle at the origin, selected, turned by `degrees`.
    fn turned(degrees: f64) -> u64 {
        let id = model::place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        model::nodes().elem(id).rotation().write(degrees);
        day::reactive::flush_sync();
        model::selection().set(vec![id]);
        id
    }

    fn corner(id: u64, want: Corner) -> (f64, f64) {
        let b = model::node_bounds(id).expect("a shape has bounds");
        let pts = screen_corners(b, rotation_of(id));
        let (_, x, y) = pts.iter().find(|(c, _, _)| *c == want).copied().unwrap();
        (x, y)
    }

    #[test]
    fn handles_sit_on_the_turned_corners_and_are_grabbable_there() {
        let _doc = install_test_doc();
        let id = turned(0.0);
        assert_eq!(corner(id, Corner::BottomRight), (96.0, 64.0));

        // A quarter turn about the center (48, 32): the bottom-right corner swings to
        // (48 - 32, 32 + 48) = (16, 80).
        model::nodes().elem(id).rotation().write(90.0);
        day::reactive::flush_sync();
        let (bx, by) = corner(id, Corner::BottomRight);
        assert!(
            (bx - 16.0).abs() < 0.001 && (by - 80.0).abs() < 0.001,
            "{bx},{by}"
        );

        // The grab follows the drawing: the turned position hits, the old one does not.
        assert_eq!(
            hit_handle(bx, by),
            Some((id, Handle::Corner(Corner::BottomRight)))
        );
        assert_eq!(
            hit_handle(96.0, 64.0),
            None,
            "the un-turned corner is bare canvas"
        );
    }

    #[test]
    fn a_line_carries_two_endpoint_handles_and_no_corners() {
        let _doc = install_test_doc();
        let id = model::place_shape(NodeKind::Line, 10.0, 20.0);
        day::reactive::flush_sync();
        let e = model::nodes().elem(id);
        e.w().write(60.0);
        e.h().write(40.0);
        day::reactive::flush_sync();
        model::selection().set(vec![id]);

        // Its handles are the two ENDS…
        assert_eq!(
            hit_handle(10.0, 20.0),
            Some((id, Handle::End(LineEnd::Start)))
        );
        assert_eq!(
            hit_handle(70.0, 60.0),
            Some((id, Handle::End(LineEnd::End)))
        );
        // …and the corners of the box it happens to occupy are not handles at all.
        assert_eq!(hit_handle(70.0, 20.0), None, "no corner handles on a line");
    }

    #[test]
    fn dragging_one_end_of_a_line_leaves_the_other_alone() {
        let start = (10.0, 20.0, 60.0, 40.0); // (10,20) → (70,60)

        // The END follows the pointer; the start is untouched.
        let f = line_dragged(start, LineEnd::End, 5.0, -10.0);
        assert_eq!(f, (10.0, 20.0, 65.0, 30.0));

        // Moving the START moves the origin, and the deltas absorb it so the far end holds
        // exactly still.
        let f = line_dragged(start, LineEnd::Start, 5.0, -10.0);
        assert_eq!(f, (15.0, 10.0, 55.0, 50.0));
        assert_eq!((f.0 + f.2, f.1 + f.3), (70.0, 60.0), "the far end is fixed");

        // A drag past the far end simply flips the direction — the fields go negative and the
        // frame normalizes, rather than the line refusing to cross itself.
        let f = line_dragged(start, LineEnd::Start, 100.0, 0.0);
        assert!(f.2 < 0.0, "{f:?}");
    }

    #[test]
    fn a_group_outline_does_not_turn_with_its_members() {
        let _doc = install_test_doc();
        let a = turned(90.0);
        let b = model::place_shape(NodeKind::Rect, 200.0, 0.0);
        day::reactive::flush_sync();
        model::selection().set(vec![a, b]);
        model::group_selection();
        day::reactive::flush_sync();
        let gid = model::selection().get_untracked()[0];
        assert_eq!(rotation_of(gid), 0.0, "a group has no rotation of its own");
        let bounds = model::node_bounds(gid).expect("union");
        assert_eq!(
            screen_corners(bounds, rotation_of(gid)),
            corner_points(bounds)
        );
    }

    #[test]
    fn dragging_a_turned_handle_resizes_along_the_shapes_own_edge() {
        let _doc = install_test_doc();
        let id = turned(90.0);
        let start = (0.0, 0.0, 96.0, 64.0);

        // Under a quarter turn the shape's own +x axis points DOWN the screen, so a downward
        // pull is the one that lengthens it — the same gesture that widened it upright.
        let f = resized_under_rotation(start, Corner::BottomRight, 0.0, 20.0, 90.0);
        assert!(
            (f.2 - 116.0).abs() < 0.001,
            "width followed the pointer: {f:?}"
        );
        assert!((f.3 - 64.0).abs() < 0.001, "height untouched: {f:?}");

        // …and the corner opposite the dragged one stays exactly where it was drawn, so the
        // shape grows out from under the pointer rather than sliding.
        let held = |b: (f64, f64, f64, f64)| {
            let pts = screen_corners(b, 90.0);
            let (_, x, y) = pts
                .iter()
                .find(|(c, _, _)| *c == Corner::TopLeft)
                .copied()
                .unwrap();
            (x, y)
        };
        let (wx, wy) = held(start);
        let (nx, ny) = held(f);
        assert!(
            (wx - nx).abs() < 0.001 && (wy - ny).abs() < 0.001,
            "{wx},{wy} vs {nx},{ny}"
        );
    }
}
