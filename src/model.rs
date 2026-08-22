//! The scene, the document, and every undoable operation.
//!
//! A drawing IS a SQLite file: each document opens as its own `ModelContainer` with its own
//! change log and its own undo stack, autosaved at every turn's end — Open is open, Save is
//! autosave, and Export a Copy is a consistent `backup_to` snapshot delivered through the
//! platform's save dialog. The whole scene is ONE table (https://daybrite.dev/docs/persistence):
//! a heterogeneous tree of rows whose `kind` says what the geometry columns mean — a `Group`
//! row is just a node whose children point at it and whose own frame is derived, never stored.
//!
//! Every operation in this file lands inside one event turn, and a TURN is the unit of undo —
//! so "group five shapes" or "drag a corner" is one step backward, with no bookkeeping here.
//! The web build opens the same container through the day-sql worker (docs/persistence.md):
//! SQLite holds the file on real OPFS, every statement crosses a synchronous channel, and a
//! commit that returned has been fsynced — the same open/flush/undo shape as the desktop,
//! with the "path" serving as the document's name in the origin's pool.

use day::model::{Op, UndoStack};
use day::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

pub(crate) const MIN_SIZE: f64 = 8.0;
const DEFAULT_W: f64 = 96.0;
const DEFAULT_H: f64 = 64.0;
const PALETTE: [&str; 6] = [
    "#3B82F6", "#EF4444", "#10B981", "#F59E0B", "#8B5CF6", "#EC4899",
];

// ---------------------------------------------------------------------------
// The scene model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum NodeKind {
    #[default]
    Rect,
    Oval,
    Group,
}

/// TEXT in the file (`rect`/`oval`/`group`) — readable by any SQLite tool, stable for SVG later.
impl day::persistence::ColumnValue for NodeKind {
    const SQL_TYPE: day::persistence::SqlType = day::persistence::SqlType::Text;
    fn to_sqlite_value(&self) -> day::persistence::Value {
        day::persistence::Value::Text(
            match self {
                NodeKind::Rect => "rect",
                NodeKind::Oval => "oval",
                NodeKind::Group => "group",
            }
            .into(),
        )
    }
    fn from_sqlite_value(v: day::persistence::Value) -> Result<Self, day::persistence::DbError> {
        match v.as_text()? {
            "rect" => Ok(NodeKind::Rect),
            "oval" => Ok(NodeKind::Oval),
            "group" => Ok(NodeKind::Group),
            other => Err(day::persistence::DbError::new(
                day::persistence::DbErrorKind::Decode,
                format!("not a node kind: {other:?}"),
            )),
        }
    }
}

#[derive(Clone, Default, PartialEq, Model)]
#[model(table = "nodes", index("parent", "z"))]
pub(crate) struct Node {
    #[model(id)]
    pub id: u64,
    /// The tree: a top-level node's parent is NULL; a group's children point at it.
    pub parent: Option<u64>,
    /// Sibling order, bottom to top. Fractional: moving a layer writes ONE row.
    pub z: f64,
    pub kind: NodeKind,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// `#RRGGBB` — the color well's currency, and SVG's.
    pub fill: String,
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

pub(crate) struct Doc {
    pub store: Store<Keyed<Node>>,
    pub container: Option<day::persistence::ModelContainer>,
    pub stack: UndoStack,
    pub path: Option<PathBuf>,
}

thread_local! {
    static DOC: RefCell<Option<Rc<Doc>>> = const { RefCell::new(None) };
}

/// Bumped whenever a DIFFERENT document becomes current; the editor rebuilds off it.
pub(crate) fn doc_rev() -> Signal<u64> {
    thread_local! {
        static REV: Signal<u64> = Signal::global(0);
    }
    REV.with(|s| *s)
}

pub(crate) fn doc() -> Rc<Doc> {
    let (doc, fresh) = DOC.with(|d| {
        let mut slot = d.borrow_mut();
        match slot.as_ref() {
            Some(doc) => (doc.clone(), false),
            None => {
                let doc = Rc::new(open_default());
                *slot = Some(doc.clone());
                (doc, true)
            }
        }
    });
    // The BOOT document must reach the platform exactly like a New/Open one: without this,
    // the undo bridge stays uninstalled until the first File ▸ New and the stock Edit ▸
    // Undo/Redo sit dead on a fresh launch (the borrow is released above, so the wiring's
    // own doc() reads cannot re-enter it).
    if fresh {
        wire_undo(&doc);
    }
    doc
}

/// Selection rides the history transiently (docs/model.md "Transient UI state"): every
/// undo/redo lands on the selection as it stood when that unit SEALED — so "select A, move
/// it, select B, move it, undo" lands on A, and the switch to B (a between-units change) is
/// nowhere. In-memory only; the base snapshot, restored when the whole history unwinds, is
/// taken here — callers wire a document whose selection is already the fresh one.
fn wire_selection_context(stack: &UndoStack) {
    stack.set_transient_context(
        || Rc::new(selection().get_untracked()),
        |ctx| {
            if let Some(sel) = ctx.downcast_ref::<Vec<u64>>() {
                selection().set(sel.clone());
            }
        },
    );
}

/// Wire a document's undo stack to the platform: the bridge (stock menu items, ⌘Z, shake)
/// plus the app's undo-unit labels. Every path a document becomes CURRENT through runs this —
/// the boot default above, and [`install_doc`] for New/Open.
fn wire_undo(doc: &Doc) {
    day::install_undo(&doc.stack);
    wire_selection_context(&doc.stack);
    doc.stack
        .set_label_resolver(|label: &'static str| -> String {
            match label {
                "add-rect" => crate::res::str::undo_add_rect().format(),
                "add-oval" => crate::res::str::undo_add_oval().format(),
                "move" => crate::res::str::undo_move().format(),
                "resize" => crate::res::str::undo_resize().format(),
                "group" => crate::res::str::undo_group().format(),
                "ungroup" => crate::res::str::undo_ungroup().format(),
                "arrange" => crate::res::str::undo_arrange().format(),
                "delete" => crate::res::str::undo_delete().format(),
                "cut" => crate::res::str::undo_cut().format(),
                "paste" => crate::res::str::undo_paste().format(),
                other => other.to_string(),
            }
        });
}

pub(crate) fn nodes() -> Store<Keyed<Node>> {
    doc().store
}

pub(crate) fn undo_stack() -> UndoStack {
    doc().stack.clone()
}

/// The document's display name — the file stem, or "Untitled" in memory (web).
pub(crate) fn doc_name() -> String {
    doc()
        .path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::res::str::doc_untitled().format())
}

fn install_doc(doc: Doc) {
    let doc = Rc::new(doc);
    // Clear the selection BEFORE wiring: the context hook's base snapshot must be the fresh
    // document's empty selection, not whatever the outgoing document had selected.
    selection().set(Vec::new());
    wire_undo(&doc);
    DOC.with(|d| *d.borrow_mut() = Some(doc));
    doc_rev().update(|r| *r += 1);
}

#[cfg(not(target_arch = "wasm32"))]
fn open_default() -> Doc {
    let remembered = day::prefs::get(LAST_DOC_KEY)
        .map(PathBuf::from)
        .filter(|p| p.exists());
    match remembered {
        Some(p) => open_file_doc(p),
        None => match day::persistence::Sqlite::app_data(DEFAULT_FILE) {
            Ok(driver) => doc_from_driver(traced(driver), default_path()),
            Err(_) => memory_doc(),
        },
    }
}

/// The web default: the remembered (or default) drawing out of the origin's OPFS pool,
/// opened synchronously — the day-sql worker is up before app code runs. No channel (a host
/// without cross-origin isolation) falls back to an in-memory scene: a working editor, just
/// not persistent.
#[cfg(target_arch = "wasm32")]
fn open_default() -> Doc {
    let storage = day::persistence::Sqlite::web_storage().ok();
    let remembered =
        day::prefs::get(LAST_DOC_KEY).filter(|n| storage.as_ref().is_some_and(|s| s.exists(n)));
    match (remembered, storage) {
        (Some(name), _) => open_file_doc(PathBuf::from(name)),
        (None, Some(_)) => open_file_doc(PathBuf::from(DEFAULT_FILE)),
        (None, None) => memory_doc(),
    }
}

const DEFAULT_FILE: &str = "sketch-default.daysketch";
const LAST_DOC_KEY: &str = "sketch.last-doc";

#[cfg(not(target_arch = "wasm32"))]
fn default_path() -> Option<PathBuf> {
    day::persistence::Sqlite::app_data_dir()
        .ok()
        .map(|d| d.join(DEFAULT_FILE))
}

/// Statement logging in debug builds: every SQL the engine executes for this document —
/// migrations, autosave flushes, undo replays, live queries — through the engine's own trace
/// (docs/persistence.md), at `trace!` because it is a per-statement firehose (docs/logging.md).
/// `DAY_LOG=trace` shows it; anything less hides it, which is the point of a level. The
/// `cfg!(debug_assertions)` guard stays: a release build should not pay to format SQL it will
/// then discard.
fn traced(driver: day::persistence::Sqlite) -> day::persistence::Sqlite {
    if cfg!(debug_assertions) {
        driver.trace_sql(|sql| trace!("sql: {sql}"))
    } else {
        driver
    }
}

fn doc_from_driver(driver: day::persistence::Sqlite, path: Option<PathBuf>) -> Doc {
    match day::persistence::ModelContainer::open(driver, day::persistence::schema![Node]) {
        Ok(container) => {
            let store = container.store::<Node>();
            let stack = container.undo(1000);
            if let Some(p) = &path {
                day::prefs::set(LAST_DOC_KEY, &p.to_string_lossy());
            }
            Doc {
                store,
                container: Some(container),
                stack,
                path,
            }
        }
        // A file that will not open (corrupt, wrong schema) must not take the app down:
        // fall back to an in-memory scene and surface nothing worse than an empty canvas.
        Err(_) => memory_doc(),
    }
}

fn open_file_doc(path: PathBuf) -> Doc {
    doc_from_driver(traced(day::persistence::Sqlite::at(&path)), Some(path))
}

fn memory_doc() -> Doc {
    let store = Store::new(Keyed::new(Vec::new()));
    let stack = UndoStack::new(1000);
    stack.watch(store);
    Doc {
        store,
        container: None,
        stack,
        path: None,
    }
}

/// File ▸ New: a fresh drawing beside the default one, numbered, opened immediately.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn new_doc() {
    let Ok(dir) = day::persistence::Sqlite::app_data_dir() else {
        install_doc(memory_doc());
        return;
    };
    let mut n = 1;
    let path = loop {
        let candidate = dir.join(format!("Drawing {n}.daysketch"));
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
    };
    install_doc(open_file_doc(path));
}

/// File ▸ New on the web: the same numbering over the origin's OPFS pool, synchronously.
#[cfg(target_arch = "wasm32")]
pub(crate) fn new_doc() {
    let Ok(storage) = day::persistence::Sqlite::web_storage() else {
        install_doc(memory_doc());
        return;
    };
    let mut n = 1;
    let name = loop {
        let candidate = format!("Drawing {n}.daysketch");
        if !storage.exists(&candidate) {
            break candidate;
        }
        n += 1;
    };
    install_doc(open_file_doc(PathBuf::from(name)));
}

/// File ▸ Open…: the platform picker. A file with a local path opens IN PLACE; a provider
/// document (mobile content URI) is imported — copied into the app's documents and opened
/// there, which is the honest cross-platform reading of "open".
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn open_doc_dialog() {
    day::task(async {
        let Some(file) = open_file()
            .title(crate::res::str::menu_open())
            .filter("Day Sketch", &["daysketch"])
            .await
        else {
            return;
        };
        let path = match file.local_path() {
            Some(p) => Some(p),
            None => match (file.read(), day::persistence::Sqlite::app_data_dir()) {
                (Ok(bytes), Ok(dir)) => {
                    let name = file
                        .file_name()
                        .unwrap_or_else(|| "Imported.daysketch".into());
                    let dest = dir.join(name);
                    std::fs::write(&dest, bytes).ok().map(|_| dest)
                }
                _ => None,
            },
        };
        if let Some(p) = path {
            install_doc(open_file_doc(p));
        }
    });
}

/// File ▸ Open… on the web: the browser picker. The picked bytes import into the OPFS pool
/// under the file's own name (replacing a same-named drawing), then open as the current
/// document — every open is an import, since a picked browser file has no in-place identity.
#[cfg(target_arch = "wasm32")]
pub(crate) fn open_doc_dialog() {
    day::task(async {
        let Some(file) = open_file()
            .title(crate::res::str::menu_open())
            .filter("Day Sketch", &["daysketch"])
            .await
        else {
            return;
        };
        let Ok(storage) = day::persistence::Sqlite::web_storage() else {
            return;
        };
        let (Some(name), Ok(bytes)) = (file.file_name(), file.read()) else {
            return;
        };
        if storage.import_db(&name, &bytes).is_ok() {
            install_doc(open_file_doc(PathBuf::from(name)));
        }
    });
}

/// File ▸ Export a Copy…: flush, snapshot (`backup_to` — consistent mid-write), and hand the
/// bytes to the platform's save dialog. Editing continues on the CURRENT file.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn export_copy_dialog() {
    let d = doc();
    let Some(container) = d.container.clone() else {
        return;
    };
    let name = format!("{}.daysketch", doc_name());
    day::task(async move {
        let tmp = day::app_temp_dir().join(format!("sketch-export-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        if container.backup_to(&tmp).is_err() {
            return;
        }
        let Ok(bytes) = std::fs::read(&tmp) else {
            return;
        };
        let _ = std::fs::remove_file(&tmp);
        let _ = save_file(bytes)
            .title(crate::res::str::menu_export())
            .suggested_name(name)
            .filter("Day Sketch", &["daysketch"])
            .await;
    });
}

/// File ▸ Export a Copy… on the web: flush, export the database image from the in-memory
/// pool, and hand the bytes to the browser's download. The OPFS mirror is not consulted —
/// the pool IS the current state.
#[cfg(target_arch = "wasm32")]
pub(crate) fn export_copy_dialog() {
    let d = doc();
    let Some(container) = d.container.clone() else {
        return;
    };
    let Some(path) = d.path.clone() else {
        return;
    };
    let name = format!("{}.daysketch", doc_name());
    day::task(async move {
        if container.save().is_err() {
            return;
        }
        let Ok(storage) = day::persistence::Sqlite::web_storage() else {
            return;
        };
        let Ok(bytes) = storage.export_db(&path.to_string_lossy()) else {
            return;
        };
        let _ = save_file(bytes)
            .title(crate::res::str::menu_export())
            .suggested_name(name)
            .filter("Day Sketch", &["daysketch"])
            .await;
    });
}

// ---------------------------------------------------------------------------
// Editor state (per app, not per document — deliberately outside the store, so selecting is
// never an undo step and never a row)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Tool {
    #[default]
    Select,
    Rect,
    Oval,
}

pub(crate) fn tool() -> Signal<Tool> {
    thread_local! {
        static TOOL: Signal<Tool> = Signal::global(Tool::Select);
    }
    TOOL.with(|s| *s)
}

/// The selected TOP-LEVEL node ids, in selection order.
pub(crate) fn selection() -> Signal<Vec<u64>> {
    thread_local! {
        static SEL: Signal<Vec<u64>> = Signal::global(Vec::new());
    }
    SEL.with(|s| *s)
}

// ---------------------------------------------------------------------------
// Scene queries (all plain reads over the store)
// ---------------------------------------------------------------------------

fn next_id(store: Store<Keyed<Node>>) -> u64 {
    store.with_untracked(|k| k.items().iter().map(|n| n.id).max().unwrap_or(0)) + 1
}

fn max_z(store: Store<Keyed<Node>>, parent: Option<u64>) -> f64 {
    store.with_untracked(|k| {
        k.items()
            .iter()
            .filter(|n| n.parent == parent)
            .map(|n| n.z)
            .fold(0.0, f64::max)
    })
}

/// [`children_of`], with TRACKED z and parent reads — the canvas draw's variant: an arrange
/// (a plain z write, no restructure) or a reparent then repaints immediately instead of on
/// the next selection change.
pub(crate) fn children_of_tracked(parent: Option<u64>) -> Vec<u64> {
    let store = nodes();
    let mut kids: Vec<(u64, f64)> = store
        .with_untracked(|k| k.items().iter().map(|n| n.id).collect::<Vec<_>>())
        .into_iter()
        .filter_map(|id| {
            let e = store.elem(id);
            (e.parent().read() == parent).then(|| (id, e.z().read()))
        })
        .collect();
    kids.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    kids.into_iter().map(|(id, _)| id).collect()
}

/// Select every top-level node, in z order — Edit ▸ Select All through the edit bridge.
pub(crate) fn select_all() {
    selection().set(children_of(None));
}

/// Move the selection by (dx, dy) as ONE undo unit — the arrow keys' work (1px, or 10 with
/// shift). A no-op with nothing selected.
pub(crate) fn nudge_selection(dx: f64, dy: f64) {
    let sel = selection().get_untracked();
    if sel.is_empty() {
        return;
    }
    let store = nodes();
    undo_stack().grouped("move", || {
        for top in &sel {
            for s in shape_descendants(*top) {
                let e = store.elem(s);
                e.x().write_commit(e.x().peek() + dx);
                e.y().write_commit(e.y().peek() + dy);
            }
        }
    });
}

/// Children of `parent`, bottom→top.
pub(crate) fn children_of(parent: Option<u64>) -> Vec<u64> {
    let store = nodes();
    let mut kids: Vec<(u64, f64)> = store.with_untracked(|k| {
        k.items()
            .iter()
            .filter(|n| n.parent == parent)
            .map(|n| (n.id, n.z))
            .collect()
    });
    kids.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    kids.into_iter().map(|(id, _)| id).collect()
}

/// Every shape (non-group) under `id`, itself included when it is a shape.
pub(crate) fn shape_descendants(id: u64) -> Vec<u64> {
    let store = nodes();
    let kind = store.elem(id).kind().peek();
    if kind == NodeKind::Group {
        children_of(Some(id))
            .into_iter()
            .flat_map(shape_descendants)
            .collect()
    } else {
        vec![id]
    }
}

/// The top-level ancestor of `id` — what a tap on a grouped shape selects.
#[allow(dead_code)] // the layers panel milestone selects through it
pub(crate) fn top_level_ancestor(id: u64) -> u64 {
    let store = nodes();
    let mut cur = id;
    while let Some(p) = store.elem(cur).parent().peek() {
        cur = p;
    }
    cur
}

/// A node's frame: its own for shapes, the union of its shape descendants for groups.
/// `None` for an empty group.
pub(crate) fn node_bounds(id: u64) -> Option<(f64, f64, f64, f64)> {
    let store = nodes();
    if store.elem(id).kind().peek() != NodeKind::Group {
        let e = store.elem(id);
        return Some((e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek()));
    }
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for s in shape_descendants(id) {
        let e = store.elem(s);
        let (x, y, w, h) = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
        acc = Some(match acc {
            None => (x, y, w, h),
            Some((ax, ay, aw, ah)) => {
                let (r, b) = ((ax + aw).max(x + w), (ay + ah).max(y + h));
                let (nx, ny) = (ax.min(x), ay.min(y));
                (nx, ny, r - nx, b - ny)
            }
        });
    }
    acc
}

// ---------------------------------------------------------------------------
// Operations — each runs inside one event turn, so each is ONE undo unit
// ---------------------------------------------------------------------------

/// Place a new shape with its top-left at (x, y); returns its id.
pub(crate) fn place_shape(kind: NodeKind, x: f64, y: f64) -> u64 {
    let store = nodes();
    let id = next_id(store);
    let z = max_z(store, None) + 1.0;
    let fill = PALETTE[(id as usize) % PALETTE.len()].to_string();
    let label = if kind == NodeKind::Oval {
        "add-oval"
    } else {
        "add-rect"
    };
    store.restructure(label, Op::Insert, id, move |v| {
        v.push(Node {
            id,
            parent: None,
            z,
            kind,
            x,
            y,
            w: DEFAULT_W,
            h: DEFAULT_H,
            fill,
        });
    });
    id
}

/// Group the current selection (2+ top-level nodes) into a new group, selected after.
pub(crate) fn group_selection() {
    let sel = selection().get_untracked();
    if sel.len() < 2 {
        return;
    }
    let store = nodes();
    let gid = next_id(store);
    let z = sel
        .iter()
        .filter_map(|id| store.with_untracked(|k| k.get(*id).map(|n| n.z)))
        .fold(0.0, f64::max);
    store.restructure("group", Op::Insert, gid, move |v| {
        v.push(Node {
            id: gid,
            parent: None,
            z,
            kind: NodeKind::Group,
            ..Default::default()
        });
    });
    for (i, id) in sel.iter().enumerate() {
        store.elem(*id).parent().write(Some(gid));
        store.elem(*id).z().write(i as f64 + 1.0);
    }
    selection().set(vec![gid]);
}

/// Ungroup every selected group: children return to the top level at the group's z, the group
/// row goes away. Non-group selections pass through unchanged.
pub(crate) fn ungroup_selection() {
    let sel = selection().get_untracked();
    let store = nodes();
    let mut after: Vec<u64> = Vec::new();
    for id in sel {
        if store.elem(id).kind().peek() != NodeKind::Group {
            after.push(id);
            continue;
        }
        let base = store.elem(id).z().peek();
        for (i, child) in children_of(Some(id)).into_iter().enumerate() {
            store.elem(child).parent().write(None);
            store
                .elem(child)
                .z()
                .write(base + (i as f64 + 1.0) / 1024.0);
            after.push(child);
        }
        store.restructure("ungroup", Op::Delete, id, move |v| {
            v.remove(id);
        });
    }
    selection().set(after);
}

pub(crate) fn delete_selection() {
    let sel = selection().get_untracked();
    let store = nodes();
    for id in sel {
        // Children first (a group takes its subtree with it), depth-first.
        let mut stack = vec![id];
        let mut order = Vec::new();
        while let Some(n) = stack.pop() {
            order.push(n);
            stack.extend(children_of(Some(n)));
        }
        for n in order.into_iter().rev() {
            store.restructure("delete", Op::Delete, n, move |v| {
                v.remove(n);
            });
        }
    }
    selection().set(Vec::new());
}

// ---------------------------------------------------------------------------
// Clipboard: SVG out, SVG in (docs/menus.md — the edit bridge). The transport is a
// self-contained SVG document, so a copied selection pastes into anything that reads SVG,
// and an SVG fragment from another editor pastes back as shapes.
// ---------------------------------------------------------------------------

/// The selection as a standalone SVG document: shapes bottom→top in sibling order, groups as
/// `<g>`, fills as `#RRGGBB`. `None` when nothing is selected.
pub(crate) fn selection_to_svg() -> Option<String> {
    let sel = selection().get_untracked();
    if sel.is_empty() {
        return None;
    }
    let store = nodes();
    // Union of the selected nodes' bounds → the viewBox, so the document stands alone.
    let mut bounds: Option<(f64, f64, f64, f64)> = None;
    for id in &sel {
        if let Some((x, y, w, h)) = node_bounds(*id) {
            bounds = Some(match bounds {
                None => (x, y, w, h),
                Some((bx, by, bw, bh)) => {
                    let (r, b) = ((bx + bw).max(x + w), (by + bh).max(y + h));
                    let (nx, ny) = (bx.min(x), by.min(y));
                    (nx, ny, r - nx, b - ny)
                }
            });
        }
    }
    let (vx, vy, vw, vh) = bounds?;

    fn write_node(out: &mut String, store: Store<Keyed<Node>>, id: u64) {
        let e = store.elem(id);
        match e.kind().peek() {
            NodeKind::Group => {
                out.push_str("<g>");
                for child in children_of(Some(id)) {
                    write_node(out, store, child);
                }
                out.push_str("</g>");
            }
            NodeKind::Rect => {
                let _ = std::fmt::Write::write_fmt(
                    out,
                    format_args!(
                        "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>",
                        e.x().peek(),
                        e.y().peek(),
                        e.w().peek(),
                        e.h().peek(),
                        e.fill().with(|f| f.cloned().unwrap_or_default()),
                    ),
                );
            }
            NodeKind::Oval => {
                let (x, y, w, h) = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
                let _ = std::fmt::Write::write_fmt(
                    out,
                    format_args!(
                        "<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\" fill=\"{}\"/>",
                        x + w / 2.0,
                        y + h / 2.0,
                        w / 2.0,
                        h / 2.0,
                        e.fill().with(|f| f.cloned().unwrap_or_default()),
                    ),
                );
            }
        }
    }

    // Sibling z-order (bottom→top), so pasting preserves stacking.
    let mut body = String::new();
    for id in children_of(None) {
        if sel.contains(&id) {
            write_node(&mut body, store, id);
        }
    }
    Some(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{vx} {vy} {vw} {vh}\">{body}</svg>"
    ))
}

/// A shape or group parsed out of pasted SVG.
#[derive(Debug, PartialEq)]
enum SvgNode {
    Shape {
        kind: NodeKind,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        fill: Option<String>,
    },
    Group(Vec<SvgNode>),
}

/// A deliberately small SVG reader: `rect`, `ellipse`, `circle`, and `g` (nested), with
/// `fill="#rgb"`/`"#rrggbb"` honored and everything else skipped — enough to round-trip our
/// own documents and to accept simple fragments from other editors. No XML library: the
/// grammar this reads is attributes-in-a-tag, which a scan handles.
fn svg_parse(text: &str) -> Vec<SvgNode> {
    fn attrs(tag: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let bytes = tag.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // attribute name
            while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                i += 1;
            }
            if start == i {
                break;
            }
            let name = tag[start..i].to_ascii_lowercase();
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'=' {
                continue;
            }
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'') {
                continue;
            }
            let quote = bytes[i];
            i += 1;
            let vstart = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            out.push((name, tag[vstart..i].to_string()));
            i += 1;
        }
        out
    }
    fn num(attrs: &[(String, String)], name: &str) -> Option<f64> {
        attrs
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.trim().trim_end_matches("px").parse().ok())
    }
    fn fill(attrs: &[(String, String)]) -> Option<String> {
        let v = attrs
            .iter()
            .find(|(n, _)| n == "fill")?
            .1
            .trim()
            .to_string();
        let hex = v.strip_prefix('#')?;
        match hex.len() {
            6 if hex.chars().all(|c| c.is_ascii_hexdigit()) => {
                Some(format!("#{}", hex.to_uppercase()))
            }
            3 if hex.chars().all(|c| c.is_ascii_hexdigit()) => {
                let e: String = hex.chars().flat_map(|c| [c, c]).collect();
                Some(format!("#{}", e.to_uppercase()))
            }
            _ => None,
        }
    }

    // One pass over the tags, keeping a stack of open groups.
    let mut roots: Vec<SvgNode> = Vec::new();
    let mut stack: Vec<Vec<SvgNode>> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else { break };
        let tag = &after[..close];
        rest = &after[close + 1..];
        if tag.starts_with('!') || tag.starts_with('?') {
            continue; // comment / prolog
        }
        if let Some(closing) = tag.strip_prefix('/') {
            if closing.trim().eq_ignore_ascii_case("g")
                && let Some(children) = stack.pop()
            {
                let group = SvgNode::Group(children);
                match stack.last_mut() {
                    Some(parent) => parent.push(group),
                    None => roots.push(group),
                }
            }
            continue;
        }
        let self_closing = tag.ends_with('/');
        let name_end = tag
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let name = tag[..name_end].to_ascii_lowercase();
        let a = attrs(&tag[name_end..]);
        let shape = match name.as_str() {
            "g" if !self_closing => {
                stack.push(Vec::new());
                continue;
            }
            "rect" => Some(SvgNode::Shape {
                kind: NodeKind::Rect,
                x: num(&a, "x").unwrap_or(0.0),
                y: num(&a, "y").unwrap_or(0.0),
                w: num(&a, "width").unwrap_or(0.0),
                h: num(&a, "height").unwrap_or(0.0),
                fill: fill(&a),
            }),
            "ellipse" => {
                let (cx, cy) = (num(&a, "cx").unwrap_or(0.0), num(&a, "cy").unwrap_or(0.0));
                let (rx, ry) = (num(&a, "rx").unwrap_or(0.0), num(&a, "ry").unwrap_or(0.0));
                Some(SvgNode::Shape {
                    kind: NodeKind::Oval,
                    x: cx - rx,
                    y: cy - ry,
                    w: rx * 2.0,
                    h: ry * 2.0,
                    fill: fill(&a),
                })
            }
            "circle" => {
                let (cx, cy) = (num(&a, "cx").unwrap_or(0.0), num(&a, "cy").unwrap_or(0.0));
                let r = num(&a, "r").unwrap_or(0.0);
                Some(SvgNode::Shape {
                    kind: NodeKind::Oval,
                    x: cx - r,
                    y: cy - r,
                    w: r * 2.0,
                    h: r * 2.0,
                    fill: fill(&a),
                })
            }
            _ => None, // unknown element: skipped, children (if any) still scan
        };
        if let Some(s) = shape {
            // Zero-sized shapes carry no geometry worth pasting.
            let keep = match &s {
                SvgNode::Shape { w, h, .. } => *w > 0.0 && *h > 0.0,
                _ => true,
            };
            if keep {
                match stack.last_mut() {
                    Some(parent) => parent.push(s),
                    None => roots.push(s),
                }
            }
        }
    }
    // Unclosed groups (truncated input): keep their children as top-level shapes.
    while let Some(children) = stack.pop() {
        roots.extend(children);
    }
    roots
}

thread_local! {
    /// Repeated pastes of the SAME payload step further (+16 each), so copies never stack
    /// invisibly; a new copy resets the ladder.
    static PASTE_STEP: RefCell<(u64, f64)> = const { RefCell::new((0, 0.0)) };
}

fn paste_offset(text: &str) -> f64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    let key = h.finish();
    PASTE_STEP.with(|p| {
        let mut p = p.borrow_mut();
        if p.0 == key {
            p.1 += 16.0;
        } else {
            *p = (key, 16.0);
        }
        p.1
    })
}

/// Copy: the selection as SVG (the edit bridge places it on the clipboard).
pub(crate) fn copy_selection_svg() -> Option<String> {
    selection_to_svg()
}

/// Cut: copy, then delete as ONE labeled undo unit.
pub(crate) fn cut_selection_svg() -> Option<String> {
    let svg = selection_to_svg()?;
    undo_stack().grouped("cut", delete_selection);
    Some(svg)
}

/// Paste SVG text: parsed shapes land offset +16 (stepping on repeats), stacked above
/// everything, selected, as one undo unit.
pub(crate) fn paste_clipboard(text: &str) {
    let parsed = svg_parse(text);
    if parsed.is_empty() {
        return;
    }
    let offset = paste_offset(text);
    let store = nodes();
    let mut next = next_id(store);
    let mut z = max_z(store, None);
    let mut pasted: Vec<u64> = Vec::new();

    fn insert(
        store: Store<Keyed<Node>>,
        node: &SvgNode,
        parent: Option<u64>,
        z: f64,
        offset: f64,
        next: &mut u64,
        fallback_fill: &mut impl FnMut(u64) -> String,
    ) -> u64 {
        let id = *next;
        *next += 1;
        match node {
            SvgNode::Shape {
                kind,
                x,
                y,
                w,
                h,
                fill,
            } => {
                let node = Node {
                    id,
                    parent,
                    z,
                    kind: *kind,
                    x: x + offset,
                    y: y + offset,
                    w: w.max(crate::model::MIN_SIZE),
                    h: h.max(crate::model::MIN_SIZE),
                    fill: fill.clone().unwrap_or_else(|| fallback_fill(id)),
                };
                store.restructure("paste", Op::Insert, id, move |v| v.push(node.clone()));
            }
            SvgNode::Group(children) => {
                let group = Node {
                    id,
                    parent,
                    z,
                    kind: NodeKind::Group,
                    ..Default::default()
                };
                store.restructure("paste", Op::Insert, id, move |v| v.push(group.clone()));
                for (i, child) in children.iter().enumerate() {
                    insert(
                        store,
                        child,
                        Some(id),
                        i as f64 + 1.0,
                        offset,
                        next,
                        fallback_fill,
                    );
                }
            }
        }
        id
    }

    undo_stack().grouped("paste", || {
        let mut fallback = |id: u64| PALETTE[(id as usize) % PALETTE.len()].to_string();
        for node in &parsed {
            z += 1.0;
            pasted.push(insert(
                store,
                node,
                None,
                z,
                offset,
                &mut next,
                &mut fallback,
            ));
        }
        // INSIDE the group: the unit's transient-selection snapshot is taken as the group
        // seals, and it must already say "the pasted nodes" (docs/model.md).
        selection().set(std::mem::take(&mut pasted));
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Arrange {
    Up,
    Down,
    Top,
    Bottom,
}

/// Reorder every selected node among ITS OWN siblings — one fractional-z write per node.
pub(crate) fn arrange_selection(op: Arrange) {
    let store = nodes();
    for id in selection().get_untracked() {
        let parent = store.elem(id).parent().peek();
        let sibs = children_of(parent);
        let Some(pos) = sibs.iter().position(|s| *s == id) else {
            continue;
        };
        let z_of = |i: usize| store.elem(sibs[i]).z().peek();
        let new_z = match op {
            Arrange::Up if pos + 1 < sibs.len() => {
                if pos + 2 < sibs.len() {
                    (z_of(pos + 1) + z_of(pos + 2)) / 2.0
                } else {
                    z_of(pos + 1) + 1.0
                }
            }
            Arrange::Down if pos > 0 => {
                if pos >= 2 {
                    (z_of(pos - 1) + z_of(pos - 2)) / 2.0
                } else {
                    z_of(pos - 1) - 1.0
                }
            }
            Arrange::Top if pos + 1 < sibs.len() => z_of(sibs.len() - 1) + 1.0,
            Arrange::Bottom if pos > 0 => z_of(0) - 1.0,
            _ => continue,
        };
        store.elem(id).z().write(new_z);
    }
}

/// The arrange label all four ops share in the undo history.
pub(crate) fn arrange_named(op: Arrange) {
    undo_stack().grouped("arrange", move || arrange_selection(op));
}

/// A container-backed memory doc WITHOUT the platform undo bridge (no tree headless),
/// installed as current — the fixture the model and inspector tests share.
#[cfg(test)]
pub(crate) fn install_test_doc() -> Rc<Doc> {
    let container = day::persistence::ModelContainer::open(
        day::persistence::Sqlite::memory(),
        day::persistence::schema![Node],
    )
    .expect("open");
    let store = container.store::<Node>();
    let stack = container.undo(1000);
    let doc = Rc::new(Doc {
        store,
        container: Some(container),
        stack,
        path: None,
    });
    DOC.with(|d| *d.borrow_mut() = Some(doc.clone()));
    selection().set(Vec::new());
    // The selection context, but not the platform bridge — tests exercise the transient
    // restoration exactly as the app wires it.
    wire_selection_context(&doc.stack);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_doc() -> Rc<Doc> {
        install_test_doc()
    }

    #[test]
    fn shift_tap_toggles_membership_and_plain_tap_replaces() {
        use crate::canvas::select_at;
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Rect, 200.0, 0.0);
        day::reactive::flush_sync();

        let plain = day::Modifiers::default();
        let shift = day::Modifiers {
            shift: true,
            ..Default::default()
        };
        select_at(Point::new(10.0, 10.0), plain);
        assert_eq!(selection().get_untracked(), vec![a]);
        select_at(Point::new(210.0, 10.0), shift);
        assert_eq!(selection().get_untracked(), vec![a, b], "shift adds");
        select_at(Point::new(10.0, 10.0), shift);
        assert_eq!(
            selection().get_untracked(),
            vec![b],
            "shift on a member removes it"
        );
        select_at(Point::new(10.0, 10.0), plain);
        assert_eq!(selection().get_untracked(), vec![a], "plain replaces");
        // The command key works like shift (the platform's other multi-select modifier).
        let primary = day::Modifiers {
            primary: true,
            ..Default::default()
        };
        select_at(Point::new(210.0, 10.0), primary);
        assert_eq!(selection().get_untracked(), vec![a, b]);
    }

    #[test]
    fn select_all_selects_top_level_nodes_in_z_order() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Oval, 50.0, 0.0);
        day::reactive::flush_sync();
        selection().set(vec![b]);
        arrange_named(Arrange::Bottom);
        day::reactive::flush_sync();
        select_all();
        assert_eq!(selection().get_untracked(), vec![b, a], "bottom→top order");
    }

    #[test]
    fn nudging_moves_the_selection_as_one_undo_unit_each() {
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 40.0, 40.0);
        day::reactive::flush_sync();
        selection().set(vec![a]);
        nudge_selection(1.0, 0.0);
        day::reactive::flush_sync();
        nudge_selection(0.0, 10.0);
        day::reactive::flush_sync();
        let e = nodes().elem(a);
        assert_eq!((e.x().peek(), e.y().peek()), (41.0, 50.0));
        assert!(doc.stack.undo());
        assert_eq!(
            (e.x().peek(), e.y().peek()),
            (41.0, 40.0),
            "one unit per nudge"
        );
        assert!(doc.stack.undo());
        assert_eq!((e.x().peek(), e.y().peek()), (40.0, 40.0));
    }

    #[test]
    fn arranging_reruns_tracked_draw_order_immediately() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Rect, 10.0, 0.0);
        day::reactive::flush_sync();

        // The same reads draw_scene makes: a bind over the TRACKED order re-fires when an
        // arrange writes z — the repaint that used to wait for the next selection change.
        let seen: Rc<RefCell<Vec<Vec<u64>>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        day::reactive::bind(
            || children_of_tracked(None),
            move |order: &Vec<u64>| sink.borrow_mut().push(order.clone()),
        );
        day::reactive::flush_sync();
        assert_eq!(seen.borrow().last(), Some(&vec![a, b]));

        selection().set(vec![a]);
        arrange_named(Arrange::Top);
        day::reactive::flush_sync();
        assert_eq!(
            seen.borrow().last(),
            Some(&vec![b, a]),
            "a plain z write repaints the order at once"
        );
    }

    #[test]
    fn undo_restores_the_selection_each_unit_sealed_with() {
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Rect, 100.0, 0.0);
        day::reactive::flush_sync();

        // Select A, move it; select B, move it — the selects themselves are no units.
        selection().set(vec![a]);
        nudge_selection(10.0, 0.0);
        day::reactive::flush_sync();
        selection().set(vec![b]);
        nudge_selection(10.0, 0.0);
        day::reactive::flush_sync();

        // Undo B's move: history lands on A's move — A selected again, B back in place.
        assert!(doc.stack.undo());
        assert_eq!(selection().get_untracked(), vec![a]);
        assert_eq!(nodes().elem(b).x().peek(), 100.0);

        // Redo lands back on B's move, with B selected.
        assert!(doc.stack.redo());
        assert_eq!(selection().get_untracked(), vec![b]);
        assert_eq!(nodes().elem(b).x().peek(), 110.0);

        // A selection made BETWEEN undos is transient: the next undo restores the sealed
        // snapshot over it.
        selection().set(vec![a, b]);
        assert!(doc.stack.undo());
        assert_eq!(selection().get_untracked(), vec![a]);

        // Unwinding the whole history lands on the fresh document: nothing selected.
        while doc.stack.undo() {}
        day::reactive::flush_sync();
        assert!(selection().get_untracked().is_empty());
    }

    #[test]
    fn copy_paste_round_trips_as_svg() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 10.0, 20.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Oval, 100.0, 50.0);
        day::reactive::flush_sync();
        selection().set(vec![a, b]);

        let svg = selection_to_svg().expect("selection serializes");
        assert!(
            svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""),
            "{svg}"
        );
        assert!(
            svg.contains("<rect x=\"10\" y=\"20\" width=\"96\" height=\"64\""),
            "{svg}"
        );
        assert!(
            svg.contains("<ellipse cx=\"148\" cy=\"82\" rx=\"48\" ry=\"32\""),
            "{svg}"
        );

        paste_clipboard(&svg);
        day::reactive::flush_sync();
        let store = nodes();
        assert_eq!(store.with_untracked(|k| k.items().len()), 4);
        let sel = selection().get_untracked();
        assert_eq!(sel.len(), 2, "pasted nodes are the selection: {sel:?}");
        // +16 offset, kind and fill preserved.
        let p = store.elem(sel[0]);
        assert_eq!((p.x().peek(), p.y().peek()), (26.0, 36.0));
        assert_eq!(p.kind().peek(), NodeKind::Rect);
        assert_eq!(
            p.fill().with(|f| f.cloned()),
            nodes().elem(a).fill().with(|f| f.cloned())
        );
        let q = store.elem(sel[1]);
        assert_eq!(q.kind().peek(), NodeKind::Oval);
        assert_eq!((q.x().peek(), q.y().peek()), (116.0, 66.0));

        // The SAME payload pastes again one step further; the whole paste is one undo unit.
        paste_clipboard(&svg);
        day::reactive::flush_sync();
        assert_eq!(store.with_untracked(|k| k.items().len()), 6);
        let again = selection().get_untracked();
        assert_eq!(store.elem(again[0]).x().peek(), 42.0, "+32 on the repeat");
        assert!(undo_stack().undo());
        day::reactive::flush_sync();
        assert_eq!(store.with_untracked(|k| k.items().len()), 4);
    }

    #[test]
    fn cut_serializes_then_deletes_as_one_unit() {
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 30.0, 40.0);
        day::reactive::flush_sync();
        selection().set(vec![a]);
        let svg = cut_selection_svg().expect("cut yields the payload");
        day::reactive::flush_sync();
        assert!(svg.contains("<rect x=\"30\" y=\"40\""), "{svg}");
        assert_eq!(nodes().with_untracked(|k| k.items().len()), 0);
        assert!(doc.stack.undo(), "one step brings it back");
        day::reactive::flush_sync();
        assert_eq!(nodes().with_untracked(|k| k.items().len()), 1);
    }

    #[test]
    fn grouped_shapes_travel_as_svg_groups() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Oval, 50.0, 10.0);
        day::reactive::flush_sync();
        selection().set(vec![a, b]);
        group_selection();
        day::reactive::flush_sync();

        let svg = selection_to_svg().expect("group serializes");
        assert!(svg.contains("<g><rect"), "group wraps its children: {svg}");

        paste_clipboard(&svg);
        day::reactive::flush_sync();
        let sel = selection().get_untracked();
        let [gid] = sel.as_slice() else {
            panic!("one pasted top-level group: {sel:?}");
        };
        assert_eq!(nodes().elem(*gid).kind().peek(), NodeKind::Group);
        assert_eq!(children_of(Some(*gid)).len(), 2);
        assert_eq!(
            node_bounds(*gid),
            Some((16.0, 16.0, 146.0, 74.0)),
            "children landed +16 from the original union"
        );
    }

    #[test]
    fn foreign_svg_fragments_paste_as_shapes() {
        let _doc = test_doc();
        // Another editor's output: prolog, comments, unknown elements, short hex, circle.
        let svg = r##"<?xml version="1.0"?>
            <!-- exported elsewhere -->
            <svg xmlns="http://www.w3.org/2000/svg" width="200" height="200">
              <desc>two shapes</desc>
              <path d="M0 0 L10 10"/>
              <circle cx="60" cy="60" r="25" fill="#f00"/>
              <rect x="5.5" y="6.5" width="40px" height="30" fill='#00FF00'/>
            </svg>"##;
        paste_clipboard(svg);
        day::reactive::flush_sync();
        let store = nodes();
        assert_eq!(
            store.with_untracked(|k| k.items().len()),
            2,
            "path/desc skipped"
        );
        let sel = selection().get_untracked();
        let circle = store.elem(sel[0]);
        assert_eq!(circle.kind().peek(), NodeKind::Oval);
        assert_eq!(
            (circle.x().peek(), circle.y().peek(), circle.w().peek()),
            (51.0, 51.0, 50.0),
            "circle → oval at cx-r+16"
        );
        assert_eq!(
            circle.fill().with(|f| f.cloned()).as_deref(),
            Some("#FF0000")
        );
        let rect = store.elem(sel[1]);
        assert_eq!((rect.x().peek(), rect.y().peek()), (21.5, 22.5));
        assert_eq!(rect.fill().with(|f| f.cloned()).as_deref(), Some("#00FF00"));
    }

    #[test]
    fn placing_a_shape_is_one_insert_and_one_undo_unit() {
        let doc = test_doc();
        let container = doc.container.clone().unwrap();
        let sql = container
            .record_sql(|| {
                place_shape(NodeKind::Rect, 10.0, 20.0);
            })
            .expect("save");
        assert_eq!(sql.len(), 1, "{sql:?}");
        assert!(sql[0].starts_with("INSERT INTO nodes"));

        day::reactive::flush_sync();
        assert!(doc.stack.undo(), "one step removes it");
        assert_eq!(nodes().keys().len(), 0);
        assert!(doc.stack.redo());
        assert_eq!(nodes().keys().len(), 1);
    }

    #[test]
    fn a_resize_storm_is_one_unit_and_one_update() {
        let doc = test_doc();
        let container = doc.container.clone().unwrap();
        let id = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        container.save().expect("flush");

        let store = nodes();
        let e = store.elem(id);
        let sql = container
            .record_sql(|| {
                for i in 1..=60 {
                    let f = i as f64;
                    e.x().write_preview(f);
                    e.y().write_preview(f);
                    e.w().write_preview(96.0 + f);
                    e.h().write_preview(64.0 + f);
                }
                doc.stack.grouped("resize", || {
                    e.x().write_commit(30.0);
                    e.y().write_commit(40.0);
                    e.w().write_commit(120.0);
                    e.h().write_commit(80.0);
                });
            })
            .expect("save");
        assert_eq!(
            sql,
            ["UPDATE nodes SET x = ?, y = ?, w = ?, h = ? WHERE id = ?"],
            "sixty frames, one UPDATE of four columns"
        );

        assert!(doc.stack.undo(), "…and one undo unit");
        assert_eq!(e.x().peek(), 0.0);
        assert_eq!(e.w().peek(), 96.0);
        assert!(!doc.stack.can_undo().get_untracked() || doc.stack.undo());
    }

    #[test]
    fn group_ungroup_round_trips_with_undo() {
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Oval, 50.0, 10.0);
        day::reactive::flush_sync();

        selection().set(vec![a, b]);
        group_selection();
        day::reactive::flush_sync();
        let sel = selection().get_untracked();
        let [gid] = sel.as_slice() else {
            panic!("group selected after grouping: {sel:?}");
        };
        let gid = *gid;
        assert_eq!(nodes().elem(gid).kind().peek(), NodeKind::Group);
        assert_eq!(nodes().elem(a).parent().peek(), Some(gid));
        assert_eq!(children_of(None), vec![gid]);
        assert_eq!(
            node_bounds(gid),
            Some((0.0, 0.0, 146.0, 74.0)),
            "group bounds are the union of its shapes"
        );

        // One step back: both shapes top-level again, the group row gone.
        assert!(doc.stack.undo());
        assert_eq!(nodes().elem(a).parent().peek(), None);
        assert!(!nodes().elem(gid).exists());
        assert!(doc.stack.redo());
        assert_eq!(nodes().elem(a).parent().peek(), Some(gid));

        selection().set(vec![gid]);
        ungroup_selection();
        day::reactive::flush_sync();
        assert!(!nodes().elem(gid).exists());
        assert_eq!(nodes().elem(a).parent().peek(), None);
        assert_eq!(selection().get_untracked(), vec![a, b]);
        assert!(doc.stack.undo(), "…and ungroup undoes too");
        assert!(nodes().elem(gid).exists());
    }

    #[test]
    fn arrange_moves_one_row_per_step() {
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Rect, 10.0, 0.0);
        day::reactive::flush_sync();
        let c = place_shape(NodeKind::Rect, 20.0, 0.0);
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![a, b, c]);

        selection().set(vec![a]);
        arrange_named(Arrange::Up);
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![b, a, c]);

        arrange_named(Arrange::Top);
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![b, c, a]);

        arrange_named(Arrange::Bottom);
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![a, b, c]);

        arrange_named(Arrange::Down);
        day::reactive::flush_sync();
        assert_eq!(
            children_of(None),
            vec![a, b, c],
            "already at the bottom: no-op"
        );

        // The whole ladder unwinds.
        let mut undone = 0;
        while doc.stack.undo() {
            undone += 1;
        }
        assert_eq!(undone, 6, "three placements + three arranges");
        assert_eq!(children_of(None), Vec::<u64>::new());
    }

    #[test]
    fn moving_a_group_moves_its_shapes_as_one_unit() {
        let doc = test_doc();
        let container = doc.container.clone().unwrap();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let b = place_shape(NodeKind::Rect, 100.0, 0.0);
        day::reactive::flush_sync();
        selection().set(vec![a, b]);
        group_selection();
        day::reactive::flush_sync();
        container.save().expect("flush");

        // The canvas drag's shape: previews per tick over every descendant, commits ONE turn.
        let store = nodes();
        let sql = container
            .record_sql(|| {
                for step in 1..=10 {
                    let d = step as f64;
                    for s in [a, b] {
                        store.elem(s).x().write_preview(d);
                    }
                }
                doc.stack.grouped("move", || {
                    store.elem(a).x().write_commit(25.0);
                    store.elem(b).x().write_commit(125.0);
                });
            })
            .expect("save");
        assert_eq!(sql.len(), 2, "one UPDATE per moved shape: {sql:?}");

        assert!(doc.stack.undo());
        assert_eq!(store.elem(a).x().peek(), 0.0);
        assert_eq!(store.elem(b).x().peek(), 100.0);
    }
}
