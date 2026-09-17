//! Object clipboard: SVG for editable round trips, standard images for other applications.
use crate::{
    images::ImageBytes,
    model::{self, NodeFields, NodeKind},
};
use day::clipboard::{Content, Representation};
use day::prelude::*;
use std::{rc::Rc, sync::Arc};

pub(crate) const DRAWING: &str = "application/x-day-sketch+svg";
pub(crate) const IMAGE_TYPES: &[&str] = &[
    "image/png",
    "image/tiff",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
];
const TYPES: &[&str] = &[
    DRAWING,
    "image/svg+xml",
    "text/html",
    "image/png",
    "image/tiff",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
    "text/plain",
];

pub(crate) fn install() {
    day::install_edit_bridge(
        || {
            let can = !model::selection().get().is_empty();
            day::EditState {
                can_copy: can,
                can_cut: can,
                can_paste: true,
                can_select_all: true,
            }
        },
        |op| match op {
            day::EditOp::Copy => copy(false),
            day::EditOp::Cut => copy(true),
            day::EditOp::Paste => paste(),
            day::EditOp::SelectAll => model::select_all(),
        },
    );
}
pub(crate) fn drawing_content(svg: String, image: Option<ImageBytes>) -> Content {
    let mut reps = vec![
        Representation::new(DRAWING, svg.as_bytes()),
        Representation::new("image/svg+xml", svg.as_bytes()),
        Representation::new("text/plain", svg.as_bytes()),
        Representation::new("text/html", svg.as_bytes()),
    ];
    if let Some(bytes) = image
        && let Some(format) = day::ImageFormat::sniff(&bytes.0)
    {
        reps.insert(
            0,
            Representation {
                mime: format.mime().into(),
                bytes: bytes.0,
            },
        );
    }
    Content(reps)
}
fn copy(cut: bool) {
    let Some(svg) = model::copy_selection_svg() else {
        return;
    };
    let scene = crate::scene();
    let document = model::doc();
    let selected = scene.selection.get_untracked();
    let image = (selected.len() == 1)
        .then(|| document.store.elem(selected[0]))
        .filter(|n| n.kind().peek() == NodeKind::Image)
        .map(|n| n.image_bytes().peek());
    let needs_png = image
        .as_ref()
        .filter(|b| day::ImageFormat::sniff(&b.0) != Some(day::ImageFormat::Png))
        .cloned();
    let mut content = drawing_content(svg, image);
    // PNG is the browser's interoperable bitmap clipboard format. Keep the original encoded
    // bytes in the SVG representation while offering PNG to image consumers as well.
    let pending: day::clipboard::ClipboardFuture<Vec<String>> = if let Some(bytes) = needs_png {
        Box::pin(async move {
            let bitmap = day::decode_image(bytes.0)
                .await
                .map_err(|_| day::clipboard::Error::InvalidData)?;
            let png = bitmap
                .encode(day::EncodeSpec {
                    format: day::ImageFormat::Png,
                    ..Default::default()
                })
                .await
                .map_err(|_| day::clipboard::Error::InvalidData)?;
            content.0.insert(0, Representation::new("image/png", png));
            day::clipboard::write(content).await
        })
    } else {
        // Start immediately, while a browser copy event/user activation is still live.
        day::clipboard::write(content)
    };
    day::task(async move {
        match pending.await {
            Ok(_)
                if cut
                    && Rc::ptr_eq(&document, &model::doc())
                    && scene.selection.try_get().as_ref() == Some(&selected) =>
            {
                model::undo_stack().grouped("cut", model::delete_selection);
            }
            Ok(_) => (),
            Err(_) => {
                alert(crate::res::str::clipboard_failed()).await;
            }
        }
    });
}
fn paste() {
    let scene = crate::scene();
    let document = model::doc();
    let pending = day::clipboard::read(TYPES);
    day::task(async move {
        let mut representation = match pending.await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(_) => {
                alert(crate::res::str::clipboard_failed()).await;
                return;
            }
        };
        if !Rc::ptr_eq(&document, &model::doc()) || scene.selection.try_get().is_none() {
            return;
        }
        if representation.mime == DRAWING
            || representation.mime == "image/svg+xml"
            || (representation.mime == "text/plain" || representation.mime == "text/html")
        {
            if let Ok(text) = std::str::from_utf8(&representation.bytes)
                && model::can_paste_svg(text)
            {
                model::paste_clipboard(text);
                return;
            }
            // Foreign image clips sometimes also offer a URL/title as plain text.
            representation = match day::clipboard::read(IMAGE_TYPES).await {
                Ok(Some(r)) => r,
                _ => return,
            };
        }
        let bytes = representation.bytes;
        let bitmap = match day::decode_image(Arc::clone(&bytes)).await {
            Ok(b) => b,
            Err(_) => {
                alert(crate::res::str::image_open_failed()).await;
                return;
            }
        };
        if !Rc::ptr_eq(&document, &model::doc()) || scene.selection.try_get().is_none() {
            return;
        }
        let frame = crate::images::initial_frame(&scene, bitmap.info().pixels);
        let id = model::place_image(ImageBytes(bytes), frame);
        scene.selection.set(vec![id]);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_image_copy_offers_original_bytes_and_editable_svg() {
        let bytes: ImageBytes = include_bytes!("../resource/images/app_logo.png")
            .to_vec()
            .into();
        let c = drawing_content("<svg/>".into(), Some(bytes.clone()));
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            c.0.iter().find(|r| r.mime == "image/png").unwrap().bytes,
            bytes.0
        );
        assert_eq!(
            &**c.0.iter().find(|r| r.mime == DRAWING).unwrap().bytes,
            b"<svg/>"
        );
    }
}
