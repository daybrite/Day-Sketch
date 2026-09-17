# Day Sketch

A vector drawing editor with drag handles, layers, and unlimited undo, built with
[Day](https://daybrite.dev) in one Rust codebase and rendered with the platform's own widgets on
Mac, iPhone, Android, Windows, Linux, HarmonyOS, and the web. Every drawing is a plain SQLite file
you can copy, share, and inspect.

<p align="center">
  <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/macos-appkit/en/editor.png" width="760" alt="The editor on macOS"></kbd>
</p>

## Run it in one command

Install the `day` CLI, then let it clone, build, and launch the app for your desktop:

```sh
cargo install day-cli
day launch --git https://github.com/daybrite/Day-Sketch.git
```

`day doctor` lists what your platform's toolkit needs and prints the install command for anything
missing. The launch prints where it put the checkout, so you can open the code and change it.

## What you get

Place rectangles, ovals, lines, and text, drag them around, resize them by their handles, turn
them, group them, and arrange the layers. A text node is a line of type in any font the platform
lists, sized by its handles. Alignment guides appear as you drag, and a shape snaps onto a
neighbor's edge or center when it comes within a few pixels of lining up (a preference turns the
snapping off). Every operation is one undoable turn, fronted by the platform's own undo
system where it has one, so ⌘Z on a Mac and the shake gesture on an iPhone both work the way
their platform expects.

<p align="center">
  <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/ios-uikit/iphone/en/editor.png" width="200" alt="The editor on iPhone"></kbd>
  <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/ios-uikit/iphone/en/palette.png" width="200" alt="The palette on iPhone"></kbd>
  <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/android-mdc/phone/en/rotation-fan.png" width="200" alt="A fan of rotated shapes on Android"></kbd>
  <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/android-mdc/phone/en/translucency.png" width="200" alt="Three translucent circles on Android"></kbd>
</p>

Under the canvas, a drawing is one observable table in a SQLite file. A drag edits it live through
a preview session, and the committed result is a row change, which is what makes undo a matter of
replaying turns. The app was the stress test the day-model and day-persistence design was drafted
against.

## The same code on every platform

These captures come from the app's own CI, which runs the walkthrough on every target and
publishes the results to the [gallery](https://daybrite.dev/gallery/Day-Sketch/).

| Windows · XAML | Linux · GTK | Linux · Qt |
|:---:|:---:|:---:|
| <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/windows-xaml/en/editor.png" width="300" alt="The editor on Windows"></kbd> | <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/linux-gtk/en/editor.png" width="300" alt="The editor on GTK"></kbd> | <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/linux-qt/en/editor.png" width="300" alt="The editor on Qt"></kbd> |

| Web · DOM | macOS · outlines | macOS · a group, turned |
|:---:|:---:|:---:|
| <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/web-dom/en/editor.png" width="300" alt="The editor in the browser"></kbd> | <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/macos-appkit/en/outlines.png" width="300" alt="Outline-only shapes on macOS"></kbd> | <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/macos-appkit/en/group-turned.png" width="300" alt="A grouped body rotated as one on macOS"></kbd> |

| macOS · type specimen | iPad · type specimen | Android tablet · type specimen |
|:---:|:---:|:---:|
| <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/macos-appkit/en/type-specimen.png" width="300" alt="Six lines of type in four styles on macOS"></kbd> | <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/ios-uikit/ipad/en/type-specimen.png" width="300" alt="The type specimen on iPad"></kbd> | <kbd><img src="https://daybrite.github.io/Day-Sketch/gallery/android-mdc/tablet/en/type-specimen.png" width="300" alt="The type specimen on an Android tablet"></kbd> |

## Build from a clone

Day compiles one toolkit backend per binary, so name a target when you build or launch. Every
target the app ships is listed in `Day.toml`.

```sh
day doctor                       # toolchains present and missing, with fixes
day launch -p macos-appkit       # build + run
day launch -p ios-uikit          # needs a booted Simulator
day launch -p android-mdc        # needs a JDK and a running emulator or device
day launch -p web-dom            # serves the WebAssembly build locally
```

A bare `cargo build` uses the crate's default `mock` backend, which is what lets rust-analyzer and
`cargo check` work with no flags. To pick a toolkit from plain cargo, turn the default off first:

```sh
cargo build --no-default-features --features appkit    # or gtk / qt / uikit / mdc / xaml / dom
```

`dayscript/demo.yaml` is a [dayscript](https://daybrite.dev/docs/dayscript) that draws, arranges,
undoes, and screenshots along the way. It is the app's UI test and the script CI runs on every
target to produce the gallery:

```sh
day launch -p macos-appkit --script dayscript/demo.yaml
```

To build against a local `day` checkout instead of the pinned git revision, let the CLI write and
verify the patch table:

```sh
day patch --local /path/to/day
```

## Inside the code

- `src/lib.rs` is `root()`: the window shell, the menus with localized shortcuts, and Settings.
- `src/canvas.rs` is the drawing surface: hit testing, handles, drag preview sessions, and the
  shape rendering.
- `src/inspector.rs` is the side panel: the palette, the layer list, and arrangement.
- `src/model.rs` is the scene as a day-model table, persisted to SQLite through
  day-persistence, with every edit as one undoable turn.
- `resource/locales/en/app.ftl` carries every user-facing string.
- `platform/` holds the thin native host projects the mobile targets build through.

`day lint` checks routes, element ids, and locale coverage.

Day Sketch is open source under the Apache-2.0 license.

### Images

Insert → Image… (also in the toolbar’s **+** menu) opens the platform file picker.
The image lands centered in the current view, sized to fit without upscaling. Move or
resize it with the canvas handles or Geometry fields; Rotation and Opacity work like
other node properties. Images participate in grouping, layer order, undo/redo,
duplication, and SVG clipboard copy/paste.

The drawing stores the original encoded file in a SQLite `image_bytes` BLOB. It does
not depend on the source path after insertion. Decoded native bitmaps are cached for
the lifetime of each canvas and released when their nodes disappear. Opening an older
drawing adds the BLOB column with empty values for its existing shapes.

The insertion command requires Day’s image decoder and file-picker capabilities.
Windows provides both, using its native WinRT file picker.

For the desktop image walkthrough, first copy `resource/images/app_logo.png` to
`/tmp/day-sketch-image-fixture.png`, then run
`day launch -p macos-appkit --script dayscript/images.yaml`. The screenshots verify
upright rendering, non-square resizing, rotation, opacity, and SQLite reopen. The
ordinary editor walkthrough remains `dayscript/demo.yaml`.

The browser counterpart is `scripts/web-image-check.mjs`: after a web build, run it
with `DAY_WEB_DRIVER_PLAYWRIGHT` pointing to a directory containing
`node_modules/playwright`. It uses a real file picker, waits for decoded canvas pixels,
edits the image, then reloads the page to verify restoration from SQLite in OPFS.

Use a Day CLI built from the same framework revision for web builds: its bundled
`shim.js` must include the new image decoder imports.

Hold **Shift** while dragging a canvas resize handle to preserve the starting aspect
ratio. You can press or release Shift during the drag. The opposite corner stays fixed,
including on rotated nodes; line endpoints keep their direction and text continues to
scale its font proportionally. Independent edge snapping pauses while Shift constrains
the resize.

Start dragging a selected node while holding **Option** on macOS or **Alt** on other
platforms to drag a copy. Multiple selected nodes and groups are copied together,
without an initial offset. Releasing the modifier continues dragging the copies;
one Undo removes the copies and one Redo restores their final positions.


Copy/Paste now exchanges image bytes through the system clipboard. A single copied image
also offers PNG (converting when needed), and editable SVG retains its original bytes and
node properties for a Day-Sketch round trip. Pasting a screenshot or other supported raster
image creates a new embedded Image node. Cut removes nodes only after clipboard publication
succeeds. Text fields keep their normal text clipboard behavior.

Run `dayscript/clipboard-images.yaml` with the image fixture staged as described above, or
`scripts/web-clipboard-check.mjs` for a real browser clipboard/persistence check. The native
binary probe lives in `day/parts/day-part-clipboard/examples/binary_clipboard.rs`.
