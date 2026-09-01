# Day Sketch

A [Day](https://daybrite.dev) app: one Rust codebase, native widgets on every platform.

## Run it

`day launch --git` clones this repo, builds it for your desktop, and runs it — no checkout needed:

```sh
cargo install day-cli
day doctor                                                  # what's installed, what's missing
day launch --git https://github.com/daybrite/Day-Sketch.git
```

`day doctor` prints the fix for anything it can't find. `day launch --git` prints where it put the
checkout, so you can `cd` there and edit the code.

From inside a clone, name a target instead. Day compiles **exactly one backend per binary**, and
the Day CLI supplies the right feature for each:

```sh
day launch -p macos-appkit   # build + run
day build  -p macos-appkit   # build only
```

Targets live in `Day.toml`. A bare `cargo build` uses this crate's default `mock` backend, which is
what lets rust-analyzer and `cargo check` work with no flags. To pick a real one from plain cargo,
turn the default off as well — otherwise `mock` and your choice are both on, which is two backends
and a compile error:

```sh
cargo build --no-default-features --features appkit    # or gtk / qt / uikit / mdc / xaml / dom
```

## What's inside

- `src/lib.rs` — the UI (`root()`), shared across every platform: a typed-route sidebar
  ([navigation](https://daybrite.dev/docs/navigation)) over four sample panels.
- `src/pages/home.rs` — signals in one glance: the reactive counter.
- `src/pages/controls.rs` — two-way bindings: toggle, slider, text field.
- `src/pages/canvas.rs` — a reactive display list drawn natively.
- `src/pages/items.rs` — a drill-down stack with data-carrying typed routes.
- `resource/locales/en/app.ftl` — every user-facing string ([localization](https://daybrite.dev/docs/localization)).
- `dayscript/demo.yaml` — a [dayscript](https://daybrite.dev/docs/dayscript) walkthrough that
  drives every feature this app ships, and doubles as its UI test:
  `day launch -p macos-appkit --script dayscript/demo.yaml`.
- `platform/` — the thin native host projects (Xcode / Gradle / hvigor) the mobile targets
  build through; `day build` keeps their identity in sync with `Day.toml`.
- `Day.toml` — app metadata + the target list.

`day lint` checks routes, element ids, and locale coverage.
