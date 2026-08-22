//! The inspector panel: property rows over the current selection.
//!
//! Each row is a small selection-fanout [`Binding<String>`]: it reads the COMMON value of the
//! selected nodes (empty text plus a "multi" placeholder when they disagree), and a typed
//! value fans out to every selected node in ONE undo turn. Reads are tracked through the
//! store, so a canvas drag's preview writes re-run them live — the fields follow the pointer
//! the same way the status row does. The panel is built to grow: a property is one
//! [`NumProp`] row, and a future section (fill, stroke, …) is one more entry in [`panel`].

use crate::model::{self, NodeFields, NodeKind};
use day::prelude::*;

/// Whether the inspector pane is showing. Deliberately per-run and default-hidden: the
/// walkthrough opens it explicitly, so every target starts from the same state.
pub(crate) fn visible() -> Signal<bool> {
    thread_local! {
        static VIS: Signal<bool> = Signal::global(false);
    }
    VIS.with(|s| *s)
}

pub(crate) fn toggle() {
    visible().update(|v| *v = !*v);
}

/// Re-runs the field bindings without a model change — bumped after a rejected or clamped
/// edit, so the canonical text paints back over whatever was typed. A global `Signal` rather
/// than a `Trigger`, which has no scope-free constructor.
fn refresh() -> Signal<u64> {
    thread_local! {
        static R: Signal<u64> = Signal::global(0);
    }
    R.with(|s| *s)
}

fn refresh_fields() {
    refresh().update(|v| *v = v.wrapping_add(1));
}

/// One numeric property of the selection — a row in the geometry section.
struct NumProp {
    /// The field's element id (`insp-x`) — the dayscript target.
    id: &'static str,
    label: fn() -> day::LocalizedText,
    /// The property's value on ONE node; `None` for a node it doesn't apply to.
    get: fn(u64) -> Option<f64>,
    /// Apply a typed value to one node. The caller wraps the whole selection in one undo
    /// group, labeled `undo` (a key of the same catalog the canvas gestures use).
    set: fn(u64, f64),
    undo: &'static str,
}

/// Move a node by (dx, dy): its own frame for a shape, every shape descendant for a group —
/// the same rule the canvas drag applies.
fn move_by(id: u64, dx: f64, dy: f64) {
    let store = model::nodes();
    for s in model::shape_descendants(id) {
        let e = store.elem(s);
        e.x().write_commit(e.x().peek() + dx);
        e.y().write_commit(e.y().peek() + dy);
    }
}

fn set_x(id: u64, v: f64) {
    if let Some((x, ..)) = model::node_bounds(id) {
        move_by(id, v - x, 0.0);
    }
}

fn set_y(id: u64, v: f64) {
    if let Some((_, y, ..)) = model::node_bounds(id) {
        move_by(id, 0.0, v - y);
    }
}

/// Width/height apply to shapes only — groups keep their derived union, the canvas's own
/// resize rule (groups move; only shapes resize).
fn set_w(id: u64, v: f64) {
    let e = model::nodes().elem(id);
    if e.kind().peek() != NodeKind::Group {
        e.w().write_commit(v.max(model::MIN_SIZE));
    }
}

fn set_h(id: u64, v: f64) {
    let e = model::nodes().elem(id);
    if e.kind().peek() != NodeKind::Group {
        e.h().write_commit(v.max(model::MIN_SIZE));
    }
}

fn geometry_props() -> [NumProp; 4] {
    [
        NumProp {
            id: "insp-x",
            label: crate::res::str::insp_x,
            get: |n| model::node_bounds(n).map(|b| b.0),
            set: set_x,
            undo: "move",
        },
        NumProp {
            id: "insp-y",
            label: crate::res::str::insp_y,
            get: |n| model::node_bounds(n).map(|b| b.1),
            set: set_y,
            undo: "move",
        },
        NumProp {
            id: "insp-w",
            label: crate::res::str::insp_w,
            get: |n| model::node_bounds(n).map(|b| b.2),
            set: set_w,
            undo: "resize",
        },
        NumProp {
            id: "insp-h",
            label: crate::res::str::insp_h,
            get: |n| model::node_bounds(n).map(|b| b.3),
            set: set_h,
            undo: "resize",
        },
    ]
}

/// The display form of a value: one decimal at most, integers bare — stable strings the
/// walkthrough can assert against.
fn fmt(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r:.1}")
    }
}

/// The selection's common value for property `ix`: `Some` when every selected node agrees
/// (to a tenth), `None` when the selection is empty or disagrees.
fn common(ix: usize, tracked: bool) -> Option<f64> {
    let sel = if tracked {
        // Track the selection AND the store: a canvas drag writes previews the fields must
        // follow live (the status row's pattern).
        model::nodes().with(|_| {});
        model::selection().get()
    } else {
        model::selection().get_untracked()
    };
    refresh().track();
    let get = geometry_props()[ix].get;
    let mut vals = sel.iter().filter_map(|n| get(*n));
    let first = vals.next()?;
    for v in vals {
        if (v - first).abs() >= 0.05 {
            return None;
        }
    }
    Some(first)
}

/// Selected nodes disagree on property `ix` — the "multi" placeholder's condition.
fn is_mixed(ix: usize) -> bool {
    !model::selection().get().is_empty() && common(ix, true).is_none()
}

/// The two-way seam between one property row's text field and the whole selection.
#[derive(Clone, Copy)]
struct PropField {
    ix: usize,
}

impl Binding<String> for PropField {
    fn read(&self) -> String {
        common(self.ix, true).map(fmt).unwrap_or_default()
    }
    fn peek(&self) -> String {
        common(self.ix, false).map(fmt).unwrap_or_default()
    }
    fn write(&self, s: String) {
        self.write_commit(s);
    }
    /// Keystrokes are previews the model must NOT follow — "1" on the way to "125" is not a
    /// position. Only the committed text (Return, focus loss) reaches the nodes.
    fn write_preview(&self, _s: String) {}
    fn write_commit(&self, s: String) {
        let sel = model::selection().get_untracked();
        let parsed = s.trim().parse::<f64>().ok().filter(|v| v.is_finite());
        let Some(v) = parsed else {
            // Rejected: leave the model alone and repaint the canonical text over the typo.
            refresh_fields();
            return;
        };
        if sel.is_empty() {
            refresh_fields();
            return;
        }
        let p = &geometry_props()[self.ix];
        model::undo_stack().grouped(p.undo, || {
            for n in &sel {
                (p.set)(*n, v);
            }
        });
        // The applied value may clamp (MIN_SIZE) or not apply (a group's width): repaint
        // the fields from what the model actually holds.
        refresh_fields();
    }
}

fn prop_row(ix: usize) -> AnyPiece {
    let p = &geometry_props()[ix];
    labeled(
        (p.label)(),
        text_field(PropField { ix })
            .placeholder(move || {
                if is_mixed(ix) {
                    crate::res::str::insp_multi().format()
                } else {
                    String::new()
                }
            })
            .id(p.id),
    )
}

// ---------------------------------------------------------------------------
// The Style section: fill color/opacity, stroke color/width/opacity. Style edits reach the
// SHAPES — a selected group restyles its members, the canvas's own fill rule — and every
// commit is ONE undo unit labeled "style". Slider drags flow as previews (the canvas follows
// the thumb) and commit once on release, the same session shape a canvas drag uses.
// ---------------------------------------------------------------------------

/// Every shape the style section edits: the shape descendants of each selected node.
fn style_targets(tracked: bool) -> Vec<u64> {
    let sel = if tracked {
        model::nodes().with(|_| {});
        model::selection().get()
    } else {
        model::selection().get_untracked()
    };
    sel.iter()
        .flat_map(|id| model::shape_descendants(*id))
        .collect()
}

/// `#RRGGBB` (or `#RGB`) → Color; unparseable falls back to mid-gray, like the canvas.
fn parse_hex(hex: &str) -> Color {
    let h = hex.trim().trim_start_matches('#');
    let expanded: String = if h.len() == 3 {
        h.chars().flat_map(|c| [c, c]).collect()
    } else {
        h.to_string()
    };
    u32::from_str_radix(&expanded, 16)
        .map(Color::hex)
        .unwrap_or(Color::hex(0x888888))
}

fn to_hex(c: Color) -> String {
    format!(
        "#{:02X}{:02X}{:02X}",
        (c.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.b.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// One numeric style property, fanned out over the style targets. `read` shows the FIRST
/// target's value — a numeric control has no "multi" form — and writes reach every target.
#[derive(Clone, Copy)]
struct StyleNum {
    read: fn(u64) -> f64,
    preview: fn(u64, f64),
    commit: fn(u64, f64),
}

impl Binding<f64> for StyleNum {
    fn read(&self) -> f64 {
        style_targets(true)
            .first()
            .map(|t| (self.read)(*t))
            .unwrap_or(0.0)
    }
    fn peek(&self) -> f64 {
        style_targets(false)
            .first()
            .map(|t| (self.read)(*t))
            .unwrap_or(0.0)
    }
    fn write(&self, v: f64) {
        self.write_commit(v);
    }
    fn write_preview(&self, v: f64) {
        for t in style_targets(false) {
            (self.preview)(t, v);
        }
    }
    fn write_commit(&self, v: f64) {
        let targets = style_targets(false);
        if targets.is_empty() {
            return;
        }
        model::undo_stack().grouped("style", || {
            for t in targets {
                (self.commit)(t, v);
            }
        });
    }
}

/// One color style property, fanned out the same way. The well shows the first target's
/// color; a pick commits to every target as one undo unit.
#[derive(Clone, Copy)]
struct StyleColor {
    read: fn(u64) -> String,
    commit: fn(u64, &str),
}

impl Binding<Color> for StyleColor {
    fn read(&self) -> Color {
        style_targets(true)
            .first()
            .map(|t| parse_hex(&(self.read)(*t)))
            .unwrap_or(Color::hex(0x888888))
    }
    fn peek(&self) -> Color {
        style_targets(false)
            .first()
            .map(|t| parse_hex(&(self.read)(*t)))
            .unwrap_or(Color::hex(0x888888))
    }
    fn write(&self, c: Color) {
        let targets = style_targets(false);
        if targets.is_empty() {
            return;
        }
        let hex = to_hex(c);
        model::undo_stack().grouped("style", || {
            for t in targets {
                (self.commit)(t, &hex);
            }
        });
    }
}

/// The opacity row's percentage field: the same bound value, shown as a localized percent
/// ("85%") and accepting one typed back with or without the suffix.
#[derive(Clone, Copy)]
struct PctField {
    num: StyleNum,
}

impl Binding<String> for PctField {
    fn read(&self) -> String {
        crate::res::str::insp_percent((self.num.read() * 100.0).round() as i64).format()
    }
    fn peek(&self) -> String {
        crate::res::str::insp_percent((self.num.peek() * 100.0).round() as i64).format()
    }
    fn write(&self, s: String) {
        self.write_commit(s);
    }
    fn write_preview(&self, _s: String) {}
    fn write_commit(&self, s: String) {
        // Fluent wraps interpolations in bidi isolates (U+2068/U+2069); a value the app
        // formatted and the user edited in place still carries them.
        let cleaned: String = s
            .chars()
            .filter(|c| !matches!(c, '\u{2068}' | '\u{2069}'))
            .collect();
        let cleaned = cleaned.trim().trim_end_matches('%').trim();
        if let Ok(pct) = cleaned.parse::<f64>() {
            self.num.write_commit((pct / 100.0).clamp(0.0, 1.0));
        }
        refresh_fields();
    }
}

const FILL_OPACITY: StyleNum = StyleNum {
    read: |t| model::nodes().elem(t).fill_opacity().read(),
    preview: |t, v| model::nodes().elem(t).fill_opacity().write_preview(v),
    commit: |t, v| model::nodes().elem(t).fill_opacity().write_commit(v),
};

const STROKE_OPACITY: StyleNum = StyleNum {
    read: |t| model::nodes().elem(t).stroke_opacity().read(),
    preview: |t, v| model::nodes().elem(t).stroke_opacity().write_preview(v),
    commit: |t, v| model::nodes().elem(t).stroke_opacity().write_commit(v),
};

const STROKE_WIDTH: StyleNum = StyleNum {
    read: |t| model::nodes().elem(t).stroke_width().read(),
    preview: |t, v| model::nodes().elem(t).stroke_width().write_preview(v.max(0.0)),
    commit: |t, v| model::nodes().elem(t).stroke_width().write_commit(v.max(0.0)),
};

const FILL_COLOR: StyleColor = StyleColor {
    read: |t| model::nodes().elem(t).fill().with(|f| f.cloned().unwrap_or_default()),
    commit: |t, hex| model::nodes().elem(t).fill().write_commit(hex.to_string()),
};

const STROKE_COLOR: StyleColor = StyleColor {
    read: |t| {
        model::nodes()
            .elem(t)
            .stroke()
            .with(|s| s.cloned().unwrap_or_default())
    },
    commit: |t, hex| model::nodes().elem(t).stroke().write_commit(hex.to_string()),
};

/// A slider with its attached percentage field — the opacity rows' control.
fn opacity_row(num: StyleNum, id: &'static str) -> AnyPiece {
    row((
        slider(num).range(0.0..=1.0).step(0.01).grow(),
        text_field(PctField { num }).id(id).width(64.0),
    ))
    .spacing(6.0)
    .align(VAlign::Center)
    .any()
}

fn style_section() -> AnyPiece {
    section((
        labeled(
            crate::res::str::insp_fill(),
            day_piece_colorpicker::color_picker(FILL_COLOR).key("insp-fill"),
        ),
        labeled(
            crate::res::str::insp_fill_opacity(),
            opacity_row(FILL_OPACITY, "insp-fill-op"),
        ),
        labeled(
            crate::res::str::insp_stroke(),
            day_piece_colorpicker::color_picker(STROKE_COLOR).key("insp-stroke"),
        ),
        labeled(
            crate::res::str::insp_stroke_width(),
            day_piece_stepper::stepper(STROKE_WIDTH)
                .range(0.0..=64.0)
                .step(1.0)
                .decimals(0)
                .key("insp-stroke-w"),
        ),
        labeled(
            crate::res::str::insp_stroke_opacity(),
            opacity_row(STROKE_OPACITY, "insp-stroke-op"),
        ),
    ))
    .title(crate::res::str::insp_style())
    .any()
}

/// The panel content: one form, one section per property group. New sections slot in here.
pub(crate) fn panel() -> AnyPiece {
    let rows = PieceVec((0..geometry_props().len()).map(prop_row).collect());
    form((
        section(rows).title(crate::res::str::insp_geometry()),
        style_section(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::install_test_doc;

    const X: PropField = PropField { ix: 0 };
    const Y: PropField = PropField { ix: 1 };
    const W: PropField = PropField { ix: 2 };

    #[test]
    fn fields_show_the_common_value_and_multi_when_mixed() {
        let _doc = install_test_doc();
        let a = model::place_shape(NodeKind::Rect, 10.0, 40.0);
        day::reactive::flush_sync();
        let b = model::place_shape(NodeKind::Oval, 200.0, 40.0);
        day::reactive::flush_sync();

        model::selection().set(vec![a]);
        assert_eq!(X.peek(), "10");
        assert_eq!(Y.peek(), "40");
        assert_eq!(W.peek(), "96");

        // Same y and w, different x: only x reads mixed.
        model::selection().set(vec![a, b]);
        assert_eq!(X.peek(), "", "disagreeing values read empty (multi)");
        assert_eq!(Y.peek(), "40");
        assert_eq!(W.peek(), "96");

        model::selection().set(Vec::new());
        assert_eq!(X.peek(), "", "no selection, no value");
    }

    #[test]
    fn a_typed_value_fans_out_to_every_selected_node_as_one_undo_unit() {
        let doc = install_test_doc();
        let a = model::place_shape(NodeKind::Rect, 10.0, 40.0);
        day::reactive::flush_sync();
        let b = model::place_shape(NodeKind::Oval, 200.0, 80.0);
        day::reactive::flush_sync();
        model::selection().set(vec![a, b]);

        X.write_commit("50".into());
        day::reactive::flush_sync();
        assert_eq!(model::node_bounds(a).map(|f| f.0), Some(50.0));
        assert_eq!(model::node_bounds(b).map(|f| f.0), Some(50.0));
        assert_eq!(X.peek(), "50", "the fields agree after the edit");

        assert!(doc.stack.undo(), "one step undoes the whole fan-out");
        day::reactive::flush_sync();
        assert_eq!(model::node_bounds(a).map(|f| f.0), Some(10.0));
        assert_eq!(model::node_bounds(b).map(|f| f.0), Some(200.0));
    }

    #[test]
    fn garbage_and_size_floors_leave_the_model_alone() {
        let doc = install_test_doc();
        let a = model::place_shape(NodeKind::Rect, 10.0, 40.0);
        day::reactive::flush_sync();
        model::selection().set(vec![a]);

        let undos_before = doc.stack.can_undo().get_untracked();
        X.write_commit("not a number".into());
        day::reactive::flush_sync();
        assert_eq!(model::node_bounds(a).map(|f| f.0), Some(10.0));
        assert_eq!(doc.stack.can_undo().get_untracked(), undos_before);

        // A width below the floor clamps to MIN_SIZE rather than inverting the shape.
        W.write_commit("1".into());
        day::reactive::flush_sync();
        assert_eq!(model::node_bounds(a).map(|f| f.2), Some(model::MIN_SIZE));
    }

    #[test]
    fn style_edits_fan_out_to_shape_descendants_as_undo_units() {
        let doc = install_test_doc();
        let a = model::place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = model::place_shape(NodeKind::Oval, 100.0, 0.0);
        day::reactive::flush_sync();
        model::selection().set(vec![a, b]);
        model::group_selection();
        day::reactive::flush_sync();

        // A selected GROUP restyles its member shapes.
        let (ea, eb) = (model::nodes().elem(a), model::nodes().elem(b));
        FILL_OPACITY.write_preview(0.4);
        assert_eq!(ea.fill_opacity().peek(), 0.4, "previews reach the canvas live");
        FILL_OPACITY.write_commit(0.5);
        day::reactive::flush_sync();
        assert_eq!(ea.fill_opacity().peek(), 0.5);
        assert_eq!(eb.fill_opacity().peek(), 0.5);

        STROKE_WIDTH.write_commit(4.0);
        day::reactive::flush_sync();
        Binding::<Color>::write(&FILL_COLOR, Color::hex(0x112233));
        day::reactive::flush_sync();
        assert_eq!(ea.fill().with(|f| f.cloned()).as_deref(), Some("#112233"));
        assert_eq!(eb.fill().with(|f| f.cloned()).as_deref(), Some("#112233"));

        // The well shows the first target's color back.
        assert_eq!(to_hex(Binding::<Color>::peek(&FILL_COLOR)), "#112233");

        // One undo unit per commit: color, then width, then opacity.
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(ea.fill().with(|f| f.cloned()).as_deref(), Some("#EF4444"));
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(ea.stroke_width().peek(), 1.0);
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(ea.fill_opacity().peek(), 1.0);
    }

    #[test]
    fn the_percent_field_localizes_and_parses_both_forms() {
        let _doc = install_test_doc();
        let a = model::place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        model::selection().set(vec![a]);
        crate::res::locales::install();

        // Fluent wraps interpolations in bidi isolates; strip them to see the visible text.
        fn visible(s: String) -> String {
            s.chars()
                .filter(|c| !matches!(c, '\u{2068}' | '\u{2069}'))
                .collect()
        }
        let pct = PctField { num: FILL_OPACITY };
        assert_eq!(visible(pct.peek()), "100%");
        pct.write_commit("25%".into());
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).fill_opacity().peek(), 0.25);
        pct.write_commit("80".into());
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).fill_opacity().peek(), 0.8);
        assert_eq!(visible(pct.peek()), "80%");
        // Out-of-range clamps; garbage leaves the value alone.
        pct.write_commit("250".into());
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).fill_opacity().peek(), 1.0);
        pct.write_commit("opaque".into());
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).fill_opacity().peek(), 1.0);
    }

    #[test]
    fn group_x_translates_members_and_group_w_is_left_derived() {
        let _doc = install_test_doc();
        let a = model::place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = model::place_shape(NodeKind::Rect, 100.0, 20.0);
        day::reactive::flush_sync();
        model::selection().set(vec![a, b]);
        model::group_selection();
        day::reactive::flush_sync();

        X.write_commit("40".into());
        day::reactive::flush_sync();
        let gid = model::selection().get_untracked()[0];
        assert_eq!(
            model::node_bounds(gid),
            Some((40.0, 0.0, 196.0, 84.0)),
            "the union moved; members kept their offsets"
        );
        assert_eq!(model::node_bounds(a).map(|f| f.0), Some(40.0));
        assert_eq!(model::node_bounds(b).map(|f| f.0), Some(140.0));

        W.write_commit("500".into());
        day::reactive::flush_sync();
        assert_eq!(
            model::node_bounds(gid).map(|f| f.2),
            Some(196.0),
            "a group's size stays derived from its shapes"
        );
    }
}
