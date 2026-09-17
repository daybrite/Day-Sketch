//! Native canvas import. Validate the complete batch before changing the document.
use crate::{clipboard, images::ImageBytes, model};
use day::{
    prelude::*,
    transfer::{Drop, Item, Operation, Target},
};
use std::{io::Read, rc::Rc, sync::Arc};

enum Imported {
    Drawing(String),
    Image(ImageBytes, Size),
}
fn representation(item: &Item) -> Option<(String, Arc<Vec<u8>>)> {
    [clipboard::DRAWING, "image/svg+xml"]
        .into_iter()
        .chain(clipboard::IMAGE_TYPES.iter().copied())
        .find_map(|mime| {
            item.representations
                .iter()
                .find(|r| r.mime == mime)
                .map(|r| (r.mime.clone(), r.bytes.clone()))
        })
}
async fn validate(item: Item, remaining: &mut usize) -> Option<Vec<Imported>> {
    let reps = if let Some(rep) = representation(&item) {
        *remaining = remaining.checked_sub(rep.1.len())?;
        vec![rep]
    } else {
        let paths = day::transfer::file_paths(item.get("text/uri-list")?)?;
        let mut reps = Vec::new();
        for path in paths {
            let file = std::fs::File::open(path).ok()?;
            let mut bytes = Vec::new();
            file.take((*remaining + 1) as u64)
                .read_to_end(&mut bytes)
                .ok()?;
            *remaining = remaining.checked_sub(bytes.len())?;
            reps.push((String::new(), Arc::new(bytes)));
        }
        reps
    };
    let mut out = Vec::new();
    for (mime, bytes) in reps {
        if (mime == clipboard::DRAWING || mime == "image/svg+xml" || mime.is_empty())
            && let Ok(svg) = std::str::from_utf8(&bytes)
            && model::can_paste_svg(svg)
        {
            out.push(Imported::Drawing(svg.to_owned()));
        } else {
            let bitmap = day::decode_image(bytes.clone()).await.ok()?;
            let size = bitmap.info().pixels;
            if !size.width.is_finite()
                || !size.height.is_finite()
                || size.width <= 0.
                || size.height <= 0.
            {
                return None;
            }
            out.push(Imported::Image(ImageBytes(bytes), size));
        }
    }
    Some(out)
}
fn receive(drop: Drop) -> bool {
    if drop.items.is_empty() {
        return false;
    }
    let scope = day::reactive::Scope::current();
    let scene = crate::scene();
    let document = model::doc();
    let zoom = scene.zoom.get_untracked();
    let pan = scene.pan.get_untracked();
    let position = Point::new(
        (drop.position.x - pan.x) / zoom,
        (drop.position.y - pan.y) / zoom,
    );
    day::task(async move {
        let mut imports = Vec::new();
        let mut remaining = day::transfer::MAX_BYTES;
        for item in drop.items {
            let Some(batch) = validate(item, &mut remaining).await else {
                alert(crate::res::str::image_open_failed()).await;
                return;
            };
            imports.extend(batch);
            if imports.len() > day::transfer::MAX_ITEMS {
                return;
            }
        }
        if !Rc::ptr_eq(&document, &model::doc()) || scene.selection.try_get().is_none() {
            return;
        }
        scope.enter(|| {
            model::undo_stack().grouped("paste", || {
                let mut ids = Vec::new();
                for (index, import) in imports.into_iter().enumerate() {
                    let at = Point::new(
                        position.x + index as f64 * 16.,
                        position.y + index as f64 * 16.,
                    );
                    match import {
                        Imported::Image(bytes, size) => {
                            let (_, _, w, h) = crate::images::initial_frame(&scene, size);
                            ids.push(model::place_image(
                                bytes,
                                (at.x - w / 2., at.y - h / 2., w, h),
                            ));
                        }
                        Imported::Drawing(svg) => {
                            model::paste_clipboard(&svg);
                            let selected = scene.selection.get_untracked();
                            let bounds = selected
                                .iter()
                                .filter_map(|id| model::node_bounds(*id))
                                .reduce(|(x, y, w, h), (a, b, c, d)| {
                                    let left = x.min(a);
                                    let top = y.min(b);
                                    (
                                        left,
                                        top,
                                        (x + w).max(a + c) - left,
                                        (y + h).max(b + d) - top,
                                    )
                                });
                            if let Some((x, y, w, h)) = bounds {
                                model::nudge_selection(at.x - x - w / 2., at.y - y - h / 2.);
                            }
                            ids.extend(selected);
                        }
                    }
                }
                scene.selection.set(ids);
            })
        });
    });
    // Receipt only: the asynchronous import never authorizes deletion in the source app.
    true
}
pub(crate) fn canvas_target() -> Target {
    let mut types = vec![
        clipboard::DRAWING.into(),
        "image/svg+xml".into(),
        "text/uri-list".into(),
    ];
    types.extend(clipboard::IMAGE_TYPES.iter().map(|s| s.to_string()));
    let accepted = types.clone();
    Target {
        types,
        accept: Rc::new(move |at| {
            let (w, h) = crate::scene().cells.viewport.get();
            if at.position.x >= 0.
                && at.position.y >= 0.
                && at.position.x < w
                && at.position.y < h
                && accepted.iter().any(|mime| at.has(mime))
            {
                Operation::Copy
            } else {
                Operation::None
            }
        }),
        receive: Rc::new(receive),
    }
}

/// An explicit handle leaves ordinary canvas manipulation and layer reordering intact.
pub(crate) fn selection_handle() -> impl Piece {
    label(crate::res::str::drag_selection())
        .padding(Insets::all(8.0))
        .drag_source(|_| {
            let svg = model::copy_selection_svg()?;
            let content = clipboard::drawing_content(svg, None);
            Some(day::transfer::Offer {
                items: vec![Item::new(
                    content
                        .0
                        .into_iter()
                        .map(|r| day::transfer::Representation {
                            mime: r.mime,
                            bytes: r.bytes,
                        })
                        .collect(),
                )],
            })
        })
        .id("drag-selection")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeFields;
    #[test]
    fn image_drop_maps_pan_zoom_and_undoes_the_batch() {
        day_core::uninstall_tree();
        let doc = model::install_test_doc();
        let (mock, _) = day_mock::MockToolkit::new();
        day_core::launch_with(mock, WindowOptions::default(), || label("test").any());
        let scene = crate::scene();
        scene.cells.viewport.set((800., 600.));
        scene.zoom.set(2.);
        scene.pan.set(Point::new(40., 20.));
        let bytes = include_bytes!("../resource/images/app_logo.png").to_vec();
        let item = Item::new(vec![day::transfer::Representation::new(
            "image/png",
            bytes.clone(),
        )]);
        assert!(receive(Drop {
            local: false,
            position: Point::new(240., 220.),
            operation: Operation::Copy,
            items: vec![item.clone(), item]
        }));
        day::reactive::flush_sync();
        let ids = scene.selection.get_untracked();
        assert_eq!(ids.len(), 2);
        let first = doc.store.elem(ids[0]);
        assert_eq!((first.x().peek(), first.y().peek()), (68., 76.));
        assert_eq!(first.image_bytes().peek().0.as_slice(), bytes);
        assert!(doc.stack.undo());
        day::reactive::flush_sync();
        assert!(doc.store.keys().is_empty());
        assert!(doc.stack.redo());
        day::reactive::flush_sync();
        assert_eq!(doc.store.keys().len(), 2);
        day_core::uninstall_tree();
    }
}
