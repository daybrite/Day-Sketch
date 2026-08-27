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
pub(crate) const DEFAULT_W: f64 = 96.0;
pub(crate) const DEFAULT_H: f64 = 64.0;
/// A new line's length — longer than a rectangle is wide, since a line has only the one
/// dimension to read.
pub(crate) const DEFAULT_LINE: f64 = 144.0;
const PALETTE: [&str; 6] = [
    "#3B82F6", "#EF4444", "#10B981", "#F59E0B", "#8B5CF6", "#EC4899",
];

// ---------------------------------------------------------------------------
// The scene model
// ---------------------------------------------------------------------------

/// What a node IS. Every `match` on this in the app is exhaustive on purpose — no `_` arm —
/// so adding a kind here makes the compiler walk you through drawing it, hitting it, writing
/// it out, and inspecting it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum NodeKind {
    #[default]
    Rect,
    Oval,
    /// A segment from the frame's ORIGIN to its far point: `(x, y)` is the start and
    /// `(x + w, y + h)` the end, so `w`/`h` are SIGNED and a line can run in any direction.
    /// [`node_bounds`] normalizes that into the rectangle everything else (selection, group
    /// unions, the inspector's fields) works in.
    Line,
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
                NodeKind::Line => "line",
                NodeKind::Group => "group",
            }
            .into(),
        )
    }
    fn from_sqlite_value(v: day::persistence::Value) -> Result<Self, day::persistence::DbError> {
        match v.as_text()? {
            "rect" => Ok(NodeKind::Rect),
            "oval" => Ok(NodeKind::Oval),
            "line" => Ok(NodeKind::Line),
            "group" => Ok(NodeKind::Group),
            other => Err(day::persistence::DbError::new(
                day::persistence::DbErrorKind::Decode,
                format!("not a node kind: {other:?}"),
            )),
        }
    }
}

#[derive(Clone, PartialEq, Model)]
#[model(table = "nodes", index("parent", "z"))]
pub(crate) struct Node {
    #[model(id)]
    pub id: u64,
    /// The tree: a top-level node's parent is NULL; a group's children point at it. The
    /// reference is the single source of truth — `children` below is an index over it.
    pub parent: Option<One<Node>>,
    /// Sibling order, bottom to top. Fractional: moving a layer writes ONE row. Maintained
    /// by the ordered relation, so an arrange is `move_to` rather than hand-rolled halving.
    pub z: f64,
    /// A group's contents, in z order — the framework's index over `parent`, so reading them
    /// is O(1) and TRACKED at one path instead of a scan that subscribes to every node.
    /// `cascade`: deleting a group takes its subtree, through the normal pipeline, as one
    /// undo unit.
    #[model(relation(target = Node, inverse = "parent", delete = "cascade", ordered = "z"))]
    pub children: Many<Node>,
    pub kind: NodeKind,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// `#RRGGBB` — the color well's currency, and SVG's `fill`.
    pub fill: String,
    /// 0..=1 — SVG's `fill-opacity`.
    pub fill_opacity: f64,
    /// `#RRGGBB` — SVG's `stroke`.
    pub stroke: String,
    /// Points — SVG's `stroke-width`; 0 draws no outline.
    pub stroke_width: f64,
    /// 0..=1 — SVG's `stroke-opacity`.
    pub stroke_opacity: f64,
    /// Degrees clockwise about the shape's own center, 0..360 — SVG's `transform="rotate(…)"`.
    pub rotation: f64,
    /// Corner rounding in points — SVG's `rx`/`ry` on a `<rect>`. Rectangles only: an oval has
    /// no corners, and the inspector hides the row rather than showing a dead field.
    pub corner_radius: f64,
}

/// Document-level settings: ONE row (id 1), seeded at open. A second model in the same
/// container, so its writes ride the same change log, autosave, and undo stack as the scene —
/// a background change is one UPDATE and one undo unit with zero extra wiring.
#[derive(Clone, PartialEq, Model)]
#[model(table = "doc")]
pub(crate) struct DocMeta {
    #[model(id)]
    pub id: u64,
    /// `#RRGGBB` — the canvas background.
    pub background: String,
}

/// White: a drawing is paper, in both themes — and the value migration seeds into files from
/// before the `doc` table existed.
impl Default for DocMeta {
    fn default() -> Self {
        DocMeta {
            id: 0,
            background: "#FFFFFF".into(),
        }
    }
}

/// The settings row's fixed id.
const META_ROW: u64 = 1;

/// Hand-written because these ARE the values lightweight migration backfills into a file from
/// before the style columns existed (docs/persistence.md) — and the stroke trio reproduces the
/// hairline every drawing had then: black at 35%, one point wide.
impl Default for Node {
    fn default() -> Self {
        Node {
            id: 0,
            parent: None,
            children: Many::default(),
            z: 0.0,
            kind: NodeKind::default(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            fill: String::new(),
            fill_opacity: 1.0,
            stroke: "#000000".into(),
            stroke_width: 1.0,
            stroke_opacity: 0.35,
            rotation: 0.0,
            corner_radius: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

pub(crate) struct Doc {
    pub store: Store<Keyed<Node>>,
    pub meta: Store<Keyed<DocMeta>>,
    pub container: Option<day::persistence::ModelContainer>,
    /// The top-level nodes, in z order — see [`doc_from_driver_inner`].
    pub roots: day::persistence::Query<Node>,
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
                "add-line" => crate::res::str::undo_add_line().format(),
                "move" => crate::res::str::undo_move().format(),
                "resize" => crate::res::str::undo_resize().format(),
                "group" => crate::res::str::undo_group().format(),
                "ungroup" => crate::res::str::undo_ungroup().format(),
                "arrange" => crate::res::str::undo_arrange().format(),
                "delete" => crate::res::str::undo_delete().format(),
                "cut" => crate::res::str::undo_cut().format(),
                "paste" => crate::res::str::undo_paste().format(),
                "style" => crate::res::str::undo_style().format(),
                "rotate" => crate::res::str::undo_rotate().format(),
                "corner" => crate::res::str::undo_corner().format(),
                "background" => crate::res::str::undo_background().format(),
                "reparent" => crate::res::str::undo_reparent().format(),
                "duplicate" => crate::res::str::undo_duplicate().format(),
                other => other.to_string(),
            }
        });
}

pub(crate) fn nodes() -> Store<Keyed<Node>> {
    doc().store
}

pub(crate) fn meta() -> Store<Keyed<DocMeta>> {
    doc().meta
}

/// The canvas background, tracked — the draw and the color well both follow a change live.
pub(crate) fn background() -> String {
    meta()
        .elem(META_ROW)
        .background()
        .with(|b| b.cloned().unwrap_or_else(|| DocMeta::default().background))
}

/// Set the background as ONE labeled undo unit — the Canvas tab's color well.
pub(crate) fn set_background(hex: &str) {
    let hex = hex.to_string();
    undo_stack().grouped("background", || {
        meta().elem(META_ROW).background().write_commit(hex);
    });
}

/// Seed the settings row into a store that lacks it. Runs BEFORE the undo stack watches the
/// store, so a fresh (or pre-`doc`-table) file opens without an undoable unit — the seed is
/// backfill, like a migration.
fn ensure_meta(meta: Store<Keyed<DocMeta>>) {
    if meta.with_untracked(|k| k.get(META_ROW).is_none()) {
        meta.restructure("background", Op::Insert, META_ROW, |v| {
            v.push(DocMeta {
                id: META_ROW,
                ..Default::default()
            });
        });
    }
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
    match doc_from_driver_inner(driver, path) {
        Ok(doc) => doc,
        // A file that will not open (corrupt, wrong schema) must not take the app down:
        // fall back to an in-memory scene and surface nothing worse than an empty canvas.
        Err(_) => memory_doc(),
    }
}

/// The open itself, fallible — so [`memory_doc`] can call it without the fallback recursing
/// into itself.
fn doc_from_driver_inner(
    driver: day::persistence::Sqlite,
    path: Option<PathBuf>,
) -> Result<Doc, day::persistence::DbError> {
    let container =
        day::persistence::ModelContainer::open(driver, day::persistence::schema![Node, DocMeta])?;
    let store = container.store::<Node>();
    let meta = container.store::<DocMeta>();
    ensure_meta(meta);
    let stack = container.undo(1000);
    // The top level of the scene, kept live. A relation hangs off a parent ROW, and the top
    // level has no such row — so the roots are a query over "no parent", maintained
    // incrementally and read as a tracked list exactly like a group's children.
    let roots = container
        .query::<Node>()
        .filter(Node::parent().is_unset())
        .sort(Node::z().asc())
        .live();
    if let Some(p) = &path {
        day::prefs::set(LAST_DOC_KEY, &p.to_string_lossy());
    }
    Ok(Doc {
        store,
        meta,
        container: Some(container),
        roots,
        stack,
        path,
    })
}

fn open_file_doc(path: PathBuf) -> Doc {
    doc_from_driver(traced(day::persistence::Sqlite::at(&path)), Some(path))
}

/// The fallback document: in memory, but still a CONTAINER — relations are wired by the
/// container, so a bare store would leave the scene graph with no `children` index at all.
/// An in-memory SQLite costs nothing and keeps every document shape identical.
fn memory_doc() -> Doc {
    match doc_from_driver_inner(day::persistence::Sqlite::memory(), None) {
        Ok(doc) => doc,
        // Nothing left to fall back to; a store without relations would draw an empty scene,
        // which is worse than the panic that says why.
        Err(e) => panic!("cannot open an in-memory document: {e}"),
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

/// Children of `parent`, bottom→top — O(1) from the relation index, already in z order.
/// `None` asks for the top level, which is the document's live root query.
///
/// TRACKED: reading a group's children subscribes to that group's relation path, and reading
/// the roots subscribes to the root query. Either way it is ONE dependency, not one per node
/// — which is what lets the canvas repaint on an arrange without waking on every unrelated
/// field in the document.
pub(crate) fn children_of(parent: Option<u64>) -> Vec<u64> {
    match parent {
        Some(p) => nodes()
            .elem(p)
            .children()
            .ids()
            .into_iter()
            .map(|id| id.handle())
            .collect(),
        None => doc()
            .roots
            .ids()
            .into_iter()
            .map(|id| id.handle())
            .collect(),
    }
}

/// Select every top-level node, in z order — Edit ▸ Select All through the edit bridge.
pub(crate) fn select_all() {
    selection().set(children_of(None));
}

/// Which groups the layers tree shows open. Session state, not document state — per-run like
/// the inspector's visibility, and shared as a signal so grouping can disclose the new group.
pub(crate) fn open_groups() -> Signal<std::collections::HashSet<u64>> {
    thread_local! {
        static OPEN: Signal<std::collections::HashSet<u64>> =
            Signal::global(std::collections::HashSet::new());
    }
    OPEN.with(|s| *s)
}

/// A node's kind, untracked — the layers tree's branch/leaf rule and its labels read it
/// outside any reactive scope (guards, type-ahead).
pub(crate) fn node_kind(id: u64) -> Option<NodeKind> {
    nodes().with_untracked(|k| k.get(id).map(|n| n.kind))
}

/// A layer row's display name — the node's kind plus its id ("Rectangle 3"), which is also
/// the tree's type-ahead text.
pub(crate) fn layer_label(kind: NodeKind, id: u64) -> String {
    let n = id as i64;
    match kind {
        NodeKind::Rect => crate::res::str::layer_rect(n).format(),
        NodeKind::Oval => crate::res::str::layer_oval(n).format(),
        NodeKind::Line => crate::res::str::layer_line(n).format(),
        NodeKind::Group => crate::res::str::layer_group(n).format(),
    }
}

/// `id`'s parent, untracked (`None` = a top-level node).
pub(crate) fn parent_of(id: u64) -> Option<u64> {
    nodes()
        .elem(id)
        .parent()
        .peek()
        .and_then(|r| r.id())
        .map(|p| p.handle())
}

/// The child of `ancestor` that lies on the path down to `descendant` — the double-click
/// drill's next selection (docs/tree.md's canvas counterpart). `None` when `ancestor` is not
/// a proper ancestor of `descendant`.
pub(crate) fn child_toward(ancestor: u64, descendant: u64) -> Option<u64> {
    let mut cur = descendant;
    loop {
        let p = parent_of(cur)?;
        if p == ancestor {
            return Some(cur);
        }
        cur = p;
    }
}

/// Is `node` inside `ancestor`'s subtree — or the node itself?
pub(crate) fn is_within(node: u64, ancestor: u64) -> bool {
    let mut cur = Some(node);
    while let Some(c) = cur {
        if c == ancestor {
            return true;
        }
        cur = parent_of(c);
    }
    false
}

/// Move `id` under `new_parent` (`None` = the top level) at `index` among the target's
/// children, bottom→top (`None` = on top of them) — the layers tree's drop commit, as ONE
/// undo unit. `index` counts the target's children BEFORE the move (the drop vocabulary
/// native trees speak, docs/tree.md), so a same-parent move past its own old slot lands
/// where the user aimed rather than one row short.
pub(crate) fn reparent(id: u64, new_parent: Option<u64>, index: Option<usize>) {
    undo_stack().grouped("reparent", || reparent_now(id, new_parent, index));
}

/// [`reparent`]'s body, WITHOUT the undo grouping — so a multi-node operation (Remove from
/// Group) can wrap several moves in one unit.
fn reparent_now(id: u64, new_parent: Option<u64>, index: Option<usize>) {
    let store = nodes();
    if Some(id) == new_parent {
        return;
    }
    // Mirror the tree's structural guard for programmatic callers: only groups take
    // children, and a node never moves into its own subtree.
    if let Some(p) = new_parent {
        if node_kind(p) != Some(NodeKind::Group) {
            return;
        }
        let mut cur = Some(p);
        while let Some(c) = cur {
            if c == id {
                return;
            }
            cur = parent_of(c);
        }
    }
    let old_parent = parent_of(id);
    let before = children_of(new_parent);
    // The final resting position among the target's OTHER children.
    let old_pos = (old_parent == new_parent)
        .then(|| before.iter().position(|s| *s == id))
        .flatten();
    let others_len = before.len() - old_pos.map(|_| 1).unwrap_or(0);
    let target = match index {
        None => others_len,
        Some(i) => {
            let mut t = i;
            if let Some(p) = old_pos
                && p < i
            {
                t -= 1;
            }
            t.min(others_len)
        }
    };
    if old_parent != new_parent {
        store.elem(id).parent().write(new_parent.map(One::to));
    }
    match new_parent {
        // The ordered relation places it: `target` is the final index among the
        // siblings (self included — it is one of them by now).
        Some(p) => {
            store.elem(p).children().move_to(id, target);
        }
        // The top level hangs off no parent row — the same fractional-z scheme
        // `arrange_selection` uses, against the OTHER roots.
        None => {
            let others: Vec<u64> = children_of(None).into_iter().filter(|s| *s != id).collect();
            let z_of = |i: usize| store.elem(others[i]).z().peek();
            let new_z = if others.is_empty() {
                1.0
            } else if target == 0 {
                z_of(0) - 1.0
            } else if target >= others.len() {
                z_of(others.len() - 1) + 1.0
            } else {
                (z_of(target - 1) + z_of(target)) / 2.0
            };
            store.elem(id).z().write(new_z);
        }
    }
}

/// Move every selected node that sits inside a group out to the TOP LEVEL (on top, in
/// selection order) — the context menu's "Remove from Group", one undo unit.
pub(crate) fn remove_selection_from_group() {
    let sel: Vec<u64> = selection()
        .get_untracked()
        .into_iter()
        .filter(|id| parent_of(*id).is_some())
        .collect();
    if sel.is_empty() {
        return;
    }
    undo_stack().grouped("reparent", || {
        for id in sel {
            reparent_now(id, None, None);
        }
    });
}

/// Duplicate the selection in place (offset, on top, selected) — the same insert path a
/// paste takes, WITHOUT touching the system clipboard.
pub(crate) fn duplicate_selection() {
    let Some(svg) = copy_selection_svg() else {
        return;
    };
    paste_text(&svg, "duplicate");
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
                // Only the axis the key moves. An arrow changes exactly one coordinate, and
                // writing the other back unchanged would put a column in the `UPDATE` with
                // nothing to say. Nothing to cancel here the way a drag has (`canvas::seal`) —
                // a nudge is one keystroke, so no preview session was ever opened.
                if dx != 0.0 {
                    e.x().write_commit(e.x().peek() + dx);
                }
                if dy != 0.0 {
                    e.y().write_commit(e.y().peek() + dy);
                }
            }
        }
    });
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
    while let Some(p) = store.elem(cur).parent().peek().and_then(|r| r.id()) {
        cur = p.handle();
    }
    cur
}

/// One SHAPE's rectangle, always normalized. A line stores signed deltas (its start is the
/// origin, its end is `origin + (w, h)`), so the rectangle it occupies is the absolute one —
/// which is what selection, group unions and the inspector's fields all mean by "the frame".
pub(crate) fn shape_frame(id: u64) -> (f64, f64, f64, f64) {
    let e = nodes().elem(id);
    let (x, y, w, h) = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
    match e.kind().peek() {
        NodeKind::Rect | NodeKind::Oval | NodeKind::Group => (x, y, w, h),
        NodeKind::Line => (x.min(x + w), y.min(y + h), w.abs(), h.abs()),
    }
}

/// A line's two ends, in model space — the RAW fields, before normalization.
pub(crate) fn line_ends(id: u64) -> ((f64, f64), (f64, f64)) {
    let e = nodes().elem(id);
    let (x, y) = (e.x().peek(), e.y().peek());
    ((x, y), (x + e.w().peek(), y + e.h().peek()))
}

/// `(x, y)` turned `degrees` about `(cx, cy)`.
fn turn_about(x: f64, y: f64, cx: f64, cy: f64, degrees: f64) -> (f64, f64) {
    let (sin, cos) = degrees.to_radians().sin_cos();
    let (dx, dy) = (x - cx, y - cy);
    (cx + dx * cos - dy * sin, cy + dx * sin + dy * cos)
}

/// Turn `id` to `angle` degrees.
///
/// A shape turns about its own center. A GROUP turns as one body: every shape inside it orbits
/// the group's center AND turns by the same amount, so the arrangement holds its shape instead
/// of each piece spinning where it stands. The members keep world-space frames — this app has
/// no transform hierarchy, and a group is a parent, not a coordinate space — so the turn is
/// applied to them rather than stored above them. What the GROUP stores is the angle it is
/// currently at, which is what the inspector reads back and what turns its selection outline.
///
/// A line has no orientation of its own; its direction IS its two ends, so it turns by moving
/// them rather than by taking an angle.
pub(crate) fn set_rotation(id: u64, angle: f64, commit: bool) {
    let store = nodes();
    let e = store.elem(id);
    let write = |f: day::model::Field<day::model::Elem<Node>, Node, f64>, v: f64| {
        if commit {
            f.write_commit(v);
        } else {
            f.write_preview(v);
        }
    };
    if e.kind().peek() != NodeKind::Group {
        write(e.rotation(), angle);
        return;
    }
    let delta = angle - e.rotation().peek();
    // The pivot is the members' COLLECTIVE centre — the mean of their centres — because the
    // turn itself fixes that point: any sequence of turns pivots on the same spot and a full
    // circle comes home exactly. (The derived union's centre would reshape with every turn
    // and walk the pivot; the group's bounds are always derived now, so there is no stored
    // frame to anchor on.)
    let shapes = shape_descendants(id);
    if shapes.is_empty() {
        return;
    }
    let (mut cx, mut cy) = (0.0, 0.0);
    for s in &shapes {
        let (x, y, w, h) = shape_frame(*s);
        (cx, cy) = (cx + x + w / 2.0, cy + y + h / 2.0);
    }
    let n = shapes.len() as f64;
    let (cx, cy) = (cx / n, cy / n);
    for s in shapes {
        let m = store.elem(s);
        match m.kind().peek() {
            NodeKind::Line => {
                let ((ax, ay), (bx, by)) = line_ends(s);
                let (ax, ay) = turn_about(ax, ay, cx, cy, delta);
                let (bx, by) = turn_about(bx, by, cx, cy, delta);
                write(m.x(), ax);
                write(m.y(), ay);
                write(m.w(), bx - ax);
                write(m.h(), by - ay);
            }
            NodeKind::Rect | NodeKind::Oval | NodeKind::Group => {
                let (x, y, w, h) = (m.x().peek(), m.y().peek(), m.w().peek(), m.h().peek());
                let (mx, my) = turn_about(x + w / 2.0, y + h / 2.0, cx, cy, delta);
                write(m.x(), mx - w / 2.0);
                write(m.y(), my - h / 2.0);
                write(
                    m.rotation(),
                    (m.rotation().peek() + delta).rem_euclid(360.0),
                );
            }
        }
    }
    write(e.rotation(), angle);
}

/// A node's frame: its own for shapes, the union of its members for groups — ALWAYS derived,
/// so the outline can never go stale as members move, resize, turn, or leave through the
/// layers tree. (Groups carried a frame of their own until 2026-08; it stopped tracking the
/// members the moment one was edited individually, which deep selection made easy to do.)
/// A rotated member contributes the box it VISUALLY occupies. `None` for an empty group.
pub(crate) fn node_bounds(id: u64) -> Option<(f64, f64, f64, f64)> {
    let store = nodes();
    if store.elem(id).kind().peek() != NodeKind::Group {
        return Some(shape_frame(id));
    }
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for s in shape_descendants(id) {
        let (x, y, w, h) = visual_frame(store, s);
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

/// The box a shape VISUALLY occupies: its frame, widened to its corners' reach when it is
/// turned — a rotated rectangle pokes outside its own x/y/w/h, and a group outline that
/// "encompasses its members" has to cover what is actually on the canvas. (An oval's turned
/// box is the rectangle's — a slight over-cover, never an under-cover.)
fn visual_frame(store: Store<Keyed<Node>>, id: u64) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = shape_frame(id);
    let e = store.elem(id);
    let r = match e.kind().peek() {
        NodeKind::Rect | NodeKind::Oval => e.rotation().peek().rem_euclid(360.0),
        // A line's direction IS its endpoints; a group never reaches here
        // (`shape_descendants` yields shapes).
        NodeKind::Line | NodeKind::Group => 0.0,
    };
    if r == 0.0 {
        return (x, y, w, h);
    }
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (mut lx, mut ly, mut hx, mut hy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (px, py) in [(x, y), (x + w, y), (x, y + h), (x + w, y + h)] {
        let (rx, ry) = turn_about(px, py, cx, cy, r);
        (lx, ly) = (lx.min(rx), ly.min(ry));
        (hx, hy) = (hx.max(rx), hy.max(ry));
    }
    (lx, ly, hx - lx, hy - ly)
}

// ---------------------------------------------------------------------------
// Operations — each runs inside one event turn, so each is ONE undo unit
// ---------------------------------------------------------------------------

/// Place a new shape with its top-left at (x, y); returns its id.
pub(crate) fn place_shape(kind: NodeKind, x: f64, y: f64) -> u64 {
    let store = nodes();
    let id = next_id(store);
    // Above whatever is already at the top level. The root query is in z order, so its last
    // row is the highest — no scan over the document.
    let z = children_of(None)
        .last()
        .map(|top| store.elem(*top).z().peek() + 1.0)
        .unwrap_or(1.0);
    let fill = PALETTE[(id as usize) % PALETTE.len()].to_string();
    let defaults = Node::default();
    // Per kind: the undo label, the starting geometry, and the stroke. A line IS its stroke,
    // so it starts visible and solid where a filled shape starts with the hairline outline.
    let (label, w, h, stroke_width, stroke_opacity) = match kind {
        NodeKind::Rect => (
            "add-rect",
            DEFAULT_W,
            DEFAULT_H,
            defaults.stroke_width,
            defaults.stroke_opacity,
        ),
        NodeKind::Oval => (
            "add-oval",
            DEFAULT_W,
            DEFAULT_H,
            defaults.stroke_width,
            defaults.stroke_opacity,
        ),
        NodeKind::Line => ("add-line", DEFAULT_LINE, 0.0, 2.0, 1.0),
        // A group is made by grouping a selection, never placed — but the arm is written out
        // rather than defaulted, so a new kind cannot slip through this table unnoticed.
        NodeKind::Group => (
            "group",
            DEFAULT_W,
            DEFAULT_H,
            defaults.stroke_width,
            defaults.stroke_opacity,
        ),
    };
    store.restructure(label, Op::Insert, id, move |v| {
        v.push(Node {
            id,
            parent: None,
            z,
            kind,
            x,
            y,
            w,
            h,
            fill,
            stroke_width,
            stroke_opacity,
            ..Node::default()
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
    // The group carries NO frame of its own: its bounds are always derived from its members
    // (`node_bounds`), so the outline can never go stale as they change.
    store.restructure("group", Op::Insert, gid, move |v| {
        v.push(Node {
            id: gid,
            parent: None,
            children: Many::default(),
            z,
            kind: NodeKind::Group,
            ..Default::default()
        });
    });
    for (i, id) in sel.iter().enumerate() {
        store.elem(*id).parent().write(Some(One::to(gid)));
        store.elem(*id).z().write(i as f64 + 1.0);
    }
    selection().set(vec![gid]);
    // The new group starts open in the layers tree: its members just visibly became its
    // children, and a collapsed row would hide the very thing the user did.
    open_groups().update(|open| {
        open.insert(gid);
    });
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
        // A group takes its subtree with it — the relation's `cascade` rule walks it, through
        // the same pipeline, so it stays one undo unit and the canvas animates the rows out.
        store.restructure("delete", Op::Delete, id, move |v| {
            v.remove(id);
        });
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

    /// SVG's own way to say "turned about its center": `transform="rotate(a cx cy)"`, empty
    /// at zero so an unrotated shape's markup is unchanged.
    fn rotate_attr(deg: f64, x: f64, y: f64, w: f64, h: f64) -> String {
        if deg.abs() <= f64::EPSILON {
            return String::new();
        }
        format!(
            " transform=\"rotate({} {} {})\"",
            deg,
            x + w / 2.0,
            y + h / 2.0
        )
    }

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
                let (x, y, w, h) = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
                let r = e.corner_radius().peek();
                let radius = if r > 0.0 {
                    format!(" rx=\"{r}\" ry=\"{r}\"")
                } else {
                    String::new()
                };
                let _ = std::fmt::Write::write_fmt(
                    out,
                    format_args!(
                        "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"{} fill=\"{}\" \
                         fill-opacity=\"{}\" stroke=\"{}\" stroke-width=\"{}\" \
                         stroke-opacity=\"{}\"{}/>",
                        x,
                        y,
                        w,
                        h,
                        radius,
                        e.fill().with(|f| f.cloned().unwrap_or_default()),
                        e.fill_opacity().peek(),
                        e.stroke().with(|s| s.cloned().unwrap_or_default()),
                        e.stroke_width().peek(),
                        e.stroke_opacity().peek(),
                        rotate_attr(e.rotation().peek(), x, y, w, h),
                    ),
                );
            }
            NodeKind::Line => {
                // SVG's own line: two endpoints, no fill — the raw (signed) fields, so the
                // direction survives the round trip.
                let ((x1, y1), (x2, y2)) = line_ends(id);
                let _ = std::fmt::Write::write_fmt(
                    out,
                    format_args!(
                        "<line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x2}\" y2=\"{y2}\" \
                         stroke=\"{}\" stroke-width=\"{}\" stroke-opacity=\"{}\"/>",
                        e.stroke().with(|s| s.cloned().unwrap_or_default()),
                        e.stroke_width().peek(),
                        e.stroke_opacity().peek(),
                    ),
                );
            }
            NodeKind::Oval => {
                let (x, y, w, h) = (e.x().peek(), e.y().peek(), e.w().peek(), e.h().peek());
                let _ = std::fmt::Write::write_fmt(
                    out,
                    format_args!(
                        "<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\" fill=\"{}\" \
                         fill-opacity=\"{}\" stroke=\"{}\" stroke-width=\"{}\" \
                         stroke-opacity=\"{}\"{}/>",
                        x + w / 2.0,
                        y + h / 2.0,
                        w / 2.0,
                        h / 2.0,
                        e.fill().with(|f| f.cloned().unwrap_or_default()),
                        e.fill_opacity().peek(),
                        e.stroke().with(|s| s.cloned().unwrap_or_default()),
                        e.stroke_width().peek(),
                        e.stroke_opacity().peek(),
                        rotate_attr(e.rotation().peek(), x, y, w, h),
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

/// A shape or group parsed out of pasted SVG. Style attributes are optional — a foreign
/// fragment without them pastes with the document defaults.
#[derive(Debug, PartialEq)]
enum SvgNode {
    Shape {
        kind: NodeKind,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        fill: Option<String>,
        fill_opacity: Option<f64>,
        stroke: Option<String>,
        stroke_width: Option<f64>,
        stroke_opacity: Option<f64>,
        rotation: Option<f64>,
        corner_radius: Option<f64>,
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
    fn color_attr(attrs: &[(String, String)], name: &str) -> Option<String> {
        let v = attrs.iter().find(|(n, _)| n == name)?.1.trim().to_string();
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
    /// The four style attributes any shape tag may carry, opacities clamped to 0..=1.
    fn style_of(a: &[(String, String)]) -> (Option<f64>, Option<String>, Option<f64>, Option<f64>) {
        (
            num(a, "fill-opacity").map(|v| v.clamp(0.0, 1.0)),
            color_attr(a, "stroke"),
            num(a, "stroke-width").map(|v| v.max(0.0)),
            num(a, "stroke-opacity").map(|v| v.clamp(0.0, 1.0)),
        )
    }

    /// The angle out of `transform="rotate(a …)"`, normalized to 0..360. Only the rotate form
    /// is read — a general matrix would need a decomposition this editor has no field for, so
    /// such a shape pastes upright rather than wrong.
    fn rotation_of(a: &[(String, String)]) -> Option<f64> {
        let t = a
            .iter()
            .find(|(n, _)| n == "transform")?
            .1
            .trim()
            .to_string();
        let inner = t.strip_prefix("rotate(")?.split(')').next()?.to_string();
        let deg: f64 = inner
            .split([',', ' '])
            .find(|s| !s.is_empty())?
            .parse()
            .ok()?;
        Some(deg.rem_euclid(360.0))
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
            "rect" => {
                let (fill_opacity, stroke, stroke_width, stroke_opacity) = style_of(&a);
                Some(SvgNode::Shape {
                    kind: NodeKind::Rect,
                    x: num(&a, "x").unwrap_or(0.0),
                    y: num(&a, "y").unwrap_or(0.0),
                    w: num(&a, "width").unwrap_or(0.0),
                    h: num(&a, "height").unwrap_or(0.0),
                    fill: color_attr(&a, "fill"),
                    fill_opacity,
                    stroke,
                    stroke_width,
                    stroke_opacity,
                    rotation: rotation_of(&a),
                    // SVG allows rx and ry to differ; this editor has one radius, so the
                    // horizontal one wins and a foreign ellipse-cornered rect pastes close.
                    corner_radius: num(&a, "rx").or_else(|| num(&a, "ry")).map(|v| v.max(0.0)),
                })
            }
            "line" => {
                let (fill_opacity, stroke, stroke_width, stroke_opacity) = style_of(&a);
                let (x1, y1) = (num(&a, "x1").unwrap_or(0.0), num(&a, "y1").unwrap_or(0.0));
                let (x2, y2) = (num(&a, "x2").unwrap_or(0.0), num(&a, "y2").unwrap_or(0.0));
                Some(SvgNode::Shape {
                    kind: NodeKind::Line,
                    x: x1,
                    y: y1,
                    // SIGNED: the deltas to the far end, which is how a line is stored.
                    w: x2 - x1,
                    h: y2 - y1,
                    fill: None,
                    fill_opacity,
                    stroke,
                    stroke_width,
                    stroke_opacity,
                    rotation: None,
                    corner_radius: None,
                })
            }
            "ellipse" => {
                let (cx, cy) = (num(&a, "cx").unwrap_or(0.0), num(&a, "cy").unwrap_or(0.0));
                let (rx, ry) = (num(&a, "rx").unwrap_or(0.0), num(&a, "ry").unwrap_or(0.0));
                let (fill_opacity, stroke, stroke_width, stroke_opacity) = style_of(&a);
                Some(SvgNode::Shape {
                    kind: NodeKind::Oval,
                    x: cx - rx,
                    y: cy - ry,
                    w: rx * 2.0,
                    h: ry * 2.0,
                    fill: color_attr(&a, "fill"),
                    fill_opacity,
                    stroke,
                    stroke_width,
                    stroke_opacity,
                    rotation: rotation_of(&a),
                    corner_radius: None,
                })
            }
            "circle" => {
                let (cx, cy) = (num(&a, "cx").unwrap_or(0.0), num(&a, "cy").unwrap_or(0.0));
                let r = num(&a, "r").unwrap_or(0.0);
                let (fill_opacity, stroke, stroke_width, stroke_opacity) = style_of(&a);
                Some(SvgNode::Shape {
                    kind: NodeKind::Oval,
                    x: cx - r,
                    y: cy - r,
                    w: r * 2.0,
                    h: r * 2.0,
                    fill: color_attr(&a, "fill"),
                    fill_opacity,
                    stroke,
                    stroke_width,
                    stroke_opacity,
                    rotation: rotation_of(&a),
                    corner_radius: None,
                })
            }
            _ => None, // unknown element: skipped, children (if any) still scan
        };
        if let Some(s) = shape {
            // Zero-sized shapes carry no geometry worth pasting — but a line is allowed one
            // zero dimension (a horizontal or vertical one has exactly that), so it only
            // needs SOME length.
            let keep = match &s {
                SvgNode::Shape {
                    kind: NodeKind::Line,
                    w,
                    h,
                    ..
                } => *w != 0.0 || *h != 0.0,
                SvgNode::Shape { w, h, .. } => *w > 0.0 && *h > 0.0,
                SvgNode::Group(_) => true,
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
    paste_text(text, "paste");
}

/// [`paste_clipboard`]'s body with its own undo label — Duplicate shares the insert path
/// but should read "Undo Duplicate", not "Undo Paste".
fn paste_text(text: &str, label: &'static str) {
    let parsed = svg_parse(text);
    if parsed.is_empty() {
        return;
    }
    let offset = paste_offset(text);
    let store = nodes();
    let mut next = next_id(store);
    let mut z = children_of(None)
        .last()
        .map(|top| store.elem(*top).z().peek())
        .unwrap_or(0.0);
    let mut pasted: Vec<u64> = Vec::new();

    fn insert(
        store: Store<Keyed<Node>>,
        node: &SvgNode,
        parent: Option<One<Node>>,
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
                fill_opacity,
                stroke,
                stroke_width,
                stroke_opacity,
                rotation,
                corner_radius,
            } => {
                let defaults = Node::default();
                let node = Node {
                    id,
                    parent,
                    children: Many::default(),
                    z,
                    kind: *kind,
                    x: x + offset,
                    y: y + offset,
                    // A line's deltas are signed and may be zero on one axis; every other
                    // shape gets the size floor.
                    w: match kind {
                        NodeKind::Line => *w,
                        NodeKind::Rect | NodeKind::Oval | NodeKind::Group => {
                            w.max(crate::model::MIN_SIZE)
                        }
                    },
                    h: match kind {
                        NodeKind::Line => *h,
                        NodeKind::Rect | NodeKind::Oval | NodeKind::Group => {
                            h.max(crate::model::MIN_SIZE)
                        }
                    },
                    fill: fill.clone().unwrap_or_else(|| fallback_fill(id)),
                    fill_opacity: fill_opacity.unwrap_or(defaults.fill_opacity),
                    stroke: stroke.clone().unwrap_or(defaults.stroke),
                    stroke_width: stroke_width.unwrap_or(defaults.stroke_width),
                    stroke_opacity: stroke_opacity.unwrap_or(defaults.stroke_opacity),
                    rotation: rotation.unwrap_or(defaults.rotation),
                    corner_radius: corner_radius.unwrap_or(defaults.corner_radius),
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
                        Some(One::to(id)),
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

    undo_stack().grouped(label, || {
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
        let parent = store.elem(id).parent().peek().and_then(|r| r.id());
        let sibs = children_of(parent.map(|p| p.handle()));
        let Some(pos) = sibs.iter().position(|s| *s == id) else {
            continue;
        };
        let target = match op {
            Arrange::Up if pos + 1 < sibs.len() => pos + 1,
            Arrange::Down if pos > 0 => pos - 1,
            Arrange::Top if pos + 1 < sibs.len() => sibs.len() - 1,
            Arrange::Bottom if pos > 0 => 0,
            _ => continue,
        };
        match parent {
            // Inside a group, the ordered relation places it: one row written, and it
            // rebalances the siblings when the fractional gap is spent.
            Some(p) => {
                store.elem(p).children().move_to(id, target);
            }
            // The top level hangs off no parent row, so there is no relation to place into —
            // the same halving, done here. Sibling sets are independent, so the two schemes
            // never meet.
            None => {
                let z_of = |i: usize| store.elem(sibs[i]).z().peek();
                let new_z = if target > pos {
                    // Moving up: land between the row above and the one after it.
                    if target + 1 < sibs.len() {
                        (z_of(target) + z_of(target + 1)) / 2.0
                    } else {
                        z_of(target) + 1.0
                    }
                } else if target >= 1 {
                    (z_of(target) + z_of(target - 1)) / 2.0
                } else {
                    z_of(0) - 1.0
                };
                store.elem(id).z().write(new_z);
            }
        }
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
        day::persistence::schema![Node, DocMeta],
    )
    .expect("open");
    let store = container.store::<Node>();
    let meta = container.store::<DocMeta>();
    ensure_meta(meta);
    // Flush the seed now: the SQL-counting tests must start from a settled file, the way a
    // real document is settled by the first autosave.
    container.save().expect("flush the seed");
    let stack = container.undo(1000);
    let roots = container
        .query::<Node>()
        .filter(Node::parent().is_unset())
        .sort(Node::z().asc())
        .live();
    let doc = Rc::new(Doc {
        store,
        meta,
        container: Some(container),
        roots,
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

    /// The parent's id, unwrapped from the reference — what the tree assertions compare.
    fn parent_of(id: u64) -> Option<u64> {
        nodes()
            .elem(id)
            .parent()
            .peek()
            .and_then(|r| r.id())
            .map(|i| i.handle())
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
            || children_of(None),
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
    fn style_attributes_round_trip_as_svg_and_default_when_absent() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 10.0, 20.0);
        day::reactive::flush_sync();
        let e = nodes().elem(a);
        e.fill_opacity().write(0.5);
        e.stroke().write("#112233".into());
        e.stroke_width().write(3.0);
        e.stroke_opacity().write(0.8);
        day::reactive::flush_sync();
        selection().set(vec![a]);

        let svg = selection_to_svg().expect("serializes");
        assert!(svg.contains("fill-opacity=\"0.5\""), "{svg}");
        assert!(svg.contains("stroke=\"#112233\""), "{svg}");
        assert!(svg.contains("stroke-width=\"3\""), "{svg}");
        assert!(svg.contains("stroke-opacity=\"0.8\""), "{svg}");

        paste_clipboard(&svg);
        day::reactive::flush_sync();
        let p = nodes().elem(selection().get_untracked()[0]);
        assert_eq!(p.fill_opacity().peek(), 0.5);
        assert_eq!(p.stroke().with(|s| s.cloned()).as_deref(), Some("#112233"));
        assert_eq!(p.stroke_width().peek(), 3.0);
        assert_eq!(p.stroke_opacity().peek(), 0.8);

        // A foreign fragment carrying no style attributes pastes with the document defaults —
        // the same values migration backfills into pre-style files.
        paste_clipboard(r##"<svg><rect x="0" y="0" width="30" height="30"/></svg>"##);
        day::reactive::flush_sync();
        let q = nodes().elem(selection().get_untracked()[0]);
        assert_eq!(q.fill_opacity().peek(), 1.0);
        assert_eq!(q.stroke().with(|s| s.cloned()).as_deref(), Some("#000000"));
        assert_eq!(q.stroke_width().peek(), 1.0);
        assert_eq!(q.stroke_opacity().peek(), 0.35);
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
    fn deleting_a_group_flushes_as_one_statement() {
        // The user-reported shape: group several shapes, delete the group, and watch the SQL.
        let doc = test_doc();
        let container = doc.container.clone().unwrap();
        let mut ids = Vec::new();
        for i in 0..4 {
            ids.push(place_shape(NodeKind::Rect, i as f64 * 10.0, 0.0));
        }
        selection().set(ids.clone());
        group_selection();
        day::reactive::flush_sync();
        let gid = selection().get_untracked()[0];
        container.save().expect("settle the seed");

        let sql = container
            .record_sql(|| {
                selection().set(vec![gid]);
                delete_selection();
            })
            .expect("flush");

        assert!(nodes().keys().is_empty(), "the subtree went");
        // One table, one shape: the group and its four shapes leave in a single statement.
        let deletes: Vec<&String> = sql.iter().filter(|s| s.starts_with("DELETE")).collect();
        assert_eq!(deletes.len(), 1, "{sql:?}");
        assert_eq!(deletes[0], "DELETE FROM nodes WHERE id IN (?, ?, ?, ?, ?)");
    }

    #[test]
    fn drill_selects_one_level_deeper_per_repeat_click() {
        use crate::canvas::drill_at;
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0); // 96x64 at the origin
        let b = place_shape(NodeKind::Rect, 200.0, 0.0);
        selection().set(vec![a, b]);
        group_selection();
        day::reactive::flush_sync();
        let g1 = selection().get_untracked()[0];
        let c = place_shape(NodeKind::Rect, 400.0, 0.0);
        selection().set(vec![g1, c]);
        group_selection();
        day::reactive::flush_sync();
        let g2 = selection().get_untracked()[0];

        // With the OUTER group solely selected, each repeat click steps one level down the
        // path to the shape under the pointer: g2 → g1 → a — then HOLDS at the shape.
        selection().set(vec![g2]);
        assert!(drill_at(Point::new(10.0, 10.0)));
        assert_eq!(selection().get_untracked(), vec![g1]);
        assert!(drill_at(Point::new(10.0, 10.0)));
        assert_eq!(selection().get_untracked(), vec![a]);
        assert!(
            drill_at(Point::new(10.0, 10.0)),
            "a repeat on the leaf holds it"
        );
        assert_eq!(selection().get_untracked(), vec![a]);

        // A hit OUTSIDE the sole selection is not a drill — the plain rule handles it.
        selection().set(vec![g1]);
        assert!(!drill_at(Point::new(410.0, 10.0)));
        // Neither is a multi-selection, whatever it covers.
        selection().set(vec![g2, c]);
        assert!(!drill_at(Point::new(10.0, 10.0)));
        // Empty canvas: nothing to drill into.
        selection().set(vec![g2]);
        assert!(!drill_at(Point::new(4000.0, 4000.0)));
    }

    #[test]
    fn child_toward_and_is_within_walk_the_parent_chain() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        let b = place_shape(NodeKind::Rect, 200.0, 0.0);
        selection().set(vec![a, b]);
        group_selection();
        day::reactive::flush_sync();
        let g = selection().get_untracked()[0];

        assert_eq!(child_toward(g, a), Some(a));
        assert_eq!(child_toward(a, g), None, "not an ancestor");
        assert_eq!(child_toward(g, g), None, "a node is not toward itself");
        assert_eq!(child_toward(g, b), Some(b));
        assert!(is_within(a, g));
        assert!(is_within(g, g));
        assert!(!is_within(g, a));
        assert!(!is_within(b, a));
    }

    #[test]
    fn remove_from_group_moves_members_to_the_top_level_in_one_unit() {
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        let b = place_shape(NodeKind::Rect, 100.0, 0.0);
        let c = place_shape(NodeKind::Rect, 200.0, 0.0);
        selection().set(vec![a, b, c]);
        group_selection();
        day::reactive::flush_sync();
        let g = selection().get_untracked()[0];

        // Two members out (the third stays); a top-level id in the selection is ignored.
        selection().set(vec![a, b, g]);
        remove_selection_from_group();
        day::reactive::flush_sync();
        assert_eq!(children_of(Some(g)), vec![c]);
        assert_eq!(
            children_of(None),
            vec![g, a, b],
            "out on top, in selection order"
        );

        // ONE undo unit puts both back.
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(children_of(Some(g)), vec![a, b, c]);
        assert_eq!(children_of(None), vec![g]);

        // Nothing selected sits in a group → a no-op that pushes no unit.
        selection().set(vec![g]);
        remove_selection_from_group();
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![g]);
    }

    #[test]
    fn duplicate_copies_in_place_and_selects_the_copies() {
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 40.0, 40.0);
        let before = nodes().keys().len();
        selection().set(vec![a]);
        duplicate_selection();
        day::reactive::flush_sync();

        assert_eq!(nodes().keys().len(), before + 1);
        let sel = selection().get_untracked();
        assert_eq!(sel.len(), 1, "the COPY is selected");
        assert_ne!(sel[0], a);
        // The paste path's +16 offset — a duplicate lands beside its original, not under it.
        let e = nodes().elem(sel[0]);
        assert_eq!((e.x().peek(), e.y().peek()), (56.0, 56.0));

        // One undo unit removes the copy.
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(nodes().keys().len(), before);
    }

    #[test]
    fn reparent_moves_across_parents_and_orders_by_drop_index() {
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        let b = place_shape(NodeKind::Oval, 10.0, 0.0);
        let c = place_shape(NodeKind::Line, 20.0, 0.0);
        selection().set(vec![a, b]);
        group_selection();
        day::reactive::flush_sync();
        let gid = selection().get_untracked()[0];
        assert_eq!(children_of(None), vec![gid, c]);

        // Into the group, on top (a drop ONTO the group row).
        reparent(c, Some(gid), None);
        day::reactive::flush_sync();
        assert_eq!(children_of(Some(gid)), vec![a, b, c]);
        assert_eq!(children_of(None), vec![gid]);

        // To the bottom of the group (a drop between the group row and its first child).
        reparent(c, Some(gid), Some(0));
        day::reactive::flush_sync();
        assert_eq!(children_of(Some(gid)), vec![c, a, b]);

        // Back out to the top level, below everything.
        reparent(c, None, Some(0));
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![c, gid]);
        assert_eq!(children_of(Some(gid)), vec![a, b]);

        // Each move is ONE undo unit, and undo restores parent AND order together.
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(children_of(Some(gid)), vec![c, a, b]);
        assert_eq!(children_of(None), vec![gid]);
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(children_of(Some(gid)), vec![a, b, c]);
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![gid, c]);
        assert_eq!(children_of(Some(gid)), vec![a, b]);
    }

    #[test]
    fn reparent_adjusts_a_same_parent_drop_past_its_own_slot() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        let b = place_shape(NodeKind::Rect, 10.0, 0.0);
        let c = place_shape(NodeKind::Rect, 20.0, 0.0);
        // The drop index counts the PRE-move list [a, b, c]: dropping a at index 3 (after c)
        // lands it on top, not one short.
        reparent(a, None, Some(3));
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![b, c, a]);
        // And dropping a at index 1 (between b and c) lands exactly there.
        reparent(a, None, Some(1));
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![b, a, c]);
    }

    #[test]
    fn reparent_refuses_itself_non_groups_and_its_own_subtree() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        let b = place_shape(NodeKind::Rect, 10.0, 0.0);
        let c = place_shape(NodeKind::Rect, 20.0, 0.0);
        selection().set(vec![a, b]);
        group_selection();
        day::reactive::flush_sync();
        let g1 = selection().get_untracked()[0];
        selection().set(vec![g1, c]);
        group_selection();
        day::reactive::flush_sync();
        let g2 = selection().get_untracked()[0];

        reparent(g2, Some(g2), None); // itself
        reparent(c, Some(a), None); // a leaf takes no children
        reparent(g2, Some(g1), None); // its own subtree
        day::reactive::flush_sync();
        assert_eq!(children_of(None), vec![g2]);
        assert_eq!(children_of(Some(g2)), vec![g1, c]);
        assert_eq!(children_of(Some(g1)), vec![a, b]);
    }

    #[test]
    fn deleting_a_group_cascades_to_its_whole_subtree() {
        // The app used to walk the subtree itself before deleting. That code is gone: the
        // relation's `cascade` rule does it, so this pins the behavior the app now relies on
        // rather than implements.
        let doc = test_doc();
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        let b = place_shape(NodeKind::Rect, 10.0, 0.0);
        selection().set(vec![a, b]);
        group_selection();
        day::reactive::flush_sync();
        let gid = selection().get_untracked()[0];
        assert_eq!(children_of(Some(gid)), vec![a, b]);

        selection().set(vec![gid]);
        delete_selection();
        day::reactive::flush_sync();
        assert!(
            nodes().with_untracked(|k| k.get(a).is_none()),
            "child went with it"
        );
        assert!(nodes().with_untracked(|k| k.get(b).is_none()));
        assert!(nodes().with_untracked(|k| k.get(gid).is_none()));
        assert_eq!(children_of(None), Vec::<u64>::new());

        // And the whole subtree comes back as ONE undo unit, because the cascade rode the
        // same turn as the delete that caused it.
        assert!(doc.stack.undo());
        assert_eq!(children_of(None), vec![gid]);
        assert_eq!(children_of(Some(gid)), vec![a, b]);
    }

    #[test]
    fn the_draw_order_is_one_dependency_not_one_per_node() {
        // The reason the relation is worth having. The canvas draw is a tracked run over the
        // sibling order; it used to read `parent` and `z` of EVERY node to find them, so the
        // canvas subscribed to two paths per node and any unrelated write woke it. Reading
        // the order through the relation (or, at the top level, the root query) is one
        // dependency whatever the document holds.
        let _doc = test_doc();
        let seen = Rc::new(RefCell::new(0usize));
        let sink = seen.clone();
        day::reactive::bind(
            || children_of(None),
            move |ids: &Vec<u64>| *sink.borrow_mut() = ids.len(),
        );
        day::reactive::flush_sync();

        for _ in 0..5 {
            place_shape(NodeKind::Rect, 0.0, 0.0);
        }
        day::reactive::flush_sync();
        let with_five = day::model::observed_paths();

        for _ in 0..25 {
            place_shape(NodeKind::Rect, 0.0, 0.0);
        }
        day::reactive::flush_sync();

        assert_eq!(*seen.borrow(), 30, "the draw order followed every insert");
        assert_eq!(
            day::model::observed_paths(),
            with_five,
            "six times the nodes must not mean six times the subscriptions"
        );
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
        assert_eq!(parent_of(a), Some(gid));
        assert_eq!(children_of(None), vec![gid]);
        assert_eq!(
            node_bounds(gid),
            Some((0.0, 0.0, 146.0, 74.0)),
            "group bounds are the union of its shapes"
        );

        // One step back: both shapes top-level again, the group row gone.
        assert!(doc.stack.undo());
        assert_eq!(parent_of(a), None);
        assert!(!nodes().elem(gid).exists());
        assert!(doc.stack.redo());
        assert_eq!(parent_of(a), Some(gid));

        selection().set(vec![gid]);
        ungroup_selection();
        day::reactive::flush_sync();
        assert!(!nodes().elem(gid).exists());
        assert_eq!(parent_of(a), None);
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
    fn a_line_stores_signed_deltas_and_normalizes_its_frame() {
        let _doc = test_doc();
        let id = place_shape(NodeKind::Line, 100.0, 100.0);
        day::reactive::flush_sync();
        let e = nodes().elem(id);
        // It starts horizontal, and a line IS its stroke: visible and solid, not a hairline.
        assert_eq!((e.w().peek(), e.h().peek()), (DEFAULT_LINE, 0.0));
        assert_eq!(e.stroke_width().peek(), 2.0);
        assert_eq!(e.stroke_opacity().peek(), 1.0);

        // Point it up and to the LEFT: the fields go negative, and the frame everything else
        // works in is still the rectangle it occupies.
        e.w().write(-40.0);
        e.h().write(-30.0);
        day::reactive::flush_sync();
        assert_eq!(line_ends(id), ((100.0, 100.0), (60.0, 70.0)));
        assert_eq!(node_bounds(id), Some((60.0, 70.0, 40.0, 30.0)));

        // A group's union takes the occupied rectangle, not the raw fields.
        let r = place_shape(NodeKind::Rect, 200.0, 200.0);
        day::reactive::flush_sync();
        selection().set(vec![id, r]);
        group_selection();
        day::reactive::flush_sync();
        let gid = selection().get_untracked()[0];
        assert_eq!(node_bounds(gid), Some((60.0, 70.0, 236.0, 194.0)));
    }

    #[test]
    fn a_line_round_trips_as_svg_with_its_direction() {
        let _doc = test_doc();
        let id = place_shape(NodeKind::Line, 100.0, 100.0);
        day::reactive::flush_sync();
        let e = nodes().elem(id);
        e.w().write(-40.0);
        e.h().write(60.0);
        e.stroke().write("#112233".into());
        day::reactive::flush_sync();
        selection().set(vec![id]);

        let svg = selection_to_svg().expect("serializes");
        assert!(
            svg.contains(r#"<line x1="100" y1="100" x2="60" y2="160""#),
            "{svg}"
        );
        assert!(!svg.contains("fill="), "a line has no fill: {svg}");

        paste_clipboard(&svg);
        day::reactive::flush_sync();
        let p = nodes().elem(selection().get_untracked()[0]);
        assert_eq!(p.kind().peek(), NodeKind::Line);
        // The signed deltas survive — and are NOT floored to MIN_SIZE the way a rect's are.
        assert_eq!((p.w().peek(), p.h().peek()), (-40.0, 60.0));
        assert_eq!(p.stroke().with(|s| s.cloned()).as_deref(), Some("#112233"));

        // A foreign horizontal line has a zero dimension and must still paste.
        paste_clipboard(r##"<svg><line x1="0" y1="5" x2="80" y2="5"/></svg>"##);
        day::reactive::flush_sync();
        let q = nodes().elem(selection().get_untracked()[0]);
        assert_eq!(q.kind().peek(), NodeKind::Line);
        assert_eq!((q.w().peek(), q.h().peek()), (80.0, 0.0));
    }

    #[test]
    fn a_line_is_hit_near_the_segment_not_across_its_bounding_box() {
        use crate::canvas::select_at;
        let _doc = test_doc();
        let id = place_shape(NodeKind::Line, 0.0, 0.0);
        day::reactive::flush_sync();
        // A diagonal from (0,0) to (100,100).
        let e = nodes().elem(id);
        e.w().write(100.0);
        e.h().write(100.0);
        day::reactive::flush_sync();
        let plain = day::Modifiers::default();

        select_at(Point::new(50.0, 52.0), plain);
        assert_eq!(selection().get_untracked(), vec![id], "on the segment");
        // Inside the bounding box, far from the line itself: a rectangle would catch this.
        select_at(Point::new(90.0, 10.0), plain);
        assert!(
            selection().get_untracked().is_empty(),
            "the box is not the shape"
        );
    }

    #[test]
    fn rotation_and_corner_radius_round_trip_as_svg() {
        let _doc = test_doc();
        let a = place_shape(NodeKind::Rect, 10.0, 20.0);
        day::reactive::flush_sync();
        let e = nodes().elem(a);
        e.rotation().write(45.0);
        e.corner_radius().write(8.0);
        day::reactive::flush_sync();
        selection().set(vec![a]);

        let svg = selection_to_svg().expect("serializes");
        assert!(svg.contains("rx=\"8\" ry=\"8\""), "{svg}");
        // Rotation is SVG's own transform, about the shape's center.
        assert!(svg.contains("transform=\"rotate(45 58 52)\""), "{svg}");

        paste_clipboard(&svg);
        day::reactive::flush_sync();
        let p = nodes().elem(selection().get_untracked()[0]);
        assert_eq!(p.rotation().peek(), 45.0);
        assert_eq!(p.corner_radius().peek(), 8.0);

        // An oval carries rotation but never a radius, and a fragment with neither pastes
        // upright and square-cornered (the migration defaults).
        paste_clipboard(r##"<svg><rect x="0" y="0" width="30" height="30"/></svg>"##);
        day::reactive::flush_sync();
        let q = nodes().elem(selection().get_untracked()[0]);
        assert_eq!(q.rotation().peek(), 0.0);
        assert_eq!(q.corner_radius().peek(), 0.0);
    }

    #[test]
    fn a_rotated_shape_is_hit_where_it_is_drawn() {
        use crate::canvas::select_at;
        let _doc = test_doc();
        // A wide, short rectangle at the origin: 96×64 with its center at (48, 32).
        let a = place_shape(NodeKind::Rect, 0.0, 0.0);
        day::reactive::flush_sync();
        let plain = day::Modifiers::default();

        // Upright, a point past the right edge misses…
        select_at(Point::new(48.0, 60.0), plain);
        assert_eq!(selection().get_untracked(), vec![a], "inside upright");
        select_at(Point::new(48.0, 75.0), plain);
        assert!(selection().get_untracked().is_empty(), "below upright");

        // …and turned a quarter turn, the same point is inside the shape as drawn: the
        // 96-long axis now runs vertically through the center.
        nodes().elem(a).rotation().write(90.0);
        day::reactive::flush_sync();
        select_at(Point::new(48.0, 75.0), plain);
        assert_eq!(selection().get_untracked(), vec![a], "inside once rotated");
    }

    #[test]
    fn background_seeds_without_a_unit_and_edits_as_one() {
        let doc = test_doc();
        assert_eq!(background(), "#FFFFFF", "seeded default");
        assert!(
            !doc.stack.can_undo().get_untracked(),
            "the seed is backfill, not history"
        );
        set_background("#112233");
        day::reactive::flush_sync();
        assert_eq!(background(), "#112233");
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert_eq!(background(), "#FFFFFF");
        assert!(doc.stack.redo());
        day::reactive::flush_sync();
        assert_eq!(background(), "#112233");
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
