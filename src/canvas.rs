//! The editor canvas: rendering, hit testing, and the drag machine behind moving, resizing and
//! rubber-band selection.
//!
//! The draw closure walks the scene bottom→top; every field it touches is a TRACKED read, so a
//! moved shape repaints with no wiring at all. Interaction comes in through the canvas's own
//! gesture decorators — `on_tap_at` for selection and tool placement, `on_drag` for move,
//! resize and the band — and every live gesture that EDITS writes the model through PREVIEWS:
//! the store (and therefore the canvas and the status readouts) follows the pointer, while
//! nothing durable fires until the pointer lifts, when committing all touched fields in ONE
//! turn makes the whole drag one undo step and one statement per table
//! (https://daybrite.dev/docs/model). A band edits nothing at all: it writes the selection,
//! which lives outside the store, so sweeping the canvas is neither a row nor an undo step.

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
    /// The previous TAP, for the double-click drill (see [`drill_at`]): a second tap within
    /// the window and slop below counts as a REPEAT of the first.
    static LAST_TAP: Cell<Option<(std::time::Instant, f64, f64)>> = const { Cell::new(None) };
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

/// The rubber band the pointer is currently sweeping, in SCREEN space as `(x, y, w, h)`, or
/// `None` between sweeps. Editor state like the selection, and for the same reason: a band is
/// a way of pointing at shapes, not a thing the document contains, so it is never a row and
/// never an undo step. Written on every pointer move; the draw closure reads it TRACKED, which
/// is the whole of the repaint wiring.
fn band() -> Signal<Option<(f64, f64, f64, f64)>> {
    thread_local! {
        static B: Signal<Option<(f64, f64, f64, f64)>> = Signal::global(None);
    }
    B.with(|s| *s)
}

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
    /// A rubber band sweeping the blank canvas. `anchor` is where the press landed, in SCREEN
    /// space — the corner the band grows from. `base` is the selection the press started with,
    /// kept whole: a plain sweep starts from nothing and replaces, while a shift- or
    /// command-sweep starts from what was already selected and ADDS to it, which is one rule
    /// rather than two because an empty base makes "add" and "replace" the same thing.
    Band {
        anchor: Point,
        base: Vec<u64>,
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

/// Seal one field of a finished gesture: a record only if the field actually moved, so the
/// turn's `UPDATE` carries only the columns that have something to say and a gesture ending
/// where it began is not an undo step at all.
///
/// An unmoved field is CANCELLED, not merely skipped. Every frame of the drag previewed it,
/// so the store holds the last previewed value, and a session that is neither committed nor
/// cancelled leaves it there — the shape would stay where the pointer passed rather than
/// where it belongs, and the open session would still be sitting in the preview map. Cancel
/// puts the pre-session value back, which for an unmoved field is the value it should have,
/// and closes the session. Cancelling a field that never opened one is a no-op.
fn seal<S: Source<Node>>(f: Field<S, Node, f64>, start: f64, end: f64) {
    if end == start {
        f.session().cancel();
    } else {
        f.write_commit(end);
    }
}

fn field_commits_move(dx: f64, dy: f64, starts: &[(u64, f64, f64)]) {
    let store = model::nodes();
    for (id, sx, sy) in starts {
        let e = store.elem(*id);
        seal(e.x(), *sx, sx + dx);
        seal(e.y(), *sy, sy + dy);
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

/// `start` is the frame the gesture began on, and only the commit reads it: the fields that
/// still hold their starting value are sealed rather than recorded. Dragging a bottom-right
/// corner moves w and h and leaves x and y exactly where they were, so that resize is an
/// `UPDATE` of two columns rather than four.
///
/// The PREVIEW pass always writes all four. A preview must be able to put a coordinate BACK —
/// drag a top-left corner out and return it, and x has to follow the pointer home — so a
/// preview that skipped an unchanged field would strand the previous frame's value on screen.
fn apply_resize(id: u64, frame: (f64, f64, f64, f64), start: (f64, f64, f64, f64), commit: bool) {
    let store = model::nodes();
    let e = store.elem(id);
    if commit {
        // Sealed in ONE turn: one undo unit, one UPDATE of however many columns moved.
        seal(e.x(), start.0, frame.0);
        seal(e.y(), start.1, frame.1);
        seal(e.w(), start.2, frame.2);
        seal(e.h(), start.3, frame.3);
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

/// Do two convex polygons overlap? The separating-axis test over both outlines' edge normals:
/// if any axis has them projecting to disjoint intervals they are apart, and if none does they
/// touch. Exact for the rectangles and quads below, and it holds for a two-point "polygon" — a
/// line segment — whose single edge still yields the axis that would separate it.
fn convex_overlap(a: &[Point], b: &[Point]) -> bool {
    let project = |poly: &[Point], ax: Point| {
        poly.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| {
            let d = p.x * ax.x + p.y * ax.y;
            (lo.min(d), hi.max(d))
        })
    };
    for poly in [a, b] {
        for i in 0..poly.len() {
            let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
            // The edge's normal. A repeated point contributes no axis to test.
            let ax = Point::new(p.y - q.y, q.x - p.x);
            if ax.x.abs() <= f64::EPSILON && ax.y.abs() <= f64::EPSILON {
                continue;
            }
            let ((a0, a1), (b0, b1)) = (project(a, ax), project(b, ax));
            if a1 < b0 || b1 < a0 {
                return false;
            }
        }
    }
    true
}

/// Is `p` inside a convex polygon? Every edge turns the same way about an interior point, so a
/// sign change across the cross products means outside.
fn point_in_convex(poly: &[Point], p: Point) -> bool {
    let mut sign = 0.0;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let cross = (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
        if cross.abs() <= f64::EPSILON {
            continue;
        }
        if sign * cross < 0.0 {
            return false;
        }
        sign = cross;
    }
    true
}

/// Does the band touch this SHAPE as drawn? The band arrives as its four model-space corners;
/// a rotated shape rides them back through the inverse rotation into its own upright space —
/// exactly as [`point_in_shape`] rides the pointer — so each kind's test is plain geometry
/// again. Every kind answers for the ink it actually lays down, which matters most for a line:
/// its frame is the box its two ends span, mostly empty for a diagonal, and a band that merely
/// entered that box has not touched the line.
fn shape_touches(id: u64, band: &[Point; 4]) -> bool {
    let e = model::nodes().elem(id);
    let (x, y, w, h) = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
    let rot = e.rotation().peek();
    let quad = if rot.abs() > f64::EPSILON {
        let m = rotation_about_center(x, y, w, h, -rot);
        band.map(|p| m.apply(p))
    } else {
        *band
    };
    match e.kind().peek() {
        // The segment itself, not the box around it.
        NodeKind::Line => {
            let ((ax, ay), (bx, by)) = model::line_ends(id);
            convex_overlap(&[Point::new(ax, ay), Point::new(bx, by)], &quad)
        }
        // Scale the ellipse's own space until it is a circle and the quad rides along, then the
        // question is the distance from that circle's center to the quad. The frame's four
        // corners lie outside the ellipse, so a band clipping only a corner touches nothing.
        NodeKind::Oval => {
            if w.abs() <= f64::EPSILON || h.abs() <= f64::EPSILON {
                return false;
            }
            let (cx, cy) = (x + w / 2.0, y + h / 2.0);
            let k = w / h;
            let scaled = quad.map(|p| Point::new(p.x, cy + (p.y - cy) * k));
            let c = Point::new(cx, cy);
            if point_in_convex(&scaled, c) {
                return true;
            }
            let r = (w / 2.0).abs();
            (0..4).any(|i| {
                let (a, b) = (scaled[i], scaled[(i + 1) % 4]);
                distance_to_segment((c.x, c.y), (a.x, a.y), (b.x, b.y)) <= r
            })
        }
        // A group is never tested directly — the walk below descends to its shapes — but the
        // arm is written out so a new kind must decide its own hit shape.
        NodeKind::Rect | NodeKind::Group => {
            let frame = [
                Point::new(x, y),
                Point::new(x + w, y),
                Point::new(x + w, y + h),
                Point::new(x, y + h),
            ];
            convex_overlap(&quad, &frame)
        }
    }
}

/// Every top-level node a SCREEN-space band touches, bottom→top. Touching, not enclosing: a
/// shape joins the selection as soon as the band reaches any part of it, which is what a
/// marquee means everywhere else and what makes a small sweep across a crowded drawing useful.
/// A group answers for its members — touch any one of them and the group is selected, the same
/// rule a click on a member follows. The band converts to model space once here rather than
/// once per shape.
fn touched_by(band: (f64, f64, f64, f64)) -> Vec<u64> {
    let a = to_model(Point::new(band.0, band.1));
    let z = to_model(Point::new(band.0 + band.2, band.1 + band.3));
    let (l, t) = (a.x.min(z.x), a.y.min(z.y));
    let (r, b) = (a.x.max(z.x), a.y.max(z.y));
    let corners = [
        Point::new(l, t),
        Point::new(r, t),
        Point::new(r, b),
        Point::new(l, b),
    ];
    model::children_of(None)
        .into_iter()
        .filter(|id| {
            model::shape_descendants(*id)
                .into_iter()
                .any(|s| shape_touches(s, &corners))
        })
        .collect()
}

/// Advance a sweep that began at `anchor` and has moved by `(dx, dy)` in screen pixels: set
/// the selection the band now makes, and return the band itself for the caller to show or
/// drop. Both the live phase and the release run through here, so the set the pointer lifts on
/// is exactly the set the band was showing.
fn sweep(anchor: Point, base: &[u64], dx: f64, dy: f64) -> (f64, f64, f64, f64) {
    let rect = (
        anchor.x.min(anchor.x + dx),
        anchor.y.min(anchor.y + dy),
        dx.abs(),
        dy.abs(),
    );
    let mut next = base.to_vec();
    for id in touched_by(rect) {
        if !next.contains(&id) {
            next.push(id);
        }
    }
    // Only on a real change: a sweep reports many moves that enclose the same shapes, and the
    // inspector rebuilds on every selection write.
    if model::selection().get_untracked() != next {
        model::selection().set(next);
    }
    rect
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
        // TRACKED order: an arrange writes the child's z, the relation index reorders, and
        // this read wakes — one dependency per parent, not one per node in the document.
        for id in crate::model::children_of(parent) {
            let e = store.elem(id);
            let kind = e.kind().with(|k| k.copied().unwrap_or_default());
            match kind {
                NodeKind::Group => draw_children(d, store, Some(id)),
                NodeKind::Rect | NodeKind::Oval | NodeKind::Line => {
                    let node = Node {
                        id,
                        children: day::persistence::Many::default(),
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
        // What a selection LOOKS like is per kind, so a new one has to say for itself. The
        // OUTLINE turns for every kind that carries an angle, groups included — a turned
        // arrangement wearing an upright box would look like the box had come loose from it.
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
            // A group shows the frame it occupies and no handles: groups move, their members
            // resize.
            NodeKind::Group => false,
        };
        let pts = screen_corners(b, rotation);
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

    // The rubber band, over everything including the outlines it is drawing: a faint wash
    // under a dashed edge, the marquee every drawing program wears. Screen space, like the
    // outlines and for the same reason — one pixel of dash means one pixel at any zoom.
    // TRACKED: this read is what repaints the canvas as the band grows.
    if let Some((x, y, w, h)) = band().get() {
        let r = rect_shape(x, y, w, h);
        d.fill(r.clone(), SELECTION.with_alpha(0.10));
        d.stroke_styled(r, SELECTION, StrokeStyle::dashed(1.0, vec![4.0, 4.0]));
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
fn handle_click(p_screen: Point, mods: day::Modifiers, source: ClickSource) {
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
    // Is this tap the SECOND arrival of a double-click? Same spot, within the window —
    // that pair's second click drills into a selected group (see [`drill_at`]). A drag's
    // stationary-click report never pairs, and clears the memory so the next tap starts
    // a fresh pair.
    #[cfg(not(target_arch = "wasm32"))]
    let repeat = match source {
        ClickSource::Tap => {
            let now = std::time::Instant::now();
            let repeat = LAST_TAP.with(|c| c.get()).is_some_and(|(t, x, y)| {
                now.duration_since(t).as_millis() < 350
                    && (p_screen.x - x).abs() < 5.0
                    && (p_screen.y - y).abs() < 5.0
            });
            LAST_TAP.with(|c| c.set(Some((now, p_screen.x, p_screen.y))));
            repeat
        }
        ClickSource::DragEnd => {
            LAST_TAP.with(|c| c.set(None));
            false
        }
    };
    // No clock on wasm (std::time is unavailable): every tap may drill. The drill only fires
    // when the hit already sits inside the SOLE selected node, so a first click never drills
    // — the cost is that a slow second click on a selected group drills where desktop would
    // require a true double-click.
    #[cfg(target_arch = "wasm32")]
    let repeat = matches!(source, ClickSource::Tap);
    on_tap(to_model(p_screen), mods, repeat);
}

/// `p` is in MODEL space — callers convert from screen coordinates first. A canvas tap always
/// SELECTS: shapes are placed from the toolbar's shape menu (centered in the viewport), so
/// there is no armed-tool mode to be in. A REPEAT plain tap (a double-click's second arrival)
/// drills into a selected group instead.
fn on_tap(p: Point, mods: day::Modifiers, repeat: bool) {
    if repeat && !(mods.shift || mods.primary) && drill_at(p) {
        return;
    }
    select_at(p, mods);
}

/// The double-click drill: with the shape under `p` inside the SOLE selected node, select one
/// level deeper toward it — group, sub-group, then shape, one level per double-click. Once
/// the shape itself is the sole selection a repeat click HOLDS it (consuming the click)
/// rather than letting the plain rule bounce the selection back to the top level. Returns
/// whether the click was consumed.
pub(crate) fn drill_at(p: Point) -> bool {
    let Some(leaf) = hit_leaf(p.x, p.y) else {
        return false;
    };
    let sel = model::selection().get_untracked();
    let [sole] = sel[..] else { return false };
    if sole == leaf {
        return true;
    }
    match model::child_toward(sole, leaf) {
        Some(next) => {
            model::selection().set(vec![next]);
            true
        }
        None => false,
    }
}

/// The deepest SHAPE under the point — the drill's target: the same top-down z walk as
/// [`hit_top_level`], descended into groups.
fn hit_leaf(px: f64, py: f64) -> Option<u64> {
    fn descend(store: Store<Keyed<Node>>, id: u64, px: f64, py: f64) -> Option<u64> {
        if store.elem(id).kind().peek() == NodeKind::Group {
            model::children_of(Some(id))
                .into_iter()
                .rev()
                .find_map(|c| descend(store, c, px, py))
        } else {
            point_in_shape(store, id, px, py).then_some(id)
        }
    }
    let store = model::nodes();
    model::children_of(None)
        .into_iter()
        .rev()
        .find_map(|top| descend(store, top, px, py))
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

fn on_drag(drag: Drag, mods: day::Modifiers, op: &Rc<RefCell<DragOp>>) {
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
                // Dragging an unselected shape selects it first, then moves the WHOLE
                // selection if the hit is part of it. Shift or command ADDS it rather than
                // replacing — the rule a click already follows, so a modified press can never
                // throw away what was selected before it. "Part of it" is subtree-deep: a
                // double-click-drilled member drags ALONE rather than snapping back to its
                // group.
                let mut sel = model::selection().get_untracked();
                let covered = hit_leaf(m.x, m.y)
                    .is_some_and(|leaf| sel.iter().any(|s| model::is_within(leaf, *s)));
                if !covered {
                    if mods.shift || mods.primary {
                        sel.push(top);
                    } else {
                        sel = vec![top];
                    }
                    model::selection().set(sel.clone());
                }
                let mut starts = Vec::new();
                for t in &sel {
                    // The shapes only: a group's bounds are DERIVED from its members
                    // (`node_bounds`), so moving them is what moves its outline.
                    for s in model::shape_descendants(*t) {
                        let e = store.elem(s);
                        starts.push((s, e.x().peek(), e.y().peek()));
                    }
                }
                DragOp::Move { starts }
            } else {
                // Blank canvas: sweep a band. Shift or the platform's command key keeps what
                // was already selected and adds to it — the same modifier rule a click follows.
                let base = if mods.shift || mods.primary {
                    model::selection().get_untracked()
                } else {
                    Vec::new()
                };
                // Anchor at the PRESS, which is not always where `Began` is reported: appkit
                // raises it on the first drag event, already carrying the translation from
                // the press. Subtracting that back out lands on the point the pointer went
                // down at on every backend — anchoring on `location` alone would count the
                // opening move twice and leave the band trailing the pointer.
                let anchor = Point::new(p.x - drag.translation.x, p.y - drag.translation.y);
                DragOp::Band { anchor, base }
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
                apply_resize(*id, f, *start, false);
            }
            DragOp::Endpoint { id, end, start } => {
                let f = line_dragged(
                    *start,
                    *end,
                    drag.translation.x / zf,
                    drag.translation.y / zf,
                );
                apply_resize(*id, f, *start, false);
            }
            DragOp::Band { anchor, base } => {
                let rect = sweep(*anchor, base, drag.translation.x, drag.translation.y);
                band().set(Some(rect));
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
                    model::undo_stack().grouped("resize", || apply_resize(id, f, start, true));
                }
                DragOp::Endpoint { id, end, start } => {
                    let f =
                        line_dragged(start, end, drag.translation.x / zf, drag.translation.y / zf);
                    model::undo_stack().grouped("resize", || apply_resize(id, f, start, true));
                }
                // The band goes away on release whatever it caught; the selection it made is
                // already in place. A press that never really moved is a CLICK the tap
                // recognizer lost to the pan recognizer — act on it, or deselection silently
                // fails on about half of real desktop clicks. A hair of travel may already
                // have swept a band, so the click starts from the selection the press did.
                DragOp::Band { anchor, base } => {
                    band().set(None);
                    if drag.translation.x.hypot(drag.translation.y) <= 3.0 {
                        model::selection().set(base);
                        handle_click(drag.location, mods, ClickSource::DragEnd);
                    } else {
                        sweep(anchor, &base, drag.translation.x, drag.translation.y);
                    }
                }
                // Reachable only on a backend that ends a drag it never began; the click
                // fallback above lives in `Band` now, since that is where a blank press lands.
                DragOp::Idle => {}
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

/// The canvas's keyboard: arrows nudge, Delete removes. Hung on the canvas rather than the
/// window, so it can only fire while the canvas is the focused piece — a text field taking
/// focus takes the keys with it, which is what stops Delete from eating a shape while someone
/// edits a hex value (docs/focus.md).
/// `owns_delete` says whether Delete is this handler's to act on: true only where the platform
/// draws no menu bar. On the four that do, Edit ▸ Delete carries the same accelerator and the
/// platform fires it BEFORE the key reaches a focused view, so acting here as well would run
/// the command twice. Backends without a menu bar — web-dom above all — never see that
/// accelerator, and this is the only route the key has. Read at the piece and passed in as
/// data, like the gesture modifiers, so the decision is testable without a toolkit under it.
pub(crate) fn canvas_key(ev: &day::KeyEvent, owns_delete: bool) {
    match ev.key.as_str() {
        // Both spellings: a full-size keyboard's Del arrives as "Delete", a laptop's ⌫ as
        // "Backspace", and both mean "remove this" on a canvas.
        "Delete" | "Backspace" if owns_delete => model::delete_selection(),
        "Delete" | "Backspace" => {}
        _ => nudge_by_key(ev),
    }
}

/// Nudge the selection with the arrow keys: 1px, or 10 with shift (docs/menus.md).
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
    // The live modifiers are read HERE, at the edge, and travel into the machine as data:
    // shift-drag and shift-click both change meaning, and a handler that reads them itself
    // could only ever run with a toolkit under it.
    .on_tap_at(|p| handle_click(p, day::modifiers(), ClickSource::Tap))
    .on_drag(move |drag| on_drag(drag, day::modifiers(), &op2))
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
    // The menu bar's presence is a static per-backend fact, so it is read once here rather
    // than on every keystroke.
    .on_key(|ev| canvas_key(ev, capability(Cap::AppMenu) == Support::Unsupported))
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

    // -----------------------------------------------------------------------
    // Rubber-band selection
    // -----------------------------------------------------------------------

    /// A settled view: screen coordinates equal model coordinates, so a band test reads as the
    /// geometry it is about. The view transform is thread-local and a sibling test may have
    /// zoomed it.
    fn unzoomed() {
        zoom().set(1.0);
        pan().set(Point::ZERO);
        band().set(None);
        model::selection().set(Vec::new());
    }

    /// A rectangle of a given size at a given place — the shapes a band has to tell apart.
    fn rect_at(x: f64, y: f64, w: f64, h: f64) -> u64 {
        let id = model::place_shape(NodeKind::Rect, x, y);
        day::reactive::flush_sync();
        let e = model::nodes().elem(id);
        e.w().write(w);
        e.h().write(h);
        day::reactive::flush_sync();
        id
    }

    /// Drive one whole sweep through the real drag machine: press at `from`, move to `to`,
    /// release there. Screen coordinates, the way the toolkit delivers them.
    fn sweep_gesture(from: (f64, f64), to: (f64, f64), mods: day::Modifiers) {
        let op = Rc::new(RefCell::new(DragOp::Idle));
        let at = |phase, (x, y): (f64, f64)| Drag {
            phase,
            location: Point::new(x, y),
            translation: Point::new(x - from.0, y - from.1),
        };
        on_drag(at(DragPhase::Began, from), mods, &op);
        on_drag(at(DragPhase::Changed, to), mods, &op);
        on_drag(at(DragPhase::Ended, to), mods, &op);
    }

    fn plain() -> day::Modifiers {
        day::Modifiers::default()
    }

    fn shift() -> day::Modifiers {
        day::Modifiers {
            shift: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_band_selects_every_shape_it_encloses() {
        let _doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 40.0, 40.0);
        let b = rect_at(80.0, 10.0, 40.0, 40.0);
        let far = rect_at(400.0, 400.0, 40.0, 40.0);

        sweep_gesture((0.0, 0.0), (200.0, 200.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![a, b]);
        assert_eq!(
            band().get_untracked(),
            None,
            "the band goes away on release"
        );

        // And a band drawn elsewhere takes the shape that is elsewhere.
        sweep_gesture((380.0, 380.0), (500.0, 500.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![far]);
    }

    #[test]
    fn a_band_grown_in_any_direction_covers_the_same_ground() {
        let _doc = install_test_doc();
        unzoomed();
        let id = rect_at(10.0, 10.0, 40.0, 40.0);
        // All four drag directions over the same rectangle of screen.
        for (from, to) in [
            ((0.0, 0.0), (100.0, 100.0)),
            ((100.0, 100.0), (0.0, 0.0)),
            ((100.0, 0.0), (0.0, 100.0)),
            ((0.0, 100.0), (100.0, 0.0)),
        ] {
            model::selection().set(Vec::new());
            sweep_gesture(from, to, plain());
            assert_eq!(
                model::selection().get_untracked(),
                vec![id],
                "{from:?}→{to:?}"
            );
        }
    }

    #[test]
    fn a_band_takes_everything_it_touches() {
        let _doc = install_test_doc();
        unzoomed();
        let covered = rect_at(10.0, 10.0, 30.0, 30.0);
        // Straddles the band's right edge: grazed is taken, same as covered.
        let grazed = rect_at(80.0, 10.0, 60.0, 30.0);
        // Beyond its reach entirely.
        rect_at(300.0, 300.0, 30.0, 30.0);

        sweep_gesture((0.0, 0.0), (100.0, 100.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![covered, grazed]);

        // Edges count as touching: a band that stops exactly ON a shape's edge takes it,
        // rather than missing by a pixel the user cannot see.
        model::selection().set(Vec::new());
        sweep_gesture((0.0, 0.0), (10.0, 10.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![covered]);

        // One pixel short of that edge takes nothing.
        model::selection().set(Vec::new());
        sweep_gesture((0.0, 0.0), (9.0, 9.0), plain());
        assert!(model::selection().get_untracked().is_empty());
    }

    #[test]
    fn a_thin_band_takes_what_it_sweeps_across() {
        // The everyday use of a touching band: a quick stroke through a row of shapes, with
        // no room to draw a rectangle around any of them.
        let _doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 20.0, 200.0);
        let b = rect_at(50.0, 10.0, 20.0, 200.0);
        let c = rect_at(90.0, 10.0, 20.0, 200.0);
        rect_at(300.0, 10.0, 20.0, 200.0);

        // A horizontal line across all three: zero height, and it still catches them.
        sweep_gesture((5.0, 100.0), (150.0, 100.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![a, b, c]);
    }

    #[test]
    fn touching_one_member_takes_the_whole_group() {
        let _doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 30.0, 30.0);
        let b = rect_at(200.0, 10.0, 30.0, 30.0);
        model::selection().set(vec![a, b]);
        model::group_selection();
        day::reactive::flush_sync();
        let group = model::selection().get_untracked()[0];

        // Reaching one member selects the GROUP, not that member — the rule a click on a
        // member already follows.
        model::selection().set(Vec::new());
        sweep_gesture((0.0, 0.0), (20.0, 20.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![group]);

        // The gap BETWEEN the two members belongs to neither, so a band inside it takes
        // nothing: a group answers for its shapes, not for the box around them.
        model::selection().set(Vec::new());
        sweep_gesture((60.0, 15.0), (190.0, 35.0), plain());
        assert!(model::selection().get_untracked().is_empty());

        // Across both: still one selection, listed once.
        sweep_gesture((0.0, 0.0), (300.0, 300.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![group]);
    }

    #[test]
    fn a_turned_shape_answers_for_where_it_is_drawn() {
        let _doc = install_test_doc();
        unzoomed();
        // 96×64 at the origin, turned a quarter about its center (48, 32): it now covers
        // (16, -16)–(80, 80) — taller than the frame the inspector shows, and narrower.
        let id = turned(90.0);
        model::selection().set(Vec::new());

        // Inside the stored frame but in a corner the turn vacated: not touched. (Every band
        // here starts on blank canvas — a press ON the shape would be a move, which selects
        // too and would pass these assertions for the wrong reason.)
        sweep_gesture((88.0, 4.0), (94.0, 10.0), plain());
        assert!(
            model::selection().get_untracked().is_empty(),
            "that corner is empty once the shape turns"
        );

        // Below the stored frame but inside the turned shape: touched. Swept upward from
        // blank canvas under it.
        sweep_gesture((40.0, 95.0), (60.0, 75.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![id]);
    }

    #[test]
    fn a_line_answers_for_its_stroke_not_the_box_around_it() {
        // A diagonal line's frame is mostly empty, and a band that only entered that box has
        // not touched the line.
        let _doc = install_test_doc();
        unzoomed();
        let id = model::place_shape(NodeKind::Line, 0.0, 0.0);
        day::reactive::flush_sync();
        let e = model::nodes().elem(id);
        e.w().write(200.0);
        e.h().write(200.0);
        day::reactive::flush_sync();
        model::selection().set(Vec::new());

        // Well off the diagonal, inside the frame it spans.
        sweep_gesture((150.0, 20.0), (190.0, 60.0), plain());
        assert!(
            model::selection().get_untracked().is_empty(),
            "the corner of a diagonal's box holds no ink"
        );

        // Straddling the segment: swept from one side of it to the other, starting clear of
        // the stroke's own grab tolerance.
        sweep_gesture((130.0, 70.0), (70.0, 130.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![id]);
    }

    #[test]
    fn an_oval_answers_for_its_curve_not_its_corners() {
        let _doc = install_test_doc();
        unzoomed();
        let id = model::place_shape(NodeKind::Oval, 0.0, 0.0);
        day::reactive::flush_sync();
        let e = model::nodes().elem(id);
        e.w().write(100.0);
        e.h().write(100.0);
        day::reactive::flush_sync();
        model::selection().set(Vec::new());

        // The frame's top-left corner is outside a circle inscribed in it.
        sweep_gesture((1.0, 1.0), (10.0, 10.0), plain());
        assert!(
            model::selection().get_untracked().is_empty(),
            "the corner of an oval's frame is not the oval"
        );

        // The same little band slid onto the curve does touch it.
        sweep_gesture((10.0, 10.0), (20.0, 20.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![id]);
    }

    #[test]
    fn a_shift_sweep_adds_to_the_selection_and_a_plain_one_replaces() {
        let _doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 30.0, 30.0);
        let b = rect_at(200.0, 10.0, 30.0, 30.0);

        sweep_gesture((0.0, 0.0), (60.0, 60.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![a]);

        // Shift keeps what was there and adds what the new band caught.
        sweep_gesture((180.0, 0.0), (260.0, 60.0), shift());
        assert_eq!(model::selection().get_untracked(), vec![a, b]);

        // Without it, the second band is the whole answer.
        sweep_gesture((180.0, 0.0), (260.0, 60.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![b]);

        // A shift-sweep that re-catches an already-selected shape does not list it twice.
        sweep_gesture((0.0, 0.0), (260.0, 60.0), shift());
        assert_eq!(model::selection().get_untracked(), vec![b, a]);
    }

    #[test]
    fn the_band_is_live_while_the_pointer_moves() {
        let _doc = install_test_doc();
        unzoomed();
        let id = rect_at(10.0, 10.0, 30.0, 30.0);
        let op = Rc::new(RefCell::new(DragOp::Idle));
        let at = |phase, x: f64, y: f64| Drag {
            phase,
            location: Point::new(x, y),
            translation: Point::new(x - 5.0, y - 5.0),
        };

        on_drag(at(DragPhase::Began, 5.0, 5.0), plain(), &op);
        // Short of the shape: a band, but nothing caught yet.
        on_drag(at(DragPhase::Changed, 8.0, 8.0), plain(), &op);
        assert_eq!(band().get_untracked(), Some((5.0, 5.0, 3.0, 3.0)));
        assert!(model::selection().get_untracked().is_empty());

        // Reaching it: selected mid-gesture, before any release.
        on_drag(at(DragPhase::Changed, 20.0, 20.0), plain(), &op);
        assert_eq!(band().get_untracked(), Some((5.0, 5.0, 15.0, 15.0)));
        assert_eq!(model::selection().get_untracked(), vec![id]);

        // Pulled back off it again: dropped just as live.
        on_drag(at(DragPhase::Changed, 8.0, 8.0), plain(), &op);
        assert!(model::selection().get_untracked().is_empty());

        on_drag(at(DragPhase::Ended, 60.0, 60.0), plain(), &op);
        assert_eq!(band().get_untracked(), None);
        assert_eq!(
            model::selection().get_untracked(),
            vec![id],
            "the release re-reads the final band, so the set the pointer lifts on is the set \
             it was showing"
        );
    }

    #[test]
    fn a_band_reads_the_view_transform() {
        let _doc = install_test_doc();
        unzoomed();
        let id = rect_at(100.0, 100.0, 40.0, 40.0);
        // Zoomed 2× with no pan, the shape is drawn at (200, 200)–(280, 280) on screen. A
        // band in MODEL coordinates would miss it entirely.
        zoom().set(2.0);
        sweep_gesture((190.0, 190.0), (290.0, 290.0), plain());
        assert_eq!(model::selection().get_untracked(), vec![id]);

        // The same numbers read as MODEL coordinates would land on the shape; as screen
        // coordinates at 2x they fall short of it, and nothing is caught.
        model::selection().set(Vec::new());
        sweep_gesture((90.0, 90.0), (150.0, 150.0), plain());
        assert!(model::selection().get_untracked().is_empty());
        unzoomed();
    }

    #[test]
    fn a_press_that_never_moves_is_a_click_not_a_band() {
        let _doc = install_test_doc();
        unzoomed();
        let id = rect_at(10.0, 10.0, 30.0, 30.0);
        model::selection().set(vec![id]);

        // Blank canvas, a pixel of tremor: still a click, and a click on nothing deselects.
        sweep_gesture((300.0, 300.0), (301.0, 300.0), plain());
        assert!(model::selection().get_untracked().is_empty());
        assert_eq!(band().get_untracked(), None);
    }

    #[test]
    fn a_press_on_a_shape_moves_it_and_draws_no_band() {
        let _doc = install_test_doc();
        unzoomed();
        let id = rect_at(10.0, 10.0, 30.0, 30.0);
        let other = rect_at(200.0, 200.0, 30.0, 30.0);
        model::selection().set(Vec::new());

        // A drag from inside the shape is a MOVE — the band must not steal it, and the shape
        // must not select the far one it sweeps past on the way.
        sweep_gesture((20.0, 20.0), (240.0, 240.0), plain());
        assert_eq!(band().get_untracked(), None);
        assert_eq!(model::selection().get_untracked(), vec![id]);
        assert_eq!(model::nodes().elem(id).x().peek(), 230.0);
        assert_eq!(model::nodes().elem(other).x().peek(), 200.0, "untouched");
    }

    #[test]
    fn sweeping_is_not_an_edit() {
        // A band points at shapes; it does not change them. Nothing it does may reach the
        // file, or every drag across the canvas would be an autosave and an undo step.
        let doc = install_test_doc();
        unzoomed();
        let container = doc
            .container
            .clone()
            .expect("the test doc owns a container");
        let a = rect_at(10.0, 10.0, 30.0, 30.0);
        rect_at(200.0, 200.0, 30.0, 30.0);
        container.save().expect("settle the seed");
        model::selection().set(Vec::new());
        let depth = doc.stack.can_undo().get_untracked();

        let sql = container
            .record_sql(|| {
                sweep_gesture((0.0, 0.0), (300.0, 300.0), plain());
                sweep_gesture((70.0, 70.0), (35.0, 35.0), plain());
            })
            .expect("flush");
        assert!(sql.is_empty(), "a sweep writes no rows: {sql:?}");
        // …and it did run: the second band narrowed the selection to the near shape.
        assert_eq!(model::selection().get_untracked(), vec![a]);
        assert_eq!(doc.stack.can_undo().get_untracked(), depth);
    }

    #[test]
    fn a_press_on_a_handle_resizes_rather_than_sweeping() {
        // Handles win over the band, and have to: a selected shape's corner handle sits in
        // otherwise blank canvas, and a band that claimed it would make resizing impossible.
        let _doc = install_test_doc();
        unzoomed();
        let id = rect_at(100.0, 100.0, 40.0, 40.0);
        model::selection().set(vec![id]);

        // Just outside the shape but within grabbing distance of its bottom-right corner.
        sweep_gesture((146.0, 146.0), (200.0, 200.0), plain());
        assert_eq!(band().get_untracked(), None, "no band was swept");
        assert_eq!(
            model::nodes().elem(id).w().peek(),
            94.0,
            "the corner moved with it"
        );
        assert_eq!(model::selection().get_untracked(), vec![id]);
    }

    #[test]
    fn a_band_anchors_at_the_press_even_when_began_arrives_late() {
        // appkit reports Began on the FIRST drag event, at that point, with the translation
        // from the press already applied — so the press is `location - translation`, and a
        // band that anchored on `location` would sit a whole opening move to one side.
        let _doc = install_test_doc();
        unzoomed();
        let id = rect_at(20.0, 20.0, 40.0, 40.0);
        model::selection().set(Vec::new());

        let op = Rc::new(RefCell::new(DragOp::Idle));
        // Pressed at (5, 5); the first event Day sees is already out at (105, 105).
        let at = |phase, x: f64, y: f64| Drag {
            phase,
            location: Point::new(x, y),
            translation: Point::new(x - 5.0, y - 5.0),
        };
        on_drag(at(DragPhase::Began, 105.0, 105.0), plain(), &op);
        on_drag(at(DragPhase::Ended, 105.0, 105.0), plain(), &op);
        assert_eq!(
            model::selection().get_untracked(),
            vec![id],
            "the band must cover (5,5)-(105,105), which holds the shape"
        );
    }

    // -----------------------------------------------------------------------
    // The canvas keyboard
    // -----------------------------------------------------------------------

    fn key(name: &str) -> day::KeyEvent {
        day::KeyEvent {
            key: name.into(),
            modifiers: 0,
        }
    }

    #[test]
    fn delete_removes_the_selection_where_no_menu_bar_carries_it() {
        let _doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 30.0, 30.0);
        let b = rect_at(80.0, 10.0, 30.0, 30.0);
        model::selection().set(vec![a]);

        canvas_key(&key("Delete"), true);
        day::reactive::flush_sync();
        assert_eq!(
            model::children_of(None),
            vec![b],
            "only the selected shape went"
        );
        assert!(model::selection().get_untracked().is_empty());

        // ⌫ is the same command — a laptop keyboard has no Del.
        model::selection().set(vec![b]);
        canvas_key(&key("Backspace"), true);
        day::reactive::flush_sync();
        assert!(model::children_of(None).is_empty());
    }

    #[test]
    fn delete_is_left_to_the_menu_where_one_exists() {
        // On a platform with a menu bar, Edit ▸ Delete carries this accelerator and fires
        // first; acting here too would run the command twice.
        let _doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 30.0, 30.0);
        model::selection().set(vec![a]);

        canvas_key(&key("Delete"), false);
        canvas_key(&key("Backspace"), false);
        day::reactive::flush_sync();
        assert_eq!(
            model::children_of(None),
            vec![a],
            "the shape is still there"
        );
        assert_eq!(model::selection().get_untracked(), vec![a]);
    }

    #[test]
    fn delete_with_nothing_selected_changes_nothing() {
        let doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 30.0, 30.0);
        model::selection().set(Vec::new());
        let depth = doc.stack.can_undo().get_untracked();

        canvas_key(&key("Delete"), true);
        day::reactive::flush_sync();
        assert_eq!(model::children_of(None), vec![a]);
        assert_eq!(
            doc.stack.can_undo().get_untracked(),
            depth,
            "and no empty undo step"
        );
    }

    #[test]
    fn the_arrows_still_nudge() {
        // Delete joined this handler; the keys that were already there keep working.
        let _doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 30.0, 30.0);
        model::selection().set(vec![a]);

        canvas_key(&key("ArrowRight"), true);
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).x().peek(), 11.0);
        assert_eq!(model::children_of(None), vec![a], "nudging deletes nothing");
    }

    #[test]
    fn a_deleted_selection_comes_back_with_undo() {
        let doc = install_test_doc();
        unzoomed();
        let a = rect_at(10.0, 10.0, 30.0, 30.0);
        let b = rect_at(80.0, 10.0, 30.0, 30.0);
        model::selection().set(vec![a, b]);

        canvas_key(&key("Delete"), true);
        day::reactive::flush_sync();
        assert!(model::children_of(None).is_empty());

        assert!(doc.stack.undo(), "one key, one undo step");
        day::reactive::flush_sync();
        assert_eq!(model::children_of(None), vec![a, b]);
    }

    // -----------------------------------------------------------------------
    // Only the columns a gesture actually moved
    // -----------------------------------------------------------------------

    /// Run a whole drag — press, several moves, release — and return the SQL one flush issues.
    fn drag_sql(doc: &Rc<crate::model::Doc>, from: (f64, f64), to: (f64, f64)) -> Vec<String> {
        let container = doc
            .container
            .clone()
            .expect("the test doc owns a container");
        container.save().expect("settle before measuring");
        container
            .record_sql(|| {
                let op = Rc::new(RefCell::new(DragOp::Idle));
                let at = |phase, (x, y): (f64, f64)| Drag {
                    phase,
                    location: Point::new(x, y),
                    translation: Point::new(x - from.0, y - from.1),
                };
                on_drag(at(DragPhase::Began, from), plain(), &op);
                for i in 1..=8 {
                    let f = i as f64 / 8.0;
                    let p = (from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f);
                    on_drag(at(DragPhase::Changed, p), plain(), &op);
                }
                on_drag(at(DragPhase::Ended, to), plain(), &op);
            })
            .expect("flush")
    }

    #[test]
    fn a_sideways_drag_updates_only_x() {
        let doc = install_test_doc();
        unzoomed();
        let id = rect_at(100.0, 100.0, 40.0, 40.0);
        model::selection().set(vec![id]);

        // Straight across: y ends exactly where it started, so it has nothing to say.
        let sql = drag_sql(&doc, (120.0, 120.0), (220.0, 120.0));
        assert_eq!(sql, ["UPDATE nodes SET x = ? WHERE id = ?"], "{sql:?}");
        assert_eq!(model::nodes().elem(id).x().peek(), 200.0);
        assert_eq!(model::nodes().elem(id).y().peek(), 100.0);

        // And straight down is the mirror of it.
        let sql = drag_sql(&doc, (220.0, 120.0), (220.0, 200.0));
        assert_eq!(sql, ["UPDATE nodes SET y = ? WHERE id = ?"], "{sql:?}");

        // A diagonal drag still needs both.
        let sql = drag_sql(&doc, (220.0, 200.0), (260.0, 240.0));
        assert_eq!(
            sql,
            ["UPDATE nodes SET x = ?, y = ? WHERE id = ?"],
            "{sql:?}"
        );
    }

    #[test]
    fn a_drag_that_ends_where_it_began_is_not_a_change() {
        let doc = install_test_doc();
        unzoomed();
        let id = rect_at(100.0, 100.0, 40.0, 40.0);
        model::selection().set(vec![id]);
        let depth = doc.stack.can_undo().get_untracked();

        // Out and back to the same pixel: the shape moved on screen the whole way, and the
        // document has nothing to record for it.
        let container = doc.container.clone().expect("container");
        container.save().expect("settle");
        let sql = container
            .record_sql(|| {
                let op = Rc::new(RefCell::new(DragOp::Idle));
                let at = |phase, x: f64, y: f64| Drag {
                    phase,
                    location: Point::new(x, y),
                    translation: Point::new(x - 120.0, y - 120.0),
                };
                on_drag(at(DragPhase::Began, 120.0, 120.0), plain(), &op);
                on_drag(at(DragPhase::Changed, 200.0, 180.0), plain(), &op);
                on_drag(at(DragPhase::Changed, 160.0, 140.0), plain(), &op);
                on_drag(at(DragPhase::Ended, 120.0, 120.0), plain(), &op);
            })
            .expect("flush");
        assert!(sql.is_empty(), "{sql:?}");
        assert_eq!(model::nodes().elem(id).x().peek(), 100.0);
        assert_eq!(model::nodes().elem(id).y().peek(), 100.0);
        assert_eq!(
            doc.stack.can_undo().get_untracked(),
            depth,
            "and no undo step that would do nothing"
        );
    }

    #[test]
    fn the_next_drag_undoes_to_its_own_starting_point() {
        // A field that sat out one gesture still records the next one correctly: the sideways
        // drag leaves y alone, and the drag after it must still undo y to where THAT drag
        // found it.
        let doc = install_test_doc();
        unzoomed();
        let id = rect_at(100.0, 100.0, 40.0, 40.0);
        model::selection().set(vec![id]);

        drag_sql(&doc, (120.0, 120.0), (220.0, 120.0)); // sideways: y never moves
        assert_eq!(model::nodes().elem(id).y().peek(), 100.0);
        drag_sql(&doc, (220.0, 120.0), (220.0, 190.0)); // now down: y moves for the first time
        assert_eq!(model::nodes().elem(id).y().peek(), 170.0);

        assert!(doc.stack.undo(), "undo the downward drag");
        day::reactive::flush_sync();
        assert_eq!(
            model::nodes().elem(id).y().peek(),
            100.0,
            "back to where THAT drag started"
        );
        assert_eq!(
            model::nodes().elem(id).x().peek(),
            200.0,
            "and the earlier sideways drag still stands"
        );
    }

    #[test]
    fn a_corner_resize_updates_only_the_edges_it_moved() {
        let doc = install_test_doc();
        unzoomed();
        let id = rect_at(100.0, 100.0, 40.0, 40.0);
        model::selection().set(vec![id]);

        // The bottom-right handle: the origin stays put, so x and y stay out of it.
        let (hx, hy) = (140.0, 140.0);
        let sql = drag_sql(&doc, (hx, hy), (hx + 30.0, hy + 20.0));
        assert_eq!(
            sql,
            ["UPDATE nodes SET w = ?, h = ? WHERE id = ?"],
            "{sql:?}"
        );
        let e = model::nodes().elem(id);
        assert_eq!((e.x().peek(), e.y().peek()), (100.0, 100.0));
        assert_eq!((e.w().peek(), e.h().peek()), (70.0, 60.0));

        // The top-left handle moves all four.
        let sql = drag_sql(&doc, (100.0, 100.0), (90.0, 90.0));
        assert_eq!(
            sql,
            ["UPDATE nodes SET x = ?, y = ?, w = ?, h = ? WHERE id = ?"],
            "{sql:?}"
        );
    }

    #[test]
    fn an_arrow_nudge_updates_only_the_axis_it_moves() {
        let doc = install_test_doc();
        unzoomed();
        let id = rect_at(100.0, 100.0, 40.0, 40.0);
        model::selection().set(vec![id]);
        let container = doc.container.clone().expect("container");
        container.save().expect("settle");

        let sql = container
            .record_sql(|| canvas_key(&key("ArrowRight"), true))
            .expect("flush");
        assert_eq!(sql, ["UPDATE nodes SET x = ? WHERE id = ?"], "{sql:?}");
        assert_eq!(model::nodes().elem(id).x().peek(), 101.0);

        let sql = container
            .record_sql(|| canvas_key(&key("ArrowDown"), true))
            .expect("flush");
        assert_eq!(sql, ["UPDATE nodes SET y = ? WHERE id = ?"], "{sql:?}");
        assert_eq!(model::nodes().elem(id).y().peek(), 101.0);
    }

    // -----------------------------------------------------------------------
    // Rotating a group turns the arrangement, not each piece
    // -----------------------------------------------------------------------

    /// Two 40x40 squares side by side with a 120-wide gap: (0,0) and (160,0), so the group's
    /// frame is 200x40 and its centre is (100, 20).
    fn pair_group() -> (u64, u64, u64) {
        let a = rect_at(0.0, 0.0, 40.0, 40.0);
        let b = rect_at(160.0, 0.0, 40.0, 40.0);
        model::selection().set(vec![a, b]);
        model::group_selection();
        day::reactive::flush_sync();
        let g = model::selection().get_untracked()[0];
        (g, a, b)
    }

    fn centre(id: u64) -> (f64, f64) {
        let e = model::nodes().elem(id);
        (
            e.x().peek() + e.w().peek() / 2.0,
            e.y().peek() + e.h().peek() / 2.0,
        )
    }

    fn near(got: (f64, f64), want: (f64, f64)) {
        assert!(
            (got.0 - want.0).abs() < 0.001 && (got.1 - want.1).abs() < 0.001,
            "got {got:?}, want {want:?}"
        );
    }

    #[test]
    fn a_group_turns_as_one_body() {
        let _doc = install_test_doc();
        unzoomed();
        let (g, a, b) = pair_group();
        assert_eq!(model::node_bounds(g), Some((0.0, 0.0, 200.0, 40.0)));

        // A quarter turn about the group's centre (100, 20): the members ORBIT it. `a`'s
        // centre (20, 20) swings to (100, -60); `b`'s (180, 20) to (100, 100).
        model::set_rotation(g, 90.0, true);
        day::reactive::flush_sync();
        near(centre(a), (100.0, -60.0));
        near(centre(b), (100.0, 100.0));

        // …and each member turns by the same amount, so the arrangement is rigid rather than
        // two squares sliding around each other.
        assert_eq!(model::nodes().elem(a).rotation().peek(), 90.0);
        assert_eq!(model::nodes().elem(b).rotation().peek(), 90.0);
        assert_eq!(model::nodes().elem(g).rotation().peek(), 90.0);

        // The outline follows the turned body: the bounds are ALWAYS the box around what is
        // actually on the canvas — a 200-wide pair turned upright reads 40 wide, 200 tall.
        assert_eq!(model::node_bounds(g), Some((80.0, -80.0, 40.0, 200.0)));
    }

    #[test]
    fn turning_a_group_twice_pivots_on_the_same_point() {
        // The failure this guards: pivoting on the union of the members would walk the centre
        // a little further with every turn, and a full circle would not come home.
        let _doc = install_test_doc();
        unzoomed();
        let (g, a, b) = pair_group();
        let (a0, b0) = (centre(a), centre(b));

        for step in 1..=4 {
            model::set_rotation(g, (step * 90) as f64 % 360.0, true);
            day::reactive::flush_sync();
        }
        near(centre(a), a0);
        near(centre(b), b0);
        assert_eq!(model::nodes().elem(a).rotation().peek(), 0.0);
    }

    #[test]
    fn a_turned_group_moves_and_keeps_its_frame_under_it() {
        let _doc = install_test_doc();
        unzoomed();
        let (g, a, _b) = pair_group();
        model::set_rotation(g, 90.0, true);
        day::reactive::flush_sync();
        let before = centre(a);

        // Drag the group by (50, 30) — from a point that is on a member, wherever it has
        // swung to.
        let (mx, my) = centre(a);
        model::selection().set(vec![g]);
        sweep_gesture((mx, my), (mx + 50.0, my + 30.0), plain());
        day::reactive::flush_sync();
        near(centre(a), (before.0 + 50.0, before.1 + 30.0));
        // The derived bounds travelled with the members: the turned pair stands upright
        // (40 × 200), now 50 right and 30 down of where the turn left it.
        assert_eq!(
            model::node_bounds(g),
            Some((130.0, -50.0, 40.0, 200.0)),
            "the outline travelled with the members"
        );
        // And the next turn pivots on the members' collective centre, which moved with them
        // to (150, 50). The angle is ABSOLUTE, so going to 180 from 90 turns by 90: `a`'s
        // centre (150, -30) swings a quarter about (150, 50) and lands at (230, 50).
        model::set_rotation(g, 180.0, true);
        day::reactive::flush_sync();
        near(centre(a), (230.0, 50.0));
    }

    #[test]
    fn group_bounds_always_rederive_from_the_members() {
        // The reported bug: move a member OUT of the grouped arrangement and re-select the
        // group — the outline used to be the group's stored frame, frozen at grouping time.
        let _doc = install_test_doc();
        unzoomed();
        let (g, a, _b) = pair_group();
        assert_eq!(model::node_bounds(g), Some((0.0, 0.0, 200.0, 40.0)));

        // Deep-select `a` (the double-click drill) and drag it far below the old box.
        model::selection().set(vec![a]);
        sweep_gesture((20.0, 20.0), (20.0, 320.0), plain());
        day::reactive::flush_sync();
        near(centre(a), (20.0, 320.0));

        // The group's outline now encompasses where the members actually are.
        model::selection().set(vec![g]);
        assert_eq!(model::node_bounds(g), Some((0.0, 0.0, 200.0, 340.0)));

        // A member's TURN widens the box too: the outline covers what the member visually
        // occupies, not just its unrotated frame. `b` is 40×40 at (160, 0); at 45° its
        // corners reach √2·20 ≈ 28.28 from its centre (180, 20).
        model::set_rotation(_b, 45.0, true);
        day::reactive::flush_sync();
        let (bx, by, bw, bh) = model::node_bounds(g).expect("bounds");
        let r = 20.0 * std::f64::consts::SQRT_2;
        assert!((bx - 0.0).abs() < 1e-9, "left still a's edge: {bx}");
        assert!(
            ((bx + bw) - (180.0 + r)).abs() < 1e-9,
            "the right edge follows the turned corner: {bw}"
        );
        assert!(
            (by - (20.0 - r)).abs() < 1e-9,
            "the top follows the turned corner: {by}"
        );
        assert!(
            (by + bh - 340.0).abs() < 1e-9,
            "bottom still a's edge: {bh}"
        );

        // And an EMPTY group has no bounds at all rather than a stale box.
        model::reparent(a, None, None);
        model::reparent(_b, None, None);
        day::reactive::flush_sync();
        assert_eq!(model::node_bounds(g), None);
    }

    #[test]
    fn a_lone_shape_still_turns_about_its_own_centre() {
        let _doc = install_test_doc();
        unzoomed();
        let id = rect_at(100.0, 100.0, 40.0, 20.0);
        model::set_rotation(id, 90.0, true);
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(id).rotation().peek(), 90.0);
        // Its stored frame is untouched — a shape turns where it stands.
        assert_eq!(model::node_bounds(id), Some((100.0, 100.0, 40.0, 20.0)));
    }

    #[test]
    fn a_group_of_lines_turns_by_moving_their_ends() {
        // A line carries no angle of its own, so a group containing one turns it by moving
        // both ends — the only thing that means anything for a line.
        let _doc = install_test_doc();
        unzoomed();
        let l = model::place_shape(NodeKind::Line, 0.0, 0.0);
        day::reactive::flush_sync();
        let e = model::nodes().elem(l);
        e.w().write(100.0);
        e.h().write(0.0);
        day::reactive::flush_sync();
        let r = rect_at(0.0, 0.0, 40.0, 40.0);
        model::selection().set(vec![l, r]);
        model::group_selection();
        day::reactive::flush_sync();
        let g = model::selection().get_untracked()[0];

        let ((ax, ay), (bx, by)) = model::line_ends(l);
        model::set_rotation(g, 90.0, true);
        day::reactive::flush_sync();
        let ((ax2, ay2), (bx2, by2)) = model::line_ends(l);
        assert_ne!((ax, ay), (ax2, ay2));
        // The segment keeps its length, and the line itself never took an angle.
        let len = |p: (f64, f64), q: (f64, f64)| (q.0 - p.0).hypot(q.1 - p.1);
        assert!((len((ax, ay), (bx, by)) - len((ax2, ay2), (bx2, by2))).abs() < 0.001);
        assert_eq!(model::nodes().elem(l).rotation().peek(), 0.0);
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
        let _id = turned(90.0);
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
