//! Original file bytes in SQLite; native bitmap handles live only in each canvas's cache.
use crate::model::NodeFields;
use day::prelude::*;
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

/// Sharing keeps node snapshots and undo history from copying megabytes during every drag.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ImageBytes(pub Arc<Vec<u8>>);

impl From<Vec<u8>> for ImageBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Arc::new(bytes))
    }
}

impl day::persistence::ColumnValue for ImageBytes {
    const SQL_TYPE: day::persistence::SqlType = day::persistence::SqlType::Blob;
    fn to_sqlite_value(&self) -> day::persistence::Value {
        day::persistence::Value::Blob(self.0.as_ref().clone())
    }
    fn from_sqlite_value(
        value: day::persistence::Value,
    ) -> Result<Self, day::persistence::DbError> {
        match value {
            day::persistence::Value::Blob(bytes) => Ok(bytes.into()),
            other => Err(day::persistence::DbError::new(
                day::persistence::DbErrorKind::Decode,
                format!("expected image BLOB, found {other:?}"),
            )),
        }
    }
}

struct CachedImage {
    bytes: ImageBytes,
    bitmap: Option<day::Bitmap>,
}

#[derive(Clone)]
pub(crate) struct ImageCache {
    entries: Rc<RefCell<HashMap<u64, CachedImage>>>,
    revision: Signal<u64>,
}

impl ImageCache {
    pub(crate) fn empty() -> Self {
        Self {
            entries: Rc::default(),
            revision: Signal::new(0),
        }
    }

    /// Bind only membership and payloads, not geometry. Reopen, paste and redo use the same
    /// path as insert. Removed rows release their handles; late decodes cannot resurrect them.
    pub(crate) fn mount(store: Store<Keyed<crate::model::Node>>) -> Self {
        let cache = Self::empty();
        let target = cache.clone();
        let scope = day::reactive::Scope::current();
        day::reactive::bind(
            move || {
                store
                    .keys()
                    .into_iter()
                    .filter_map(|id| {
                        let e = store.elem(id);
                        (e.kind().read() == crate::model::NodeKind::Image)
                            .then(|| (id, e.image_bytes().read()))
                    })
                    .collect::<Vec<_>>()
            },
            move |sources: &Vec<(u64, ImageBytes)>| {
                let mut pending = Vec::new();
                {
                    let mut entries = target.entries.borrow_mut();
                    let ids: std::collections::HashSet<_> =
                        sources.iter().map(|(id, _)| *id).collect();
                    entries.retain(|id, _| ids.contains(id));
                    for (id, bytes) in sources {
                        if entries.get(id).is_some_and(|entry| entry.bytes == *bytes) {
                            continue;
                        }
                        entries.insert(
                            *id,
                            CachedImage {
                                bytes: bytes.clone(),
                                bitmap: None,
                            },
                        );
                        pending.push((*id, bytes.clone()));
                    }
                }
                for (id, bytes) in pending {
                    let weak = Rc::downgrade(&target.entries);
                    let revision = target.revision;
                    day::decode_image_async(bytes.0.clone(), move |result| {
                        if !scope.is_alive() {
                            return;
                        }
                        let Some(entries) = weak.upgrade() else {
                            return;
                        };
                        let changed = {
                            let mut entries = entries.borrow_mut();
                            if let Some(entry) = entries.get_mut(&id).filter(|e| e.bytes == bytes) {
                                entry.bitmap = result.ok();
                                true
                            } else {
                                false
                            }
                        };
                        if changed {
                            revision.update(|r| *r += 1);
                        }
                    });
                }
            },
        );
        cache
    }

    pub(crate) fn track(&self) {
        self.revision.track();
    }
    pub(crate) fn get(&self, id: u64) -> Option<day::Bitmap> {
        self.entries
            .borrow()
            .get(&id)
            .and_then(|e| e.bitmap.clone())
    }
}

pub(crate) fn supported() -> bool {
    capability(Cap::ImageDecode) != Support::Unsupported
        && capability(Cap::FileDialogs) != Support::Unsupported
}

/// Fit a new image inside 80% of the visible canvas, preserving aspect and never upscaling.
fn initial_frame(scene: &crate::Scene, size: Size) -> (f64, f64, f64, f64) {
    let (vw, vh) = scene.cells.viewport.get();
    let zoom = scene.zoom.get_untracked();
    let pan = scene.pan.get_untracked();
    let scale = (vw * 0.8 / zoom / size.width)
        .min(vh * 0.8 / zoom / size.height)
        .min(1.0);
    let (w, h) = (size.width * scale, size.height * scale);
    (
        (vw / 2.0 - pan.x) / zoom - w / 2.0,
        (vh / 2.0 - pan.y) / zoom - h / 2.0,
        w,
        h,
    )
}

pub(crate) fn insert_dialog() {
    let scene = crate::scene();
    let document = crate::model::doc();
    day::task(async move {
        if !supported() {
            alert(crate::res::str::image_unavailable()).await;
            return;
        }
        let Some(file) = open_file()
            .title(crate::res::str::tool_image())
            .filter(
                crate::res::str::image_files().format(),
                &[
                    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "heic", "heif",
                    "avif",
                ],
            )
            .await
        else {
            return;
        };
        if !Rc::ptr_eq(&document, &crate::model::doc()) || scene.selection.try_get().is_none() {
            return;
        }
        let decoded = match file.read() {
            Ok(bytes) => {
                let bytes = ImageBytes::from(bytes);
                day::decode_image(bytes.0.clone())
                    .await
                    .map(|bitmap| (bytes, bitmap))
            }
            Err(_) => Err(day::ImageError::Decode),
        };
        if !Rc::ptr_eq(&document, &crate::model::doc()) || scene.selection.try_get().is_none() {
            return;
        }
        if let Ok((bytes, bitmap)) = decoded {
            let size = bitmap.info().pixels;
            if size.width.is_finite()
                && size.height.is_finite()
                && size.width > 0.0
                && size.height > 0.0
            {
                let id = crate::model::place_image(bytes, initial_frame(&scene, size));
                scene.selection.set(vec![id]);
                return;
            }
        }
        alert(crate::res::str::image_open_failed())
            .message(crate::res::str::image_open_failed_detail())
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{self, Node, NodeKind};
    use day::persistence::{SqliteConnection, SqliteDriver, Value};

    fn bytes() -> ImageBytes {
        include_bytes!("../resource/images/app_logo.png")
            .to_vec()
            .into()
    }

    #[test]
    fn image_bytes_survive_edit_undo_duplicate_and_reopen() {
        let doc = model::install_test_doc();
        let bytes = bytes();
        let id = model::place_image(bytes.clone(), (10.0, 20.0, 180.0, 120.0));
        model::selection().set(vec![id]);
        day::reactive::flush_sync();
        let e = doc.store.elem(id);
        let sql = doc
            .container
            .as_ref()
            .unwrap()
            .record_sql(|| {
                doc.stack.grouped("resize", || {
                    e.x().write_commit(30.0);
                    e.w().write_commit(240.0);
                    e.rotation().write_commit(35.0);
                    e.fill_opacity().write_commit(0.4);
                });
            })
            .unwrap();
        assert!(sql.iter().any(|sql| sql.starts_with("UPDATE nodes")));
        assert!(
            sql.iter().all(|sql| !sql.contains("image_bytes")),
            "{sql:?}"
        );
        assert!(doc.stack.undo());
        assert_eq!(
            (
                e.x().peek(),
                e.w().peek(),
                e.rotation().peek(),
                e.fill_opacity().peek()
            ),
            (10.0, 180.0, 0.0, 1.0)
        );
        assert!(doc.stack.redo());
        day::reactive::flush_sync();
        model::duplicate_selection();
        day::reactive::flush_sync();
        let copy = model::selection().get_untracked()[0];
        assert_ne!(copy, id);
        assert_eq!(doc.store.elem(copy).image_bytes().peek(), bytes);
        assert_eq!(doc.store.elem(copy).rotation().peek(), 35.0);
        assert_eq!(doc.store.elem(copy).fill_opacity().peek(), 0.4);
        let svg = model::selection_to_svg().unwrap();
        assert!(svg.contains("data:image/png;base64,"));
        model::paste_clipboard(&svg);
        day::reactive::flush_sync();
        let pasted = model::selection().get_untracked()[0];
        assert_eq!(doc.store.elem(pasted).image_bytes().peek(), bytes);
        assert_eq!(doc.store.elem(pasted).rotation().peek(), 35.0);
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert!(doc.stack.redo());
        day::reactive::flush_sync();

        let path =
            std::env::temp_dir().join(format!("day-sketch-image-{}.sqlite", std::process::id()));
        doc.container.as_ref().unwrap().backup_to(&path).unwrap();
        {
            let mut connection = day::persistence::Sqlite::at(&path).open().unwrap();
            let mut rows = Vec::new();
            connection
                .query(
                    "SELECT typeof(image_bytes), image_bytes FROM nodes WHERE id = ?",
                    &[Value::Int(id as i64)],
                    &mut |row| rows.push(vec![row.get(0), row.get(1)]),
                )
                .unwrap();
            assert_eq!(rows[0][0], Value::Text("blob".into()));
            assert_eq!(rows[0][1], Value::Blob(bytes.0.as_ref().clone()));
        }
        {
            let reopened = day::persistence::ModelContainer::open(
                day::persistence::Sqlite::at(&path),
                day::persistence::schema![Node, model::DocMeta],
            )
            .unwrap();
            reopened.warm::<Node>().unwrap();
            let restored = reopened.cache::<Node>().elem(id);
            assert_eq!(restored.kind().peek(), NodeKind::Image);
            assert_eq!(restored.image_bytes().peek(), bytes);
            assert_eq!(
                (
                    restored.x().peek(),
                    restored.w().peek(),
                    restored.rotation().peek(),
                    restored.fill_opacity().peek()
                ),
                (30.0, 240.0, 35.0, 0.4)
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pre_image_documents_backfill_an_empty_blob() {
        let driver = day::persistence::Sqlite::memory().with_init(|db| {
            db.execute_batch("CREATE TABLE nodes (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, x REAL NOT NULL); INSERT INTO nodes VALUES (1, 'rect', 42);").unwrap();
        });
        let container =
            day::persistence::ModelContainer::open(driver, day::persistence::schema![Node])
                .unwrap();
        container.warm::<Node>().unwrap();
        let node = container.cache::<Node>().elem(1);
        assert_eq!(node.kind().peek(), NodeKind::Rect);
        assert_eq!(node.x().peek(), 42.0);
        assert!(node.image_bytes().peek().0.is_empty());
    }

    #[test]
    fn cache_decodes_once_for_geometry_edits_and_releases_deleted_rows() {
        day_core::uninstall_tree();
        let doc = model::install_test_doc();
        let (mock, probe) = day_mock::MockToolkit::new();
        day_core::launch_with(mock, WindowOptions::default(), || label("test").any());
        let scope = day::reactive::Scope::child();
        let cache = scope.enter(|| ImageCache::mount(doc.store));
        let id = model::place_image(bytes(), (20.0, 30.0, 100.0, 80.0));
        day::reactive::flush_sync();
        assert!(cache.get(id).is_some());
        let bitmap = cache.get(id).unwrap().id();
        probe.clear_log();
        doc.store.elem(id).x().write_commit(100.0);
        doc.store.elem(id).rotation().write_commit(60.0);
        day::reactive::flush_sync();
        assert_eq!(cache.get(id).unwrap().id(), bitmap);
        assert!(!probe.log().iter().any(|s| s.starts_with("decode_image")));
        model::selection().set(vec![id]);
        model::delete_selection();
        day::reactive::flush_sync();
        assert!(cache.get(id).is_none());
        assert!(
            probe
                .log()
                .iter()
                .any(|s| s == &format!("release_image #{}", bitmap.0))
        );
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert!(cache.get(id).is_some());
        scope.dispose();
        drop(cache);
        day_core::uninstall_tree();
    }

    #[test]
    fn insertion_fits_and_centers_in_a_zoomed_panned_view() {
        model::install_test_doc();
        let scene = crate::scene();
        scene.cells.viewport.set((800.0, 600.0));
        scene.zoom.set(2.0);
        scene.pan.set(Point::new(40.0, -20.0));
        assert_eq!(
            initial_frame(&scene, Size::new(1600.0, 800.0)),
            (20.0, 80.0, 320.0, 160.0)
        );
    }
}
