//! Day Sketch, a vector drawing editor built on [Day](https://daybrite.dev), and the stress
//! test the day-model/day-persistence design was drafted against: a drawing is a SQLite file,
//! the scene is one observable table, drags edit it live through preview sessions, and every
//! operation (placement, move, resize, group, arrange) is one undoable turn, fronted by the
//! platform's undo system where it has one.

use day::prelude::*;
use day_piece_settings::prelude::*;

mod canvas;
mod clipboard;
mod images;
mod inspector;
mod model;
mod transfer;

// The mobile / embedded entry point. Expands to the export each platform's shell binds against,
// and to nothing at all on a plain cargo desktop build, where src/main.rs is the entry instead.
day::day_start!("Day Sketch", root);

/// Typed constants for the files under `resource/`, generated at build time by `day-build`.
pub mod res {
    include!(concat!(env!("OUT_DIR"), "/day_resources.rs"));
}

/// Whether a drag snaps to the alignment guides (canvas.rs): the Editing group's one setting,
/// persisted under `sketch.snap` (https://daybrite.dev/docs/settings).
pub(crate) static SNAP: Pref<bool> = Pref::new("sketch.snap", || true);

/// ⌘/Ctrl + the localized key: the letter comes from the command's `.key` attribute in the
/// catalog (docs/localization.md), so a locale may override it while every other locale
/// inherits the default's. The modifier scheme stays semantic, here in code.
fn cmd(key: day::LocalizedText) -> Shortcut {
    Shortcut::primary(key.format())
}

fn cmd_shift(key: day::LocalizedText) -> Shortcut {
    Shortcut::primary(key.format()).shift()
}

fn cmd_alt(key: day::LocalizedText) -> Shortcut {
    Shortcut::primary(key.format()).alt()
}

/// The arrange commands, one list served three ways: the Arrange menu, the canvas context
/// menu, and (with icons) the window toolbar.
pub(crate) fn arrange_menu_entries() -> Vec<MenuEntry> {
    arrange_entries(true)
}

/// `track` = read the selection reactively (the menu-bar builder re-runs on change). The
/// canvas context menu is lowered once at build, so it keeps every item enabled and lets the
/// actions no-op on an empty selection instead of freezing the launch-time state.
fn arrange_entries(track: bool) -> Vec<MenuEntry> {
    let sel = if track {
        model::selection().get()
    } else {
        model::selection().get_untracked()
    };
    let some = !track || !sel.is_empty();
    let two = !track || sel.len() >= 2;
    vec![
        menu_item(res::str::menu_group())
            .action(model::group_selection)
            .shortcut(cmd(res::str::menu_group_key()))
            .enabled(two),
        menu_item(res::str::menu_ungroup())
            .action(model::ungroup_selection)
            .shortcut(cmd_shift(res::str::menu_ungroup_key()))
            .enabled(some),
        menu_separator(),
        menu_item(res::str::menu_forward())
            .action(|| model::arrange_named(model::Arrange::Up))
            .shortcut(cmd(res::str::menu_forward_key()))
            .enabled(some),
        menu_item(res::str::menu_backward())
            .action(|| model::arrange_named(model::Arrange::Down))
            .shortcut(cmd(res::str::menu_backward_key()))
            .enabled(some),
        menu_item(res::str::menu_front())
            .action(|| model::arrange_named(model::Arrange::Top))
            .shortcut(cmd_shift(res::str::menu_front_key()))
            .enabled(some),
        menu_item(res::str::menu_back())
            .action(|| model::arrange_named(model::Arrange::Bottom))
            .shortcut(cmd_shift(res::str::menu_back_key()))
            .enabled(some),
        menu_separator(),
        menu_item(res::str::menu_delete())
            .action(model::delete_selection)
            .enabled(some),
    ]
}

fn menus() -> Vec<MenuEntry> {
    let file = vec![
        menu_item(res::str::menu_new())
            .action(model::new_doc)
            .shortcut(cmd(res::str::menu_new_key())),
        menu_item(res::str::menu_open())
            .action(model::open_doc_dialog)
            .shortcut(cmd(res::str::menu_open_key())),
        menu_separator(),
        menu_item(res::str::menu_export())
            .action(model::export_copy_dialog)
            .shortcut(cmd_shift(res::str::menu_export_key())),
    ];
    let mut view = vec![
        zoom_in_command().menu_item(),
        zoom_reset_command().menu_item(),
        zoom_out_command().menu_item(),
        menu_separator(),
        // One stable label ("Inspector"), not a Show/Hide flip: dayscript targets menu items
        // by catalog key, and a flipping label would break `menu: { key: menu_inspector }`.
        menu_item(res::str::menu_inspector())
            .action(inspector::toggle)
            .shortcut(cmd_alt(res::str::menu_inspector_key())),
    ];
    // The layers pane exists where the tree piece does (`Cap::Tree`, docs/tree.md), native
    // or composed; there is no menu item for a pane a target cannot show.
    if capability(Cap::Tree) != Support::Unsupported {
        view.push(
            menu_item(res::str::menu_layers())
                .action(inspector::layers_toggle)
                .shortcut(cmd_alt(res::str::menu_layers_key())),
        );
    }
    vec![
        sub_menu(res::str::menu_file(), file).bar_role(MenuBarRole::File),
        // Role-only Undo/Redo: the native standard commands, which on macOS/iOS resolve
        // through the responder chain to Day's NSUndoManager front: a focused text field
        // keeps its own typing undo, everything else reaches the document stack.
        sub_menu(
            res::str::menu_edit().format(),
            vec![
                menu_role(MenuRole::Undo),
                menu_role(MenuRole::Redo),
                menu_separator(),
                // Role items: the platform's Cut/Copy/Paste, the same menu items,
                // shortcuts, and responder precedence its text editing uses; shapes travel
                // as SVG through the edit bridge (docs/menus.md).
                menu_role(MenuRole::Cut),
                menu_role(MenuRole::Copy),
                menu_role(MenuRole::Paste),
                menu_role(MenuRole::SelectAll),
                menu_separator(),
                // Plain Delete, not a ⌘ combination: the key is the command everywhere this
                // app runs. It is spelled in code rather than through a localized `.key`
                // attribute like the letter shortcuts above, because a locale can pick a
                // better mnemonic letter for Group, but nobody relocates the Delete key.
                //
                // Backspace answers to the same accelerator: appkit and gtk both fold the two
                // names onto one key, and `canvas::canvas_key` accepts both where it handles
                // this itself.
                menu_item(res::str::menu_delete())
                    .action(model::delete_selection)
                    .shortcut(Shortcut::plain("Delete")),
            ],
        )
        .bar_role(MenuBarRole::Edit),
        sub_menu(res::str::menu_view(), view).bar_role(MenuBarRole::View),
        // Insert: the shape vocabulary, one item per kind, the same entries the toolbar's
        // pull-down and the mobile sheet serve, so a new shape appears in all three at once.
        sub_menu(res::str::menu_insert(), shape_menu_entries()),
        sub_menu(res::str::menu_arrange(), arrange_menu_entries()),
    ]
}

fn toolbar() -> Vec<ToolbarEntry> {
    let mut items = Vec::new();
    if capability(Cap::Tree) != Support::Unsupported {
        // Leading, like every sidebar toggle: the pane it shows is the leading pane.
        items.push(
            toolbar_toggle(
                day::id!(tb_layers),
                res::str::menu_layers(),
                inspector::layers_visible(),
            )
            .icon(Symbol::Sidebar)
            // Leading: it reveals the pane to its left.
            .placement(ToolbarPlacement::Navigation)
            .tooltip(res::str::menu_layers()),
        );
    }
    items.extend(vec![
        toolbar_button(day::id!(tb_group), res::str::menu_group())
            .icon(Symbol::Group)
            .tooltip(res::str::menu_group())
            .action(model::group_selection),
        toolbar_button(day::id!(tb_ungroup), res::str::menu_ungroup())
            .icon(Symbol::Ungroup)
            .tooltip(res::str::menu_ungroup())
            .action(model::ungroup_selection),
        // Insert: a pull-down of the shape vocabulary, each item drawn with the platform's
        // own glyph. Placing is a command, not a mode: the shape lands in the middle of the
        // visible canvas, selected and ready to style.
        toolbar_menu(
            day::id!(tb_shape),
            res::str::tool_shape(),
            shape_menu_entries(),
        )
        .icon(Symbol::Add)
        .tooltip(res::str::tool_shape()),
        toolbar_separator(day::id!(tb_sep_tools)),
        // The zoom group: out, actual size, in. The separator sets the trio off from its
        // neighbors.
        zoom_out_command().toolbar_item().id(day::id!(tb_zoom_out)),
        zoom_reset_command()
            .toolbar_item()
            .id(day::id!(tb_zoom_reset)),
        zoom_in_command().toolbar_item().id(day::id!(tb_zoom_in)),
        // The arrange pair and the inspector toggle trail the rest: `Secondary` is what packs
        // them to the far end of a desktop bar and folds them away first on a phone
        // (docs/toolbars.md), which is what the flexible space used to say by position.
        // The inspector is the exception where no menu bar backs the toolbar (the phones and
        // the web): there it is the one way to the properties panel, so it stays on the bar
        // (`Primary` folds last) rather than two taps away in the overflow.
        toolbar_button(day::id!(tb_forward), res::str::menu_forward())
            .icon(Symbol::Up)
            .placement(ToolbarPlacement::Secondary)
            .tooltip(res::str::menu_forward())
            .action(|| model::arrange_named(model::Arrange::Up)),
        toolbar_button(day::id!(tb_backward), res::str::menu_backward())
            .icon(Symbol::Down)
            .placement(ToolbarPlacement::Secondary)
            .tooltip(res::str::menu_backward())
            .action(|| model::arrange_named(model::Arrange::Down)),
        // Two-way: a menu/button toggle elsewhere re-checks this item through the signal.
        toolbar_toggle(
            day::id!(tb_inspector),
            res::str::menu_inspector(),
            inspector::visible(),
        )
        .icon(Symbol::Info)
        .placement(if capability(Cap::AppMenu) == Support::Native {
            ToolbarPlacement::Secondary
        } else {
            ToolbarPlacement::Primary
        })
        .tooltip(res::str::menu_inspector()),
    ]);
    // Settings: the desktops reach the shared surface (https://daybrite.dev/docs/settings)
    // from the App menu and the web from its emulated one, so only a phone, which has neither,
    // carries the command on its bar. It trails with the other secondary items, and `open()`
    // presents the surface as a cover: this window is a canvas, not a nav host, so no nav item
    // claims the `settings` route.
    if capability(Cap::AppMenu) == Support::Unsupported {
        items.push(
            toolbar_button(day::id!(tb_settings), day::tr("day-settings"))
                .icon(Symbol::Settings)
                .placement(ToolbarPlacement::Secondary)
                .tooltip(day::tr("day-settings"))
                .action(|| {
                    day_piece_settings::open();
                }),
        );
    }
    items
}

/// The shape vocabulary, served twice. Choosing one places it in the middle of the visible
/// canvas and selects it; placing is a command, not an armed-tool mode.
///
/// The window toolbar's pull-down, each item drawn with the platform's own glyph; on a phone the
/// same item rides the navigation bar or app bar (docs/toolbars.md), folding into its overflow
/// when the bar is narrow.
fn shape_menu_entries() -> Vec<MenuEntry> {
    vec![
        menu_item(res::str::tool_rect())
            .icon(Symbol::Rectangle)
            .action(|| canvas::place_centered(model::NodeKind::Rect)),
        menu_item(res::str::tool_oval())
            .icon(Symbol::Oval)
            .action(|| canvas::place_centered(model::NodeKind::Oval)),
        menu_item(res::str::tool_line())
            .icon(Symbol::Line)
            .action(|| canvas::place_centered(model::NodeKind::Line)),
        menu_item(res::str::tool_text())
            .icon(Symbol::Text)
            .action(|| canvas::place_centered(model::NodeKind::Text)),
        menu_item(res::str::tool_image())
            .id(day::id!(insert_image))
            .enabled(images::supported())
            .action(images::insert_dialog),
    ]
}

/// The selection's context menu. The canvas's right-click and the layers tree's rows serve
/// the same commands, built at summon time so they describe the selection as it stands:
/// Delete; whichever grouping ops apply (Group needs two nodes, Ungroup a group, Remove
/// from Group a grouped member); the z-order moves; Cut/Copy through the edit bridge (the
/// same clipboard path the Edit menu takes) and Duplicate (clipboard untouched).
pub(crate) fn selection_context_menu() -> Vec<MenuEntry> {
    let sel = model::selection().get_untracked();
    if sel.is_empty() {
        return Vec::new();
    }
    let mut items = vec![
        menu_item(res::str::menu_delete()).action(model::delete_selection),
        menu_separator(),
    ];
    let before_grouping = items.len();
    if sel.len() >= 2 {
        items.push(menu_item(res::str::menu_group()).action(model::group_selection));
    }
    if sel
        .iter()
        .any(|id| model::node_kind(*id) == Some(model::NodeKind::Group))
    {
        items.push(menu_item(res::str::menu_ungroup()).action(model::ungroup_selection));
    }
    if sel.iter().any(|id| model::parent_of(*id).is_some()) {
        items.push(
            menu_item(res::str::ctx_remove_group()).action(model::remove_selection_from_group),
        );
    }
    if items.len() > before_grouping {
        items.push(menu_separator());
    }
    items.extend([
        menu_item(res::str::ctx_move_up()).action(|| model::arrange_named(model::Arrange::Up)),
        menu_item(res::str::ctx_move_down()).action(|| model::arrange_named(model::Arrange::Down)),
        menu_item(res::str::ctx_move_front()).action(|| model::arrange_named(model::Arrange::Top)),
        menu_item(res::str::ctx_move_back())
            .action(|| model::arrange_named(model::Arrange::Bottom)),
        menu_separator(),
        menu_item(res::str::ctx_cut()).action(|| {
            let _ = day::invoke_edit(day::EditOp::Cut);
        }),
        menu_item(res::str::ctx_copy()).action(|| {
            let _ = day::invoke_edit(day::EditOp::Copy);
        }),
        menu_item(res::str::ctx_duplicate()).action(model::duplicate_selection),
    ]);
    items
}

fn status_row() -> impl Piece {
    row((
        label(move || {
            let store = model::nodes();
            store.with(|_| {});
            let n = store.with_untracked(|k| k.items().len());
            res::str::status_count(n as i64).format()
        })
        .tabular()
        .id(day::id!(sk_count)),
        // The single selection's frame, integer-rounded: what walkthrough drags assert on.
        label(move || {
            let sel = model::selection().get();
            model::nodes().with(|_| {});
            match sel.as_slice() {
                [only] => match model::node_bounds(*only) {
                    Some((x, y, w, h)) => format!(
                        "{},{} {}x{}",
                        x.round() as i64,
                        y.round() as i64,
                        w.round() as i64,
                        h.round() as i64
                    ),
                    None => String::new(),
                },
                _ => String::new(),
            }
        })
        .tabular()
        .id(day::id!(sk_frame)),
        label(move || {
            res::str::status_zoom((canvas::zoom().get() * 100.0).round() as i64).format()
        })
        .tabular()
        .id(day::id!(sk_zoom)),
        spacer(),
        label(move || {
            model::doc_rev().track();
            model::doc_name()
        })
        .font(Font::Footnote)
        .id(day::id!(sk_doc)),
    ))
    .spacing(Space::L)
    // Phone widths cannot hold every readout on one line (the tool row's rule): wrap, or
    // the trailing doc name squeezes into a one-character-wide column.
    .fit(RowFit::Wrap { run_spacing: 4.0 })
    .padding(Insets::symmetric(12.0, 6.0))
}

fn editor() -> impl Piece {
    // The commands act on the canvas, so they are declared on it (docs/toolbars.md): they ride
    // whichever chrome the editor has, and a phone that pushes an inspector page over the canvas
    // gets that page's own commands instead of these.
    column((canvas::editor_canvas(), status_row())).toolbar(toolbar)
}

/// Everything one window owns (docs/state.md): what it is looking at and how, but never what
/// the drawing is.
///
/// The document is app-wide (`model::doc()`): two windows edit the same drawing, the way two
/// windows on one Illustrator file do. Everything here is that window's own view of it: its
/// zoom and pan, its selection, its rubber band, which inspector tab is showing, which groups
/// are disclosed. None of it is persisted and none of it is undoable.
///
/// `Clone`, not `Copy`, because the transient gesture cells ride along behind an `Rc`; every
/// accessor below calls `scene()` fresh, so no call site had to change.
#[derive(Clone)]
pub(crate) struct Scene {
    // --- the view transform + gesture state (canvas.rs) ---
    pub(crate) zoom: Signal<f64>,
    pub(crate) pan: Signal<Point>,
    pub(crate) band: Signal<Option<(f64, f64, f64, f64)>>,
    /// The alignment guides the drag in flight is snapped to (canvas.rs); empty between drags.
    pub(crate) guides: Signal<Vec<canvas::Guide>>,
    pub(crate) canvas_focused: Signal<bool>,
    // --- the editor's own selection + disclosure (model.rs) ---
    pub(crate) selection: Signal<Vec<u64>>,
    pub(crate) open_groups: Signal<std::collections::HashSet<u64>>,
    // --- the inspector (inspector.rs) ---
    pub(crate) inspector_visible: Signal<bool>,
    pub(crate) layers_visible: Signal<bool>,
    pub(crate) active_tab: Signal<inspector::Tab>,
    pub(crate) refresh: Signal<u64>,
    /// Transient, non-reactive scratch: the last laid-out canvas size and the click/tap
    /// debounce the double-click drill needs, plus the paste ladder.
    pub(crate) cells: std::rc::Rc<SceneCells>,
}

#[derive(Default)]
pub(crate) struct SceneCells {
    /// The canvas's last laid-out size, the anchor for menu/toolbar zoom (its center).
    pub(crate) viewport: std::cell::Cell<(f64, f64)>,
    pub(crate) last_click:
        std::cell::Cell<Option<(std::time::Instant, f64, f64, canvas::ClickSource)>>,
    pub(crate) last_tap: std::cell::Cell<Option<(std::time::Instant, f64, f64)>>,
    /// Repeated pastes of the same payload step further (+16 each), so copies never stack
    /// invisibly; a new copy resets the ladder.
    pub(crate) paste_step: std::cell::RefCell<(u64, f64)>,
}

impl Ambient for Scene {
    fn create() -> Self {
        Scene {
            zoom: Signal::new(1.0),
            pan: Signal::new(Point::ZERO),
            band: Signal::new(None),
            guides: Signal::new(Vec::new()),
            canvas_focused: Signal::new(true),
            selection: Signal::new(Vec::new()),
            open_groups: Signal::new(std::collections::HashSet::new()),
            inspector_visible: Signal::new(true),
            // The class is already seeded when the root builds (docs/size-classes.md).
            layers_visible: Signal::new(
                day::size_class()
                    .map(|s| s.width != day::prelude::WidthClass::Compact)
                    .unwrap_or(true),
            ),
            active_tab: Signal::new(inspector::Tab::Canvas),
            refresh: Signal::new(0),
            cells: std::rc::Rc::new(SceneCells {
                viewport: std::cell::Cell::new((800.0, 600.0)),
                ..Default::default()
            }),
        }
    }
}

/// This window's `Scene`: the ambient one while a piece builds, the focused window's when a
/// command runs later from a handler that belongs to no scope (docs/state.md).
pub(crate) fn scene() -> Scene {
    Scene::try_ambient()
        .or_else(Scene::focused)
        .expect("no window is open, so there is no Scene to act on")
}

pub fn root() -> impl Piece {
    res::locales::install();
    day::prefs::install_nav_store();
    // Settings and About (https://daybrite.dev/docs/settings): appearance and language from
    // Day's catalog, the Editing group below, the app's name and version from Day.toml. On a
    // desktop this adds the App menu's Settings… and About items and their windows; a phone
    // reaches the same pages from the bar's Settings item (`toolbar`), as a cover.
    Preferences::new()
        .about(About::from_manifest().link(day::tr("day-about-website"), "https://daybrite.dev"))
        .group(general_group(res::locales::ALL))
        .group(
            Group::new("editing", res::str::pref_editing())
                .icon(Symbol::Edit)
                .row(rows::toggle(&SNAP, res::str::pref_snap()).id(day::id!(pref_snap))),
        )
        .install();
    // File ▸ New Window (docs/windows.md): the same shell again, which is why the view state
    // (zoom, pan, selection, the inspector) lives on a `Scene` rather than in globals. The
    // document stays shared, so two windows show one drawing from two vantage points.
    day::register_new_window(window_shell);

    window_shell()
}

/// One window's UI: the first window's, and every File ▸ New Window's.
fn window_shell() -> impl Piece {
    Scene::scoped(|_scene| {
        // Installed from inside the scope, because both read what this window is looking at:
        // the toolbar per window (docs/toolbars.md), and the menu bar's enabled/checked states
        // through `scene()`. The menu bar is one bar for the app, so it installs once.
        // The menu bar and the clipboard bridge are one per app, but both read what the front
        // window is looking at (through `scene()`), so they install from inside a window's
        // scope, once, in the order `root()` used to run them.
        static APP_ONCE: std::sync::Once = std::sync::Once::new();
        APP_ONCE.call_once(|| app_menu(menus));
        // Opening the (or a) document wires its undo stack to the platform, synchronously on
        // every target; the web's day-sql worker is up before app code runs. Idempotent, so each
        // window finds the document already open.
        let _ = model::doc();
        // The platform's Cut/Copy/Paste reach the shape editor as SVG (docs/menus.md); a focused
        // text field keeps its own clipboard behavior ahead of these. After `doc()`, exactly as
        // `root()` ordered it: the edit-state bind reads a selection the document owns.
        static EDIT_ONCE: std::sync::Once = std::sync::Once::new();
        EDIT_ONCE.call_once(|| {
            clipboard::install();
        });
        // The selection drives the inspector's tab, per window: any change lands it on the tab
        // that talks about the current state. Inside the scope, so the bind reads this window's
        // selection.
        day::reactive::bind(
            || model::selection().get(),
            |sel: &Vec<u64>| inspector::retarget(!sel.is_empty()),
        );
        // Rebuild the whole editor when a different document becomes current: the editor is
        // keyed on the document rev, and each build is fresh in its own scope. The inspector
        // wraps the swap, so the pane (and its visibility) survives a document switch. On a
        // compact window the panel is a half sheet, so the shapes it describes stay on show
        // while it is open.
        let editor_with_inspector = inspector(
            inspector::visible(),
            keyed(move || Some(model::doc_rev().get()), |_| editor()),
            inspector::panel,
        )
        .half_sheet()
        .sheet_done(res::str::insp_done());
        // The layers pane wraps the whole editor on its leading side wherever the tree piece
        // exists, native (macOS/GTK/iOS) or composed (web, docs/tree.md M2); a target that
        // answers `Unsupported` keeps exactly the window it had until the tree lands there.
        if capability(Cap::Tree) != Support::Unsupported {
            inspector(
                inspector::layers_visible(),
                editor_with_inspector,
                inspector::layers_panel,
            )
            .width(220.0)
            .edge(PaneEdge::Leading)
            .any()
        } else {
            editor_with_inspector.any()
        }
    })
}

fn zoom_in_command() -> Command {
    Command::new(day::id!(zoom_in), res::str::menu_zoom_in(), || {
        canvas::zoom_step(1.25)
    })
    .icon(Symbol::ZoomIn)
    .shortcut(cmd(res::str::menu_zoom_in_key()))
}

fn zoom_out_command() -> Command {
    Command::new(day::id!(zoom_out), res::str::menu_zoom_out(), || {
        canvas::zoom_step(0.8)
    })
    .icon(Symbol::ZoomOut)
    .shortcut(cmd(res::str::menu_zoom_out_key()))
}

fn zoom_reset_command() -> Command {
    Command::new(
        day::id!(zoom_reset),
        res::str::menu_zoom_reset(),
        canvas::zoom_reset,
    )
    .icon(Symbol::ZoomReset)
    .shortcut(cmd(res::str::menu_zoom_reset_key()))
}
