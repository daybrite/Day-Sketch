//! Day Sketch — a vector drawing editor built on [Day](https://daybrite.dev), and the stress
//! test the day-model/day-persistence design was drafted against: a drawing is a SQLite file,
//! the scene is one observable table, drags edit it live through preview sessions, and every
//! operation — placement, move, resize, group, arrange — is one undoable turn, fronted by the
//! platform's own undo system where it has one.

use day::prelude::*;

mod canvas;
mod inspector;
mod model;

/// Typed constants for the files under `resource/`, generated at build time by `day-build`.
pub mod res {
    include!(concat!(env!("OUT_DIR"), "/day_resources.rs"));
}

const THEME_KEY: &str = "app.theme";
const LOCALE_KEY: &str = "app.locale";

fn settings_body() -> impl Piece {
    form((day_piece_settings::settings_sections(
        THEME_KEY,
        LOCALE_KEY,
        res::locales::ALL,
    ),))
}

/// ⌘/Ctrl + the LOCALIZED key: the letter comes from the command's `.key` attribute in the
/// catalog (docs/localization.md), so a locale may override it while every other locale
/// inherits the default's — the modifier scheme stays semantic, here in code.
fn cmd(key: day::LocalizedText) -> Shortcut {
    Shortcut {
        key: key.format(),
        primary: true,
        ..Default::default()
    }
}

fn cmd_shift(key: day::LocalizedText) -> Shortcut {
    Shortcut {
        key: key.format(),
        primary: true,
        shift: true,
        ..Default::default()
    }
}

fn cmd_alt(key: day::LocalizedText) -> Shortcut {
    Shortcut {
        key: key.format(),
        primary: true,
        alt: true,
        ..Default::default()
    }
}

/// The arrange commands — one list, served three ways: the Arrange menu, the canvas context
/// menu, and (with icons) the window toolbar.
pub(crate) fn arrange_menu_entries() -> Vec<MenuEntry> {
    arrange_entries(true)
}

/// `track` = read the selection reactively (the menu-bar builder re-runs on change). The
/// canvas context menu is lowered ONCE at build, so it keeps every item enabled and lets the
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
        menu_item(res::str::menu_group().format())
            .action(model::group_selection)
            .shortcut(cmd(res::str::menu_group_key()))
            .enabled(two),
        menu_item(res::str::menu_ungroup().format())
            .action(model::ungroup_selection)
            .shortcut(cmd_shift(res::str::menu_ungroup_key()))
            .enabled(some),
        menu_separator(),
        menu_item(res::str::menu_forward().format())
            .action(|| model::arrange_named(model::Arrange::Up))
            .shortcut(cmd(res::str::menu_forward_key()))
            .enabled(some),
        menu_item(res::str::menu_backward().format())
            .action(|| model::arrange_named(model::Arrange::Down))
            .shortcut(cmd(res::str::menu_backward_key()))
            .enabled(some),
        menu_item(res::str::menu_front().format())
            .action(|| model::arrange_named(model::Arrange::Top))
            .shortcut(cmd_shift(res::str::menu_front_key()))
            .enabled(some),
        menu_item(res::str::menu_back().format())
            .action(|| model::arrange_named(model::Arrange::Bottom))
            .shortcut(cmd_shift(res::str::menu_back_key()))
            .enabled(some),
        menu_separator(),
        menu_item(res::str::menu_delete().format())
            .action(model::delete_selection)
            .enabled(some),
    ]
}

/// The canvas right-click/long-press menu: the standard clipboard trio (role items — the
/// platform's own commands), then the arrange set. The Arrange BAR menu deliberately carries
/// only the arrange set; Cut/Copy/Paste live in the standard Edit menu.
pub(crate) fn context_menu_entries() -> Vec<MenuEntry> {
    let mut items = vec![
        menu_role(MenuRole::Cut),
        menu_role(MenuRole::Copy),
        menu_role(MenuRole::Paste),
        menu_separator(),
    ];
    items.extend(arrange_entries(false));
    items
}

fn menus() -> Vec<MenuEntry> {
    let file = vec![
        menu_item(res::str::menu_new().format())
            .action(model::new_doc)
            .shortcut(cmd(res::str::menu_new_key())),
        menu_item(res::str::menu_open().format())
            .action(model::open_doc_dialog)
            .shortcut(cmd(res::str::menu_open_key())),
        menu_separator(),
        menu_item(res::str::menu_export().format())
            .action(model::export_copy_dialog)
            .shortcut(cmd_shift(res::str::menu_export_key())),
    ];
    let view = vec![
        menu_item(res::str::menu_zoom_in().format())
            .action(|| canvas::zoom_step(1.25))
            .shortcut(cmd(res::str::menu_zoom_in_key())),
        menu_item(res::str::menu_zoom_reset().format())
            .action(canvas::zoom_reset)
            .shortcut(cmd(res::str::menu_zoom_reset_key())),
        menu_item(res::str::menu_zoom_out().format())
            .action(|| canvas::zoom_step(0.8))
            .shortcut(cmd(res::str::menu_zoom_out_key())),
        menu_separator(),
        // One stable label ("Inspector"), not a Show/Hide flip: dayscript targets menu items
        // by catalog key, and a flipping label would break `menu: { key: menu_inspector }`.
        menu_item(res::str::menu_inspector().format())
            .action(inspector::toggle)
            .shortcut(cmd_alt(res::str::menu_inspector_key())),
    ];
    vec![
        sub_menu(res::str::menu_file().format(), file).bar_role(MenuBarRole::File),
        // Role-only Undo/Redo: the native standard commands, which on macOS/iOS resolve
        // through the responder chain to Day's NSUndoManager front — a focused text field
        // keeps its own typing undo, everything else reaches the document stack.
        sub_menu(
            res::str::menu_edit().format(),
            vec![
                menu_role(MenuRole::Undo),
                menu_role(MenuRole::Redo),
                menu_separator(),
                // Role items: the platform's own Cut/Copy/Paste — the same menu items,
                // shortcuts, and responder precedence its text editing uses; shapes travel
                // as SVG through the edit bridge (docs/menus.md).
                menu_role(MenuRole::Cut),
                menu_role(MenuRole::Copy),
                menu_role(MenuRole::Paste),
                menu_role(MenuRole::SelectAll),
                menu_separator(),
                menu_item(res::str::menu_delete().format()).action(model::delete_selection),
            ],
        )
        .bar_role(MenuBarRole::Edit),
        sub_menu(res::str::menu_view().format(), view).bar_role(MenuBarRole::View),
        // Insert: the shape vocabulary, one item per kind — the same entries the toolbar's
        // pull-down and the mobile sheet serve, so a new shape appears in all three at once.
        sub_menu(res::str::menu_insert().format(), shape_menu_entries()),
        sub_menu(res::str::menu_arrange().format(), arrange_menu_entries()),
    ]
}

fn toolbar() -> Vec<ToolbarEntry> {
    vec![
        toolbar_button("tb-group", res::str::menu_group())
            .icon(Symbol::Add)
            .tooltip(res::str::menu_group())
            .action(model::group_selection),
        toolbar_button("tb-ungroup", res::str::menu_ungroup())
            .icon(Symbol::Remove)
            .tooltip(res::str::menu_ungroup())
            .action(model::ungroup_selection),
        // Insert: a pull-down of the shape vocabulary, each item drawn with the platform's
        // own glyph. Placing is a command, not a mode — the shape lands in the middle of the
        // visible canvas, selected and ready to style.
        toolbar_menu("tb-shape", res::str::tool_shape(), shape_menu_entries())
            .icon(Symbol::Add)
            .tooltip(res::str::tool_shape()),
        toolbar_separator(),
        // The zoom group: out, actual size, in — the separator sets the trio off from its
        // neighbors, and the reset button carries text (there is no glyph for "100%").
        toolbar_button("tb-zoom-out", res::str::menu_zoom_out())
            .icon(Symbol::ZoomOut)
            .tooltip(res::str::menu_zoom_out())
            .action(|| canvas::zoom_step(0.8)),
        toolbar_button("tb-zoom-reset", res::str::menu_zoom_reset())
            .tooltip(res::str::menu_zoom_reset())
            .action(canvas::zoom_reset),
        toolbar_button("tb-zoom-in", res::str::menu_zoom_in())
            .icon(Symbol::ZoomIn)
            .tooltip(res::str::menu_zoom_in())
            .action(|| canvas::zoom_step(1.25)),
        toolbar_flexible_space(),
        toolbar_button("tb-forward", res::str::menu_forward())
            .icon(Symbol::Up)
            .tooltip(res::str::menu_forward())
            .action(|| model::arrange_named(model::Arrange::Up)),
        toolbar_button("tb-backward", res::str::menu_backward())
            .icon(Symbol::Down)
            .tooltip(res::str::menu_backward())
            .action(|| model::arrange_named(model::Arrange::Down)),
        // Two-way: a menu/button toggle elsewhere re-checks this item through the signal.
        toolbar_toggle(
            "tb-inspector",
            res::str::menu_inspector(),
            inspector::visible(),
        )
        .icon(Symbol::Info)
        .tooltip(res::str::menu_inspector()),
    ]
}

/// The shape vocabulary, served twice. Choosing one places it in the middle of the visible
/// canvas and selects it — placing is a command, not an armed-tool mode.
///
/// The window toolbar's pull-down is the desktop form, each item drawn with the platform's own
/// glyph. Phones have no window toolbar (`Cap::Toolbar` is Unsupported there), so the tool row
/// carries the same two choices as a native action sheet — the mobile idiom for exactly this,
/// and docs/toolbars.md's own advice for a command that has nowhere on the chrome to live.
fn shape_menu_entries() -> Vec<MenuEntry> {
    vec![
        menu_item(res::str::tool_rect().format())
            .icon(Symbol::Rectangle)
            .action(|| canvas::place_centered(model::NodeKind::Rect)),
        menu_item(res::str::tool_oval().format())
            .icon(Symbol::Oval)
            .action(|| canvas::place_centered(model::NodeKind::Oval)),
        menu_item(res::str::tool_line().format())
            .icon(Symbol::Line)
            .action(|| canvas::place_centered(model::NodeKind::Line)),
    ]
}

/// The tool row's shape button: the same two choices as an action sheet, so every target can
/// place a shape (and one id, `tool-shape`, drives it in the walkthrough everywhere).
fn choose_shape() {
    day::task(async {
        let picked = Alert::<model::NodeKind>::new(res::str::tool_shape())
            .sheet()
            .button(res::str::tool_rect(), model::NodeKind::Rect)
            .button(res::str::tool_oval(), model::NodeKind::Oval)
            .button(res::str::tool_line(), model::NodeKind::Line)
            .cancel(res::str::menu_cancel())
            .present()
            .await;
        if let Some(kind) = picked {
            canvas::place_centered(kind);
        }
    });
}

fn tool_row() -> impl Piece {
    let stack = model::undo_stack();
    let (u1, u2, r1, r2) = (stack.clone(), stack.clone(), stack.clone(), stack);
    row((
        // The same shape choices as the window toolbar's item, for the targets that have no
        // window toolbar (`Cap::Toolbar` is Unsupported on mobile and the web).
        button(res::str::tool_shape())
            .bordered()
            .action(choose_shape)
            .id("tool-shape"),
        spacer(),
        button(res::str::menu_undo())
            .bordered()
            .action(move || {
                u1.undo();
            })
            .enabled(move || u2.can_undo().get())
            .id("sk-undo"),
        button(res::str::menu_redo())
            .bordered()
            .action(move || {
                r1.redo();
            })
            .enabled(move || r2.can_redo().get())
            .id("sk-redo"),
        // The inspector's in-content toggle: the mobile targets have no window toolbar
        // (`Cap::Toolbar` is Unsupported there), so the tool row is where the affordance
        // lives — and one id everywhere keeps the walkthrough portable.
        button(res::str::tool_inspector())
            .bordered()
            .action(inspector::toggle)
            .id("tool-inspector"),
    ))
    .spacing(8.0)
    // Phone widths cannot hold the whole strip on one line; wrap instead of clipping.
    .fit(RowFit::Wrap { run_spacing: 6.0 })
    .padding(Insets::symmetric(12.0, 8.0))
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
        .id("sk-count"),
        // The single selection's frame, integer-rounded — what walkthrough drags assert on.
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
        .id("sk-frame"),
        label(move || {
            res::str::status_zoom((canvas::zoom().get() * 100.0).round() as i64).format()
        })
        .tabular()
        .id("sk-zoom"),
        spacer(),
        label(move || {
            model::doc_rev().track();
            model::doc_name()
        })
        .font(Font::Footnote)
        .id("sk-doc"),
    ))
    .spacing(16.0)
    // Phone widths cannot hold every readout on one line (the tool row's rule): wrap, or
    // the trailing doc name squeezes into a one-character-wide column.
    .fit(RowFit::Wrap { run_spacing: 4.0 })
    .padding(Insets::symmetric(12.0, 6.0))
}

fn editor() -> impl Piece {
    column((tool_row(), canvas::editor_canvas(), status_row()))
}

pub fn root() -> impl Piece {
    res::locales::install();
    day_piece_settings::apply_startup(THEME_KEY, LOCALE_KEY);
    day::prefs::install_nav_store();
    day::register_preferences(settings_body);
    app_menu_reactive(menus);
    toolbar_reactive(toolbar);
    // Opening the (or a) document wires its undo stack to the platform — synchronously on
    // every target; the web's day-sql worker is up before app code runs.
    let _ = model::doc();
    // The platform's Cut/Copy/Paste reach the shape editor as SVG (docs/menus.md); a focused
    // text field keeps its own clipboard behavior ahead of these.
    day::install_edit_commands(
        || !model::selection().get().is_empty(),
        model::copy_selection_svg,
        model::cut_selection_svg,
        model::paste_clipboard,
        model::select_all,
    );
    // The selection drives the inspector's tab: any change lands it on the tab that talks
    // about the current state — Selected while something is, Canvas when nothing is. The
    // bind fires on every selection change (taps, paste, undo/redo restoration), and only
    // on change, so a manual tab choice stands until the selection next moves.
    day::reactive::bind(
        || model::selection().get(),
        |sel: &Vec<u64>| inspector::retarget(!sel.is_empty()),
    );
    // Rebuild the whole editor when a DIFFERENT document becomes current: the rev alternates
    // the arms, and each arm builds fresh in its own scope. The inspector wraps the swap, so
    // the pane (and its visibility) survives a document switch.
    inspector(
        inspector::visible(),
        when(move || model::doc_rev().get().is_multiple_of(2), editor).otherwise(editor),
        inspector::panel,
    )
    .sheet_done(res::str::insp_done())
}

// The mobile / embedded entry point. Expands to the export each platform's shell binds against —
// and to nothing at all on a plain cargo desktop build, where src/main.rs is the entry instead.
day::day_main!("Day Sketch", root);
