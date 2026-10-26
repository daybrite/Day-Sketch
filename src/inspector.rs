//! The inspector panel: property rows over the current selection.
//!
//! Each row is a small selection-fanout [`Binding<String>`]: it reads the common value of the
//! selected nodes (empty text plus a "multi" placeholder when they disagree), and a typed
//! value fans out to every selected node in one undo turn. Reads are tracked through the
//! store, so a canvas drag's preview writes re-run them live; the fields follow the pointer
//! the same way the status row does. The panel is built to grow: a property is one
//! [`NumProp`] row, and a future section (fill, stroke, …) is one more entry in [`panel`].

use crate::model::{self, NodeFields, NodeKind};
use day::prelude::*;

// Inspector callbacks belong to a drawing window even when a different window has focus.
// Reactive computations do not restore their creation scope; preserve it at the UI boundary.
fn window_read<T>(read: impl Fn() -> T + 'static) -> impl Fn() -> T {
    let scope = day::reactive::Scope::current();
    move || scope.enter(&read)
}

#[derive(Clone)]
struct WindowBinding<B> {
    scope: day::reactive::Scope,
    binding: B,
}

fn window_binding<B>(binding: B) -> WindowBinding<B> {
    WindowBinding {
        scope: day::reactive::Scope::current(),
        binding,
    }
}

impl<T: 'static, B: Binding<T>> Binding<T> for WindowBinding<B> {
    fn read(&self) -> T {
        self.scope.enter(|| self.binding.read())
    }
    fn peek(&self) -> T {
        self.scope.enter(|| self.binding.peek())
    }
    fn write(&self, value: T) {
        self.scope.enter(|| self.binding.write(value));
    }
    fn write_preview(&self, value: T) {
        self.scope.enter(|| self.binding.write_preview(value));
    }
    fn write_commit(&self, value: T) {
        self.scope.enter(|| self.binding.write_commit(value));
    }
    fn write_teardown(&self, value: T) {
        self.scope.enter(|| self.binding.write_teardown(value));
    }
}

/// Whether the inspector pane is showing. Shown by default, since the panel is the app's main
/// property surface, and per-run (not persisted): every target starts from the same state,
/// which is what the walkthrough scripts assume.
pub(crate) fn visible() -> Signal<bool> {
    crate::scene().inspector_visible
}

pub(crate) fn toggle() {
    visible().update(|v| *v = !*v);
}

/// Whether the layers pane is showing: the leading tree over the document (docs/tree.md).
/// Open by default where the window has room; a compact window (a phone) starts with it
/// closed, since the pane would squeeze the canvas it exists to describe. The View-menu
/// item, the toolbar toggle and the tool row's Layers button all reopen it. Per-run, like
/// [`visible`]; the class is already seeded when the root builds (docs/size-classes.md).
pub(crate) fn layers_visible() -> Signal<bool> {
    crate::scene().layers_visible
}

pub(crate) fn layers_toggle() {
    layers_visible().update(|v| *v = !*v);
}

/// A layer row's kind glyph. The symbols are language-neutral, like the sheet's `✕`, and
/// picked from one optical family: the geometric-shape block's x-height forms plus the
/// division slash, whose metrics sit inside a text line (the box-drawing `╱` spans the whole
/// line, the large circle `◯` overshoots it, and `▭` collapses to a pair of dashes at row
/// size, which is what made the first cut look misaligned).
fn kind_glyph(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Rect => "□",
        NodeKind::Oval => "○",
        NodeKind::Line => "∕",
        NodeKind::Group => "⊞",
        NodeKind::Text => "T",
        NodeKind::Image => "▧",
    }
}

/// The layers pane, rebuilt per document exactly like the editor (`window_shell`'s `keyed`):
/// the tree's connection captures the current doc's store, and a new document is a new store,
/// so without the rebuild the panel would keep watching the old one.
pub(crate) fn layers_panel() -> AnyPiece {
    column((
        crate::transfer::selection_handle(),
        keyed(move || Some(model::doc_rev().get()), |_| layers_tree()).grow(),
    ))
    .any()
}

/// The tree itself (docs/tree.md). Selection is the same signal the canvas reads, expansion
/// is [`model::open_groups`], and a drag commits through [`model::reparent`]: one model,
/// three surfaces.
fn layers_tree() -> AnyPiece {
    let selection = model::selection();
    tree(
        model::nodes().tree(model::children_of),
        |slot: ModelSlot<model::Node>| {
            row((
                // Centered in a fixed slot, so every name starts on one column whatever
                // the glyph's advance; the row's default cross-align centers both labels
                // on the row's vertical middle.
                label(move || kind_glyph(slot.kind().read()).to_string())
                    .align(TextAlign::Center)
                    .width(18.0),
                label(move || model::layer_label(slot.kind().read(), slot.key())),
            ))
            .spacing(5.0)
            // Clear of the selection pill's rounded corner (the appkit row insets it 5pt).
            .padding(Insets::horizontal(8.0))
        },
    )
    .row_height(RowHeight::Uniform(28.0))
    .expanded(model::open_groups())
    .expandable(|id| model::node_kind(*id) == Some(NodeKind::Group))
    .multi_select()
    .selected(move || selection.get())
    .on_selection(move |keys| selection.set(keys))
    .movable(Move::new(model::reparent))
    .type_ahead(|id| {
        model::node_kind(*id)
            .map(|k| model::layer_label(k, *id))
            .unwrap_or_default()
    })
    .row_id(|id| day::id!(layer, *id))
    // The same context menu the canvas serves, per row: a summon on a row outside the
    // current selection selects that row first, so the menu describes what it acts on.
    .row_context_menu(|id| {
        if !model::selection().get_untracked().contains(id) {
            model::selection().set(vec![*id]);
            day::reactive::flush_sync();
        }
        crate::selection_context_menu()
    })
    .id(day::id!(layers))
    .any()
}

// ---------------------------------------------------------------------------
// Tabs: Canvas (document settings) and Selected (the selection's properties). The selection
// drives which one shows (see [`retarget`]), and the segmented control on top of the panel
// lets the user look at the other one until the selection next changes.
// ---------------------------------------------------------------------------

/// The inspector's two tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Canvas,
    Selected,
}

pub(crate) fn active_tab() -> Signal<Tab> {
    crate::scene().active_tab
}

/// Follow the selection: any selection change lands the inspector on the tab that talks about
/// it (Selected while something is selected, Canvas when nothing is). Called from the
/// selection bind in `root()`, so every path that selects (taps, drags, paste, undo/redo's
/// transient restoration) retargets without knowing about tabs.
pub(crate) fn retarget(selected: bool) {
    let want = if selected { Tab::Selected } else { Tab::Canvas };
    if active_tab().get_untracked() != want {
        active_tab().set(want);
    }
}

/// Re-runs the field bindings without a model change. Bumped after a rejected or clamped
/// edit, so the canonical text paints back over whatever was typed. A global `Signal` rather
/// than a `Trigger`, which has no scope-free constructor.
fn refresh() -> Signal<u64> {
    crate::scene().refresh
}

fn refresh_fields() {
    refresh().update(|v| *v = v.wrapping_add(1));
}

/// One numeric property of the selection: a row in the geometry section.
struct NumProp {
    /// The field's element id (`insp_x`), the dayscript target.
    id: fn() -> day::ElementId,
    label: fn() -> day::LocalizedText,
    /// The property's value on one node; `None` for a node it doesn't apply to.
    get: fn(u64) -> Option<f64>,
    /// Apply a typed value to one node. The caller wraps the whole selection in one undo
    /// group, labeled `undo` (a key of the same catalog the canvas gestures use).
    set: fn(u64, f64),
    undo: &'static str,
}

/// Move a node by (dx, dy): its own frame for a shape, every shape descendant for a group,
/// the same rule the canvas drag applies. A group's bounds are derived from its members
/// (`node_bounds`), so moving them is the whole move.
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

/// Width/height apply to shapes only; groups keep their derived union, the canvas's resize
/// rule (groups move; only shapes resize).
///
/// A line's fields are signed deltas to its far end, and the inspector shows the rectangle it
/// occupies: a typed extent therefore sets the delta's magnitude and leaves its direction
/// alone (a line running up-left keeps running up-left), and it may legitimately be zero. A
/// horizontal line has no height, where a rectangle floors at [`model::MIN_SIZE`].
fn set_extent(id: u64, v: f64, horizontal: bool) {
    let e = model::nodes().elem(id);
    let field = if horizontal { e.w() } else { e.h() };
    match e.kind().peek() {
        NodeKind::Group => {}
        NodeKind::Line => field.write_commit(v.max(0.0).copysign(field.peek())),
        NodeKind::Rect | NodeKind::Oval | NodeKind::Image => {
            field.write_commit(v.max(model::MIN_SIZE))
        }
        // A text node's frame is measured, so "make it 150 wide" means "scale the type until
        // it is": the point size takes the ratio, and the frame follows the type.
        NodeKind::Text => {
            let current = field.peek();
            if current > 0.0 {
                let size = (e.font_size().peek() * v.max(model::MIN_SIZE) / current)
                    .clamp(model::MIN_FONT_SIZE, model::MAX_FONT_SIZE);
                e.font_size().write_commit(size);
                model::refit_text(id, true);
            }
        }
    }
}

fn set_w(id: u64, v: f64) {
    set_extent(id, v, true);
}

fn set_h(id: u64, v: f64) {
    set_extent(id, v, false);
}

fn geometry_props() -> [NumProp; 4] {
    [
        NumProp {
            id: || day::id!(insp_x),
            label: crate::res::str::insp_x,
            get: |n| model::node_bounds(n).map(|b| b.0),
            set: set_x,
            undo: "move",
        },
        NumProp {
            id: || day::id!(insp_y),
            label: crate::res::str::insp_y,
            get: |n| model::node_bounds(n).map(|b| b.1),
            set: set_y,
            undo: "move",
        },
        NumProp {
            id: || day::id!(insp_w),
            label: crate::res::str::insp_w,
            get: |n| model::node_bounds(n).map(|b| b.2),
            set: set_w,
            undo: "resize",
        },
        NumProp {
            id: || day::id!(insp_h),
            label: crate::res::str::insp_h,
            get: |n| model::node_bounds(n).map(|b| b.3),
            set: set_h,
            undo: "resize",
        },
    ]
}

/// The display form of a value: one decimal at most, integers bare, so the walkthrough has
/// stable strings to assert against.
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
        // Track the selection and the store: a canvas drag writes previews the fields must
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

/// Selected nodes disagree on property `ix`: the "multi" placeholder's condition.
fn is_mixed(ix: usize) -> bool {
    !model::selection().get().is_empty() && common(ix, true).is_none()
}

/// The two-way binding between one property row's text field and the whole selection.
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
    /// Keystrokes are previews the model must not follow: "1" on the way to "125" is not a
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

/// Erases because the rows are collected into one `Vec<AnyPiece>` for the section below.
fn prop_row(ix: usize) -> AnyPiece {
    let p = &geometry_props()[ix];
    labeled(
        (p.label)(),
        text_field(window_binding(PropField { ix }))
            .placeholder(window_read(move || {
                if is_mixed(ix) {
                    crate::res::str::insp_multi().format()
                } else {
                    String::new()
                }
            }))
            .id((p.id)()),
    )
    .any()
}

// ---------------------------------------------------------------------------
// The Style section: fill color/opacity, stroke color/width/opacity. Style edits reach the
// shapes (a selected group restyles its members, the canvas's fill rule), and every
// commit is one undo unit labeled "style". Slider drags flow as previews (the canvas follows
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

/// One numeric style property, fanned out over the style targets. `read` shows the first
/// target's value (a numeric control has no "multi" form), and writes reach every target.
#[derive(Clone, Copy)]
struct StyleNum {
    read: fn(u64) -> f64,
    preview: fn(u64, f64),
    commit: fn(u64, f64),
    /// Which nodes the value fans out to. Style properties reach the SHAPES that draw, so a
    /// group's fill edits its members. Rotation reaches the selection itself: a group turns as
    /// one body about its own center, which is a thing only the group can do
    /// ([`model::set_rotation`]).
    targets: fn(bool) -> Vec<u64>,
}

impl Binding<f64> for StyleNum {
    fn read(&self) -> f64 {
        (self.targets)(true)
            .first()
            .map(|t| (self.read)(*t))
            .unwrap_or(0.0)
    }
    fn peek(&self) -> f64 {
        (self.targets)(false)
            .first()
            .map(|t| (self.read)(*t))
            .unwrap_or(0.0)
    }
    fn write(&self, v: f64) {
        self.write_commit(v);
    }
    fn write_preview(&self, v: f64) {
        for t in (self.targets)(false) {
            (self.preview)(t, v);
        }
    }
    fn write_commit(&self, v: f64) {
        let targets = (self.targets)(false);
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

/// The rotation stepper's value, shown as localized degrees ("45°") and accepting one typed
/// back with or without the suffix. The stepper drives a `Binding<f64>`, so this wraps the
/// bound number rather than replacing it.
#[derive(Clone, Copy)]
struct DegField {
    num: StyleNum,
}

impl Binding<f64> for DegField {
    fn read(&self) -> f64 {
        self.num.read()
    }
    fn peek(&self) -> f64 {
        self.num.peek()
    }
    fn write(&self, v: f64) {
        self.num.write(v.rem_euclid(360.0));
    }
    fn write_preview(&self, v: f64) {
        self.num.write_preview(v.rem_euclid(360.0));
    }
    fn write_commit(&self, v: f64) {
        self.num.write_commit(v.rem_euclid(360.0));
    }
}

/// The opacity row's percentage field: the same bound value, shown as a localized percent
/// ("85%") and accepting one typed back with or without the suffix.
#[derive(Clone, Copy)]
struct PctField {
    num: StyleNum,
}

fn percent_field_text(value: f64) -> String {
    // This is the entire editable value, not an interpolation inside surrounding prose.
    // WinUI's TextBox displays Fluent's isolate controls as FSI/PDI boxes.
    crate::res::str::insp_percent((value * 100.0).round() as i64)
        .format()
        .chars()
        .filter(|c| !matches!(c, '\u{2068}' | '\u{2069}'))
        .collect()
}

impl Binding<String> for PctField {
    fn read(&self) -> String {
        percent_field_text(self.num.read())
    }
    fn peek(&self) -> String {
        percent_field_text(self.num.peek())
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
    targets: style_targets,
};

const STROKE_OPACITY: StyleNum = StyleNum {
    read: |t| model::nodes().elem(t).stroke_opacity().read(),
    preview: |t, v| model::nodes().elem(t).stroke_opacity().write_preview(v),
    commit: |t, v| model::nodes().elem(t).stroke_opacity().write_commit(v),
    targets: style_targets,
};

const STROKE_WIDTH: StyleNum = StyleNum {
    read: |t| model::nodes().elem(t).stroke_width().read(),
    preview: |t, v| {
        model::nodes()
            .elem(t)
            .stroke_width()
            .write_preview(v.max(0.0))
    },
    commit: |t, v| {
        model::nodes()
            .elem(t)
            .stroke_width()
            .write_commit(v.max(0.0))
    },
    targets: style_targets,
};

const ROTATION: StyleNum = StyleNum {
    read: |t| model::nodes().elem(t).rotation().read(),
    preview: |t, v| model::set_rotation(t, v.rem_euclid(360.0), false),
    commit: |t, v| model::set_rotation(t, v.rem_euclid(360.0), true),
    // The selection, not its shapes: turning a group means turning the whole arrangement
    // about one center, which is lost the moment the value fans out to the members.
    targets: |tracked| {
        if tracked {
            model::nodes().with(|_| {});
            model::selection().get()
        } else {
            model::selection().get_untracked()
        }
    },
};

const CORNER_RADIUS: StyleNum = StyleNum {
    read: |t| model::nodes().elem(t).corner_radius().read(),
    preview: |t, v| {
        model::nodes()
            .elem(t)
            .corner_radius()
            .write_preview(v.max(0.0))
    },
    commit: |t, v| {
        model::nodes()
            .elem(t)
            .corner_radius()
            .write_commit(v.max(0.0))
    },
    targets: style_targets,
};

const FILL_COLOR: StyleColor = StyleColor {
    read: |t| {
        model::nodes()
            .elem(t)
            .fill()
            .with(|f| f.cloned().unwrap_or_default())
    },
    commit: |t, hex| model::nodes().elem(t).fill().write_commit(hex.to_string()),
};

/// The Canvas tab's background well: the document's single settings row, committed as one
/// labeled undo unit; [`model::set_background`] owns the grouping.
#[derive(Clone, Copy)]
struct BgColor;

impl Binding<Color> for BgColor {
    fn read(&self) -> Color {
        parse_hex(&model::background())
    }
    fn peek(&self) -> Color {
        parse_hex(&model::background())
    }
    fn write(&self, c: Color) {
        model::set_background(&to_hex(c));
    }
}

const STROKE_COLOR: StyleColor = StyleColor {
    read: |t| {
        model::nodes()
            .elem(t)
            .stroke()
            .with(|s| s.cloned().unwrap_or_default())
    },
    commit: |t, hex| {
        model::nodes()
            .elem(t)
            .stroke()
            .write_commit(hex.to_string())
    },
};

/// A slider with its attached percentage field, the opacity rows' control.
fn opacity_row(num: StyleNum, slider_id: day::ElementId, field: day::ElementId) -> impl Piece {
    row((
        slider(window_binding(num))
            .range(0.0..=1.0)
            .step(0.01)
            .id(slider_id)
            .grow(),
        // 72, not 64: Material's text field pads its text enough that "100%" clipped at 64.
        text_field(window_binding(PctField { num }))
            .id(field)
            .width(72.0),
    ))
    .spacing(6.0)
    .align(VAlign::Center)
}

/// Whether every shape the style section would edit is a rectangle, the condition for
/// showing a corner-radius row at all. An oval has no corners, and a mixed selection has no
/// one answer, so both hide the row rather than offering a field that does nothing.
fn selection_is_rects() -> bool {
    every_target(|k| match k {
        NodeKind::Rect => true,
        NodeKind::Oval | NodeKind::Line | NodeKind::Group | NodeKind::Text | NodeKind::Image => {
            false
        }
    })
}

/// Every target is a text node, the condition for the Text section (content, font, style,
/// size). A mixed selection shows none of it rather than a field that edits only some.
fn selection_is_text() -> bool {
    every_target(|k| match k {
        NodeKind::Text => true,
        NodeKind::Rect | NodeKind::Oval | NodeKind::Line | NodeKind::Group | NodeKind::Image => {
            false
        }
    })
}

/// A stroke outlines a shape or IS a line; type has neither, so a text node's inspector has
/// no stroke rows at all.
fn selection_has_stroke() -> bool {
    every_target(|k| match k {
        NodeKind::Rect | NodeKind::Oval | NodeKind::Line => true,
        NodeKind::Group | NodeKind::Text | NodeKind::Image => false,
    })
}

/// Every shape the style section would edit answers `yes`, and there is at least one. The
/// panel asks this before offering a property, so a row is present exactly when it applies to
/// the whole selection; a mixed selection has no one answer and shows nothing rather than a
/// field that edits only some of what is selected.
fn every_target(applies: impl Fn(NodeKind) -> bool) -> bool {
    let targets = style_targets(true);
    !targets.is_empty()
        && targets
            .iter()
            .all(|t| applies(model::nodes().elem(*t).kind().read()))
}

/// A fill needs an interior. A line has none; it is its stroke. Type is filled: its fill is
/// the color it is set in.
fn selection_has_fill() -> bool {
    every_target(|k| match k {
        NodeKind::Rect | NodeKind::Oval | NodeKind::Text => true,
        NodeKind::Line | NodeKind::Group | NodeKind::Image => false,
    })
}

/// A rotation turns a frame about its center; a group turns about its own, carrying its
/// members around it. A line has no frame of its own (its direction is where its two ends
/// are), so it is turned by dragging them, not by an angle field.
///
/// Asked of the selection rather than its shapes, matching where the field writes: a group
/// answers for itself, so a group of lines can still be turned as a body even though no line
/// in it takes an angle.
fn selection_can_rotate() -> bool {
    let sel = model::selection().get();
    model::nodes().with(|_| {});
    !sel.is_empty()
        && sel.iter().all(|t| {
            matches!(
                model::nodes().elem(*t).kind().read(),
                NodeKind::Rect
                    | NodeKind::Oval
                    | NodeKind::Group
                    | NodeKind::Text
                    | NodeKind::Image
            )
        })
}

// ---------------------------------------------------------------------------
// The Text section: a text node's content, family, style and point size. Every edit
// re-measures the frame (`model::refit_text`) inside the same undo unit, so the outline
// always hugs the type.
// ---------------------------------------------------------------------------

/// The text nodes among the style targets.
fn text_targets(tracked: bool) -> Vec<u64> {
    style_targets(tracked)
        .into_iter()
        .filter(|t| model::nodes().elem(*t).kind().peek() == NodeKind::Text)
        .collect()
}

/// The content field's binding: the common text when every target agrees, else empty; a
/// commit writes every target and refits it as one undo unit. Keystrokes are previews the
/// model does not follow (the geometry fields' rule).
#[derive(Clone, Copy)]
struct TextContent;

impl TextContent {
    fn common(tracked: bool) -> String {
        let targets = text_targets(tracked);
        refresh().track();
        let mut texts = targets.iter().map(|t| {
            model::nodes()
                .elem(*t)
                .text()
                .with(|s| s.cloned().unwrap_or_default())
        });
        let Some(first) = texts.next() else {
            return String::new();
        };
        if texts.all(|t| t == first) {
            first
        } else {
            String::new()
        }
    }
}

impl Binding<String> for TextContent {
    fn read(&self) -> String {
        Self::common(true)
    }
    fn peek(&self) -> String {
        Self::common(false)
    }
    fn write(&self, s: String) {
        self.write_commit(s);
    }
    fn write_preview(&self, _s: String) {}
    fn write_commit(&self, s: String) {
        let targets = text_targets(false);
        // Nothing to say (no target, or the words every target already has): no unit.
        if targets.is_empty() || s == Self::common(false) {
            refresh_fields();
            return;
        }
        model::undo_stack().grouped("edit-text", || {
            for t in targets {
                model::nodes().elem(t).text().write_commit(s.clone());
                model::refit_text(t, true);
            }
        });
        refresh_fields();
    }
}

/// The point size: a fan-out number like the style rows, with a refit on every write, so a
/// stepper drag previews the frame growing with the type.
const FONT_SIZE: StyleNum = StyleNum {
    read: |t| model::nodes().elem(t).font_size().read(),
    preview: |t, v| {
        model::nodes()
            .elem(t)
            .font_size()
            .write_preview(v.clamp(model::MIN_FONT_SIZE, model::MAX_FONT_SIZE));
        model::refit_text(t, false);
    },
    commit: |t, v| {
        model::nodes()
            .elem(t)
            .font_size()
            .write_commit(v.clamp(model::MIN_FONT_SIZE, model::MAX_FONT_SIZE));
        model::refit_text(t, true);
    },
    targets: text_targets,
};

/// The platform's families, by name: the font menu's options after "System".
fn family_names() -> Vec<String> {
    day::font_families()
        .iter()
        .map(|f| f.family.clone())
        .collect()
}

/// The font menu's options: the default face first (an empty family), then every family the
/// platform lists, each its own label.
fn family_options() -> Vec<(String, String)> {
    let mut options = vec![(String::new(), crate::res::str::font_system().format())];
    options.extend(family_names().into_iter().map(|f| (f.clone(), f)));
    options
}

/// The first text target's family, tracked or not (empty = the default face).
fn current_family(tracked: bool) -> String {
    text_targets(tracked)
        .first()
        .map(|t| {
            model::nodes()
                .elem(*t)
                .font_family()
                .with(|f| f.cloned().unwrap_or_default())
        })
        .unwrap_or_default()
}

/// The font menu's binding: the first target's family as the platform lists it, empty for the
/// default face (and for a family the platform does not list).
#[derive(Clone, Copy)]
struct FamilyChoice;

impl FamilyChoice {
    fn listed(tracked: bool) -> String {
        let family = current_family(tracked);
        if family.is_empty() {
            return String::new();
        }
        family_names()
            .into_iter()
            .find(|f| f.eq_ignore_ascii_case(&family))
            .unwrap_or_default()
    }
}

impl Binding<String> for FamilyChoice {
    fn read(&self) -> String {
        Self::listed(true)
    }
    fn peek(&self) -> String {
        Self::listed(false)
    }
    fn write(&self, family: String) {
        let targets = text_targets(false);
        // A re-selection of the current entry (a native control echoing the mark Day just
        // moved) is not an edit, so it records no unit.
        if targets.is_empty() || family == Self::listed(false) {
            return;
        }
        model::undo_stack().grouped("style", || {
            for t in targets {
                model::nodes()
                    .elem(t)
                    .font_family()
                    .write_commit(family.clone());
                model::refit_text(t, true);
            }
        });
    }
}

/// A face as the style menu names it: weight and slant.
type Face = (FontWeight, bool);

/// The faces a family offers, as (weight, italic) pairs sorted light→heavy, upright before
/// slanted. A family the platform did not describe (the default face, or one it lists
/// without faces) offers the four synthesized styles every platform can draw, so the style
/// menu always has Regular / Bold / Italic / Bold Italic in that order for the System face.
fn styles_for(family: &str) -> Vec<Face> {
    let listed = (!family.is_empty())
        .then(|| {
            day::font_families()
                .iter()
                .find(|f| f.family.eq_ignore_ascii_case(family))
                .map(|f| {
                    let mut faces: Vec<Face> = f
                        .faces
                        .iter()
                        .map(|face| (face.weight, face.italic))
                        .collect();
                    faces.sort_by_key(|(w, i)| (*i, *w));
                    faces.dedup();
                    faces
                })
        })
        .flatten()
        .unwrap_or_default();
    if listed.len() > 1 {
        listed
    } else {
        vec![
            (FontWeight::Regular, false),
            (FontWeight::Bold, false),
            (FontWeight::Regular, true),
            (FontWeight::Bold, true),
        ]
    }
}

/// A style's menu label from its weight and slant: localized, never the platform's face name,
/// so a walkthrough can assert it by key on every target.
fn style_label(weight: FontWeight, italic: bool) -> String {
    use crate::res::str as s;
    match (weight, italic) {
        (FontWeight::Regular, false) => s::style_regular().format(),
        (FontWeight::Bold, false) => s::style_bold().format(),
        (FontWeight::Regular, true) => s::style_italic().format(),
        (FontWeight::Bold, true) => s::style_bold_italic().format(),
        (w, italic) => {
            let name = match w {
                FontWeight::UltraLight => s::style_ultralight().format(),
                FontWeight::Thin => s::style_thin().format(),
                FontWeight::Light => s::style_light().format(),
                FontWeight::Medium => s::style_medium().format(),
                FontWeight::Semibold => s::style_semibold().format(),
                FontWeight::Heavy => s::style_heavy().format(),
                FontWeight::Black => s::style_black().format(),
                FontWeight::Regular | FontWeight::Bold => s::style_regular().format(),
            };
            if italic {
                s::style_weight_italic(name).format()
            } else {
                name
            }
        }
    }
}

/// The style menu's options for the first target's family. Reactive, so choosing another
/// family re-lists them.
fn style_options() -> Vec<(Face, String)> {
    styles_for(&current_family(true))
        .into_iter()
        .map(|(w, i)| ((w, i), style_label(w, i)))
        .collect()
}

/// The style menu's binding over [`styles_for`]: the first target's face, or the family's
/// first listed face when the target's is not among them.
#[derive(Clone, Copy)]
struct StyleChoice;

impl StyleChoice {
    fn face(tracked: bool) -> Face {
        let styles = styles_for(&current_family(tracked));
        let fallback = styles
            .first()
            .copied()
            .unwrap_or((FontWeight::Regular, false));
        let targets = text_targets(tracked);
        let Some(first) = targets.first() else {
            return fallback;
        };
        let e = model::nodes().elem(*first);
        let want = (
            model::weight_of(e.font_weight().peek()),
            e.font_italic().peek(),
        );
        styles.into_iter().find(|s| *s == want).unwrap_or(fallback)
    }
}

impl Binding<Face> for StyleChoice {
    fn read(&self) -> Face {
        Self::face(true)
    }
    fn peek(&self) -> Face {
        Self::face(false)
    }
    fn write(&self, face: Face) {
        let targets = text_targets(false);
        if targets.is_empty() || face == Self::face(false) {
            return;
        }
        let (weight, italic) = face;
        model::undo_stack().grouped("style", || {
            for t in targets {
                let e = model::nodes().elem(t);
                e.font_weight().write_commit(model::weight_value(weight));
                e.font_italic().write_commit(italic);
                model::refit_text(t, true);
            }
        });
    }
}

/// The Text section, mounted only while every target is a text node (`when` disposes it
/// otherwise, so its fields are never dead dayscript targets on a shape).
fn text_section() -> AnyPiece {
    when(window_read(selection_is_text), || {
        section((
            labeled(
                crate::res::str::insp_content(),
                text_field(window_binding(TextContent)).id(day::id!(insp_text)),
            ),
            labeled(
                crate::res::str::insp_font(),
                picker(window_binding(FamilyChoice), window_read(family_options))
                    .id(day::id!(insp_font)),
            ),
            labeled(
                crate::res::str::insp_style(),
                picker(window_binding(StyleChoice), window_read(style_options))
                    .id(day::id!(insp_style)),
            ),
            labeled(
                crate::res::str::insp_size(),
                day::stepper::stepper(window_binding(FONT_SIZE))
                    .range(model::MIN_FONT_SIZE..=model::MAX_FONT_SIZE)
                    .step(1.0)
                    .decimals(0)
                    .id(day::id!(insp_size))
                    .grow(),
            ),
        ))
        .title(crate::res::str::insp_text())
    })
    .any()
}

/// Keep the native color well above its opacity controls. The inspector's label column
/// leaves too little width for all three controls side by side on some toolkits.
fn paint_row(
    well: StyleColor,
    well_id: day::ElementId,
    num: StyleNum,
    slider_id: day::ElementId,
    field: day::ElementId,
) -> impl Piece {
    column((
        day::colorpicker::color_picker(window_binding(well)).id(well_id),
        opacity_row(num, slider_id, field).grow(),
    ))
    .spacing(4.0)
    .align(HAlign::Leading)
}

fn style_section() -> impl Piece {
    section((
        when(
            window_read(|| every_target(|k| k == NodeKind::Image)),
            || {
                labeled(
                    crate::res::str::insp_opacity(),
                    opacity_row(
                        FILL_OPACITY,
                        day::id!(insp_image_op_slider),
                        day::id!(insp_image_op),
                    )
                    .grow(),
                )
            },
        ),
        // The fill row mounts only for shapes that have an interior (docs: `when` disposes
        // the arm, so a line's inspector has no fill row at all rather than a dead one).
        when(window_read(selection_has_fill), || {
            labeled(
                crate::res::str::insp_fill(),
                paint_row(
                    FILL_COLOR,
                    day::id!(insp_fill),
                    FILL_OPACITY,
                    day::id!(insp_fill_op_slider),
                    day::id!(insp_fill_op),
                )
                .grow(),
            )
        }),
        // The stroke pair mounts for what strokes: shapes and lines, never type.
        when(window_read(selection_has_stroke), || {
            labeled(
                crate::res::str::insp_stroke(),
                paint_row(
                    STROKE_COLOR,
                    day::id!(insp_stroke),
                    STROKE_OPACITY,
                    day::id!(insp_stroke_op_slider),
                    day::id!(insp_stroke_op),
                )
                .grow(),
            )
        }),
        when(window_read(selection_has_stroke), || {
            labeled(
                crate::res::str::insp_stroke_width(),
                day::stepper::stepper(window_binding(STROKE_WIDTH))
                    .range(0.0..=64.0)
                    .step(1.0)
                    .decimals(0)
                    .id(day::id!(insp_stroke_w))
                    // Same right edge as the paint rows above it.
                    .grow(),
            )
        }),
    ))
    .title(crate::res::str::insp_style())
}

/// The geometry section's transform rows: rotation always, corner radius only where it means
/// something. Both are steppers over the same fan-out binding the style numbers use.
fn rotation_row() -> AnyPiece {
    when(window_read(selection_can_rotate), rotation_field).any()
}

/// The angle field itself; see [`rotation_row`] for when it is shown.
fn rotation_field() -> impl Piece {
    labeled(
        crate::res::str::insp_rotation(),
        day::stepper::stepper(window_binding(DegField { num: ROTATION }))
            // 360 is IN the range so the stepper's own up-arrow can reach it; the binding
            // wraps it back to 0, which is what a full turn means.
            .range(0.0..=360.0)
            .step(1.0)
            .decimals(0)
            .id(day::id!(insp_rotation))
            // Fill the row like the text fields above, so every control in the section shares
            // one right edge instead of the steppers ending short.
            .grow(),
    )
}

/// The corner-radius row, mounted only where the property means something. `when` mounts and
/// disposes it with the selection's kind, because a hidden field would still be a dayscript
/// target, and a disabled one still reads as a property ovals have.
fn corner_row() -> AnyPiece {
    when(window_read(selection_is_rects), || {
        labeled(
            crate::res::str::insp_corner(),
            day::stepper::stepper(window_binding(CORNER_RADIUS))
                .range(0.0..=200.0)
                .step(1.0)
                .decimals(0)
                .id(day::id!(insp_corner))
                .grow(),
        )
    })
    .any()
}

/// The two tabs with their labels. The second counts the selection, pluralized by the catalog
/// (Fluent's plural nav), so a language with more forms than English adds them there, not here.
fn tab_options() -> Vec<(Tab, day::LocalizedText)> {
    let n = model::selection().get().len() as i64;
    vec![
        (Tab::Canvas, crate::res::str::insp_tab_canvas()),
        (Tab::Selected, crate::res::str::insp_tab_selected(n)),
    ]
}

/// The Selected tab: one form, one section per property group of the selection. New
/// per-selection sections slot in here.
fn selected_panel() -> impl Piece {
    // The four frame fields, then the transform rows, every row a direct child of the
    // section, so the section's row rhythm spaces all six alike. (Nesting the last two in
    // a column of their own gave them that column's spacing instead, and they read as a
    // cramped afterthought under Height.)
    let mut rows: Vec<AnyPiece> = (0..geometry_props().len()).map(prop_row).collect();
    rows.push(rotation_row());
    rows.push(corner_row());
    form((
        section(rows).title(crate::res::str::insp_geometry()),
        text_section(),
        style_section(),
    ))
}

/// The Canvas tab: the document's own properties. New document-level settings slot in here.
fn canvas_panel() -> impl Piece {
    form((section((labeled(
        crate::res::str::insp_background(),
        day::colorpicker::color_picker(BgColor).id(day::id!(insp_bg)),
    ),)),))
}

/// The panel content: the tab strip over whichever tab is active. The padding is the pane's
/// breathing room; the inspector hosts hand the panel the full pane rect, so the inset
/// lives here, once, for every target.
pub(crate) fn panel() -> impl Piece {
    let scope = day::reactive::Scope::current();
    let tab = active_tab();
    column((
        // The Selected tab names its selection ("No Items" / "1 Item" / "3 Items"), so its
        // label changes with the count: a reactive option list (docs/picker.md).
        picker(tab, move || scope.enter(tab_options))
            .segmented()
            .id(day::id!(insp_tab)),
        switch(
            move || tab.get(),
            |tab| match tab {
                Tab::Canvas => canvas_panel().any(),
                Tab::Selected => selected_panel().any(),
            },
        ),
    ))
    .spacing(10.0)
    .padding(Insets::symmetric(14.0, 12.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::install_test_doc;

    const X: PropField = PropField { ix: 0 };
    const Y: PropField = PropField { ix: 1 };
    const W: PropField = PropField { ix: 2 };

    #[test]
    fn inspector_binding_keeps_its_window_when_another_scope_is_current() {
        let _doc = install_test_doc();
        let first = model::place_shape(NodeKind::Rect, 10.0, 20.0);
        let second = model::place_shape(NodeKind::Rect, 100.0, 200.0);
        let drawing = day::reactive::Scope::child();
        let other = day::reactive::Scope::child();
        let (scene, field, options) = drawing.enter(|| {
            let scene = crate::Scene::create();
            drawing.provide(scene.clone());
            scene.selection.set(vec![first]);
            (scene, window_binding(X), window_read(tab_options))
        });
        other.enter(|| {
            let scene = crate::Scene::create();
            other.provide(scene.clone());
            scene.selection.set(vec![second]);
            assert_eq!(field.read(), "10");
            field.write_commit("25".into());
            assert_eq!(model::node_bounds(first).unwrap().0, 25.0);
            assert_eq!(model::node_bounds(second).unwrap().0, 100.0);
        });
        let observed = Signal::new(String::new());
        let read = field.clone();
        drawing.enter(|| {
            day::reactive::bind(
                move || read.read(),
                move |v: &String| observed.set(v.clone()),
            );
        });
        // A background window's selection still updates its own bindings and tab labels.
        other.enter(|| scene.selection.set(vec![second]));
        day::reactive::flush_sync();
        assert_eq!(observed.get_untracked(), "100");
        assert_eq!(options()[1].0, Tab::Selected);
        drawing.dispose();
        other.dispose();
    }

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

        // A selected group restyles its member shapes.
        let (ea, eb) = (model::nodes().elem(a), model::nodes().elem(b));
        FILL_OPACITY.write_preview(0.4);
        assert_eq!(
            ea.fill_opacity().peek(),
            0.4,
            "previews reach the canvas live"
        );
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

        let pct = PctField { num: FILL_OPACITY };
        // Assert the actual editable text: normalizing away isolates here hid the
        // visible FSI/PDI boxes in native Windows text fields.
        assert_eq!(pct.peek(), "100%");
        assert_eq!(pct.read(), "100%");
        pct.write_commit("25%".into());
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).fill_opacity().peek(), 0.25);
        pct.write_commit("80".into());
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).fill_opacity().peek(), 0.8);
        assert_eq!(pct.peek(), "80%");
        pct.write_commit("\u{2068}50\u{2069}%".into());
        day::reactive::flush_sync();
        assert_eq!(pct.peek(), "50%");
        // Out-of-range clamps; garbage leaves the value alone.
        pct.write_commit("250".into());
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).fill_opacity().peek(), 1.0);
        pct.write_commit("opaque".into());
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(a).fill_opacity().peek(), 1.0);
    }

    #[test]
    fn the_background_well_writes_one_labeled_unit_and_reads_back() {
        let doc = install_test_doc();
        assert_eq!(to_hex(Binding::<Color>::peek(&BgColor)), "#FFFFFF", "seed");
        Binding::<Color>::write(&BgColor, Color::hex(0x336699));
        day::reactive::flush_sync();
        assert_eq!(model::background(), "#336699");
        assert_eq!(to_hex(Binding::<Color>::peek(&BgColor)), "#336699");
        assert!(doc.stack.undo(), "one unit takes it back");
        day::reactive::flush_sync();
        assert_eq!(model::background(), "#FFFFFF");
    }

    #[test]
    fn the_selected_tab_names_its_count_pluralized() {
        let _doc = install_test_doc();
        crate::res::locales::install();
        fn visible(s: String) -> String {
            s.chars()
                .filter(|c| !matches!(c, '\u{2068}' | '\u{2069}'))
                .collect()
        }
        let label = || visible(tab_options()[1].1.format());

        model::selection().set(Vec::new());
        assert_eq!(label(), "No Items");
        let a = model::place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = model::place_shape(NodeKind::Oval, 200.0, 0.0);
        day::reactive::flush_sync();
        model::selection().set(vec![a]);
        assert_eq!(label(), "1 Item");
        model::selection().set(vec![a, b]);
        assert_eq!(label(), "2 Items");
    }

    #[test]
    fn corner_radius_shows_for_rectangles_only_and_rotation_wraps() {
        let _doc = install_test_doc();
        let r = model::place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let o = model::place_shape(NodeKind::Oval, 200.0, 0.0);
        day::reactive::flush_sync();

        model::selection().set(vec![r]);
        assert!(selection_is_rects(), "a rectangle has corners");
        model::selection().set(vec![o]);
        assert!(!selection_is_rects(), "an oval has none");
        model::selection().set(vec![r, o]);
        assert!(!selection_is_rects(), "a mixed selection has no one answer");
        model::selection().set(Vec::new());
        assert!(!selection_is_rects(), "nothing selected shows nothing");

        // Rotation is an angle on a circle: 360 is 0, and −1 is 359.
        model::selection().set(vec![r]);
        let deg = DegField { num: ROTATION };
        deg.write_commit(45.0);
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(r).rotation().peek(), 45.0);
        deg.write_commit(360.0);
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(r).rotation().peek(), 0.0);
        deg.write_commit(-1.0);
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(r).rotation().peek(), 359.0);

        // Corner radius fans out and floors at 0, as one undo unit.
        CORNER_RADIUS.write_commit(12.0);
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(r).corner_radius().peek(), 12.0);
        CORNER_RADIUS.write_commit(-5.0);
        day::reactive::flush_sync();
        assert_eq!(model::nodes().elem(r).corner_radius().peek(), 0.0);
    }

    #[test]
    fn a_line_offers_only_the_properties_it_has() {
        let _doc = install_test_doc();
        let line = model::place_shape(NodeKind::Line, 0.0, 0.0);
        day::reactive::flush_sync();
        let rect = model::place_shape(NodeKind::Rect, 200.0, 0.0);
        day::reactive::flush_sync();

        // A line is its stroke: it has no fill, angle field or corners.
        model::selection().set(vec![line]);
        assert!(!selection_has_fill());
        assert!(!selection_can_rotate());
        assert!(!selection_is_rects());

        // A rectangle has all three.
        model::selection().set(vec![rect]);
        assert!(selection_has_fill());
        assert!(selection_can_rotate());
        assert!(selection_is_rects());

        // A mixed selection has no one answer, so it offers none of them rather than editing
        // half of what is selected.
        model::selection().set(vec![line, rect]);
        assert!(!selection_has_fill());
        assert!(!selection_can_rotate());
        assert!(!selection_is_rects());
    }

    #[test]
    fn a_typed_extent_sets_a_lines_length_and_keeps_its_direction() {
        let _doc = install_test_doc();
        let id = model::place_shape(NodeKind::Line, 100.0, 100.0);
        day::reactive::flush_sync();
        let e = model::nodes().elem(id);
        e.w().write(-40.0);
        e.h().write(30.0);
        day::reactive::flush_sync();
        model::selection().set(vec![id]);

        // The fields show the rectangle the line occupies…
        assert_eq!(W.peek(), "40");
        // …and typing an extent scales the delta without turning the line around.
        W.write_commit("80".into());
        day::reactive::flush_sync();
        assert_eq!(e.w().peek(), -80.0, "still running leftward");
        // A line may legitimately have no height, where a rectangle floors at MIN_SIZE.
        let h: PropField = PropField { ix: 3 };
        h.write_commit("0".into());
        day::reactive::flush_sync();
        assert_eq!(e.h().peek(), 0.0);
        model::selection().set(vec![model::place_shape(NodeKind::Rect, 0.0, 0.0)]);
        day::reactive::flush_sync();
        W.write_commit("1".into());
        day::reactive::flush_sync();
        assert_eq!(
            model::node_bounds(model::selection().get_untracked()[0]).map(|b| b.2),
            Some(model::MIN_SIZE)
        );
    }

    #[test]
    fn retargeting_lands_on_the_tab_that_matches_the_selection() {
        let _doc = install_test_doc();
        retarget(false);
        assert_eq!(active_tab().get_untracked(), Tab::Canvas);
        retarget(true);
        assert_eq!(active_tab().get_untracked(), Tab::Selected);
        retarget(false);
        assert_eq!(active_tab().get_untracked(), Tab::Canvas);
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

    #[test]
    fn a_text_node_offers_text_rows_and_no_stroke_rows() {
        let _doc = install_test_doc();
        let t = model::place_shape(NodeKind::Text, 10.0, 10.0);
        let r = model::place_shape(NodeKind::Rect, 200.0, 10.0);
        day::reactive::flush_sync();
        model::selection().set(vec![t]);
        assert!(selection_is_text());
        assert!(selection_has_fill(), "the fill is the text's color");
        assert!(selection_can_rotate());
        assert!(!selection_has_stroke());
        assert!(!selection_is_rects());
        model::selection().set(vec![r]);
        assert!(!selection_is_text() && selection_has_stroke());
        model::selection().set(vec![t, r]);
        assert!(
            !selection_is_text(),
            "a mixed selection shows neither section"
        );
        assert!(!selection_has_stroke());
    }

    #[test]
    fn typed_text_refits_the_frame_as_one_undo_unit() {
        let doc = install_test_doc();
        let t = model::place_shape(NodeKind::Text, 10.0, 10.0);
        day::reactive::flush_sync();
        model::selection().set(vec![t]);
        let store = model::nodes();
        let w0 = store.elem(t).w().peek();
        TextContent.write_commit("A much longer line of type".to_string());
        assert_eq!(TextContent.peek(), "A much longer line of type");
        assert!(
            store.elem(t).w().peek() > w0,
            "the frame grew with the words"
        );
        // One undo takes back both the words and the refit: they were one unit.
        assert!(doc.stack.undo());
        assert_eq!(store.elem(t).w().peek(), w0);
        assert_eq!(
            store.elem(t).text().peek(),
            crate::res::str::text_default().format()
        );
    }

    #[test]
    fn the_style_menu_lists_four_synthesized_faces_for_the_system_family() {
        let _doc = install_test_doc();
        let t = model::place_shape(NodeKind::Text, 10.0, 10.0);
        day::reactive::flush_sync();
        model::selection().set(vec![t]);
        let styles = styles_for("");
        assert_eq!(styles.len(), 4);
        assert_eq!(styles[1], (FontWeight::Bold, false));
        assert_eq!(StyleChoice.peek(), (FontWeight::Regular, false));
        StyleChoice.write((FontWeight::Bold, false));
        let e = model::nodes().elem(t);
        assert_eq!(e.font_weight().peek(), 700);
        assert!(!e.font_italic().peek());
        assert_eq!(StyleChoice.peek(), (FontWeight::Bold, false));
        StyleChoice.write((FontWeight::Bold, true));
        assert!(e.font_italic().peek() && e.font_weight().peek() == 700);
        assert_eq!(
            FamilyChoice.peek(),
            "",
            "the default face is the first entry"
        );
    }

    #[test]
    fn a_typed_width_scales_a_text_nodes_type() {
        let _doc = install_test_doc();
        let t = model::place_shape(NodeKind::Text, 10.0, 10.0);
        day::reactive::flush_sync();
        model::selection().set(vec![t]);
        let e = model::nodes().elem(t);
        let w0 = e.w().peek();
        W.write_commit(format!("{}", w0 * 2.0));
        assert!(
            (e.font_size().peek() - 48.0).abs() < 1e-6,
            "{}",
            e.font_size().peek()
        );
        assert!((e.w().peek() - w0 * 2.0).abs() < 1e-6);
    }
}
