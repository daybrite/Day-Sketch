# Day Sketch

A vector drawing editor with drag handles, layers, and unlimited undo, built with
[Day](https://daybrite.dev) in one Rust codebase and rendered with the platform's own widgets on
Mac, iPhone, Android, Windows, Linux, HarmonyOS, and the web. Every drawing is a plain SQLite file
you can copy, share, and inspect.

<p align="center">
  <img src="https://daybrite.github.io/Day-Sketch/gallery/macos-appkit/en/editor.png" width="760" alt="The editor on macOS">
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

Place rectangles and ovals, drag them around, resize them by their handles, turn them, group them,
and arrange the layers. Every operation is one undoable turn, fronted by the platform's own undo
system where it has one, so ⌘Z on a Mac and the shake gesture on an iPhone both work the way
their platform expects.

<p align="center">
  <img src="https://daybrite.github.io/Day-Sketch/gallery/ios-uikit/en/editor.png" width="200" alt="The editor on iPhone">
  <img src="https://daybrite.github.io/Day-Sketch/gallery/ios-uikit/en/palette.png" width="200" alt="The palette on iPhone">
  <img src="https://daybrite.github.io/Day-Sketch/gallery/android-mdc/en/rotation-fan.png" width="200" alt="A fan of rotated shapes on Android">
  <img src="https://daybrite.github.io/Day-Sketch/gallery/android-mdc/en/translucency.png" width="200" alt="Three translucent circles on Android">
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
| <img src="https://daybrite.github.io/Day-Sketch/gallery/windows-xaml/en/editor.png" width="300" alt="The editor on Windows"> | <img src="https://daybrite.github.io/Day-Sketch/gallery/linux-gtk/en/editor.png" width="300" alt="The editor on GTK"> | <img src="https://daybrite.github.io/Day-Sketch/gallery/linux-qt/en/editor.png" width="300" alt="The editor on Qt"> |

| Web · DOM | macOS · outlines | macOS · a group, turned |
|:---:|:---:|:---:|
| <img src="https://daybrite.github.io/Day-Sketch/gallery/web-dom/en/editor.png" width="300" alt="The editor in the browser"> | <img src="https://daybrite.github.io/Day-Sketch/gallery/macos-appkit/en/outlines.png" width="300" alt="Outline-only shapes on macOS"> | <img src="https://daybrite.github.io/Day-Sketch/gallery/macos-appkit/en/group-turned.png" width="300" alt="A grouped body rotated as one on macOS"> |

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
