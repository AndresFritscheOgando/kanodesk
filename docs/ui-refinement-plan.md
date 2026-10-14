# UI refinement plan

**Status (2026-10-13): carried out**, all six phases; see `PROGRESS.md`
("UI refinement"). Where the build differs from this plan:

- Fonts: egui's defaults are kept (decision 2's fallback); Inter was not
  bundled.
- No `egui_kittest` (decision 5): every decision is tested in the models.
- Connection manager: option A. The expiry bar uses the session's
  `started_unix_ms` (the host sets it when the request arrives), so the
  model needed no new field.
- Recent rows have hover but no double-click to connect (a click target
  over the row would take clicks from its buttons).
- The plan's "✓" glyph is not in egui's bundled fonts; the app uses "✔".
- Not seen with a live session yet: the streaming toolbar, fullscreen and
  the password dialog (see `PROGRESS.md`, Known issues).

How to carry out each finding of the UI audit of 2026-10-13: the main window
(`kanodesk`), the viewer (`kanodesk --connect ID`) and the connection
manager (`kanodesk --cm`). The audit ran the release build on Windows 11 and
captured Home, Settings, an invalid ID and the viewer's connecting and
failure dialogs. The connection manager and a live streaming session were
reviewed from the code only.

The plan keeps the project's rules: decisions and texts live in the `*_model.rs`
files so they can be unit-tested, windows only draw; every phase ends with
`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`
and `cargo test --workspace` passing, a look at the real windows, and an
update to `PROGRESS.md`.

## Contents

- [Phases at a glance](#phases-at-a-glance)
- [Decisions to take first](#decisions-to-take-first)
- [Phase 1: Foundation (theme, shared widgets, icon)](#phase-1-foundation) covers finding 8
- [Phase 2: Truthful status and errors](#phase-2-truthful-status-and-errors) covers findings 1, 2, 3 and 7
- [Phase 3: Home and the connection manager](#phase-3-home-and-the-connection-manager) covers findings 6 and 9
- [Phase 4: Settings](#phase-4-settings) covers findings 4 and 5
- [Phase 5: Keyboard and screen readers](#phase-5-keyboard-and-screen-readers) covers findings 10 and 12
- [Phase 6: The streaming view](#phase-6-the-streaming-view) covers finding 11
- [How to check the windows by hand](#how-to-check-the-windows-by-hand)
- [Risks](#risks)

## Phases at a glance

| Phase | Findings | Size | Touches the wire or IPC? |
|---|---|---|---|
| 1. Foundation | 8 | M | No |
| 2. Truthful status and errors | 1, 2, 3, 7 | M | No (host-side change only, same IPC messages) |
| 3. Home and connection manager | 6, 9 | M | No (option B of 9 would bump `CM_PROTOCOL` and `CONTROL_PROTOCOL`) |
| 4. Settings | 4, 5 | M | No |
| 5. Keyboard and screen readers | 10, 12 | S | No |
| 6. Streaming view | 11 | L | No |

Phase 1 comes first because every later phase uses its primary and danger
buttons, status dot, card frame and copy button. Phase 2 is the most
valuable for users; if time is short, do phase 2 alone with plain buttons and
restyle later. Each phase is one commit (`feat(desktop): …` / `fix(host): …`).

## Decisions to take first

These change what gets built. Each has a recommendation; the plan assumes it.

1. **Accent colour.** Recommended: a deep teal, `#1F7A70` for filled buttons
   (white text 5.1:1) and `#4FD1C1` for accent text on dark backgrounds. Any
   other hue works if it passes the contrast tests in phase 1.
2. **Font.** egui's default proportional font is Ubuntu-Light and has no bold
   weight, which is why headings look thin and grey. Recommended: bundle
   **Inter** Regular and SemiBold (SIL Open Font License, about 600 KB; ship
   `OFL.txt` next to the licences). Fallback: keep the default font and
   build hierarchy from size and colour only.
3. **Connection manager header** (finding 9). Recommended: **option A**,
   drop the device-key line (no protocol change). Option B adds the device ID
   to `HostStatus`, which changes both IPC protocols.
4. **Opening Settings from the viewer** (finding 2). Recommended: a new
   `kanodesk --settings` role that opens the main window on Settings, and
   accept that a second main window may open. Making the main window
   single-instance is a separate, later task.
5. **UI interaction tests.** Recommended: add `egui_kittest` (same 0.36
   line as egui) as a dev-dependency of `kanodesk-desktop` for a few
   interaction tests driven through AccessKit labels (no image snapshots, so
   CI needs no GPU). Without it, keep all logic in the models and test there.
6. **Fullscreen shortcut** (finding 11). Recommended: `Ctrl+Alt+Shift+F`,
   matching the existing `+M` and `+K` shortcuts. Plain F11 would be taken
   from the remote side.

---

## Phase 1: Foundation

### Finding 8: no identity, default theme, small targets

**Seen:** the main window shows egui's logo and the viewer winit's generic
icon; errors are pure red (`#FF0000`); the accent is egui's default blue;
buttons are about 20 px tall; headings are the same light grey as body text.

**Goal:** one small design system shared by all three windows, with a real
icon, readable sizes and colours that pass WCAG AA.

#### 1.1 Add `apps/desktop/src/theme.rs`

Public items (all documented, `missing_docs` is on):

```rust
/// Colour tokens for one theme.
pub struct Palette {
    pub bg: Color32,          // window background
    pub surface: Color32,     // cards, panels
    pub surface_raised: Color32, // popups, modals
    pub border: Color32,
    pub text: Color32,
    pub text_weak: Color32,
    pub accent: Color32,      // filled primary buttons
    pub accent_text: Color32, // accent-coloured text, selection, links
    pub on_accent: Color32,   // text on accent fill
    pub success: Color32,
    pub warn: Color32,
    pub danger: Color32,      // filled danger buttons
    pub danger_text: Color32, // error text
}

pub const DARK: Palette = Palette { /* values below */ };
pub const LIGHT: Palette = Palette { /* values below */ };

/// Installs fonts, text styles, spacing and both palettes on `ctx`.
pub fn install(ctx: &egui::Context);

/// The palette `ui` currently draws with (dark or light).
pub fn palette(ui: &egui::Ui) -> &'static Palette;

/// WCAG contrast ratio of two colours (1.0 to 21.0).
pub fn contrast(a: Color32, b: Color32) -> f32;
```

Starting values (dark / light):

| Token | Dark | Light |
|---|---|---|
| `bg` | `#141618` | `#F6F7F8` |
| `surface` | `#1C1F22` | `#FFFFFF` |
| `surface_raised` | `#24282C` | `#FFFFFF` |
| `border` | `#2E3338` | `#D9DEE3` |
| `text` | `#E8EAED` | `#1A1D21` |
| `text_weak` | `#9AA3AD` | `#5B6670` |
| `accent` | `#1F7A70` | `#1F7A70` |
| `accent_text` | `#4FD1C1` | `#17665E` |
| `on_accent` | `#FFFFFF` | `#FFFFFF` |
| `success` | `#3FB950` | `#1A7F37` |
| `warn` | `#F0A63A` | `#9A6700` |
| `danger` | `#C93035` | `#C93035` |
| `danger_text` | `#FF7B72` | `#B42318` |

`install` does:

- `ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::System)`.
- For each of `Theme::Dark` and `Theme::Light`, `ctx.set_style_of(theme, style)`
  where `style` starts from `Style::default()` and sets:
  - `visuals`: `panel_fill = bg`, `window_fill = surface_raised`,
    `extreme_bg_color = bg` (text-edit background, darker than the card it
    sits on), `faint_bg_color = surface`, `selection.bg_fill = accent`,
    `selection.stroke.color = on_accent`, `hyperlink_color = accent_text`,
    `error_fg_color = danger_text`, `warn_fg_color = warn`,
    `window_stroke.color = border`, `widgets.noninteractive.fg_stroke.color = text`,
    `widgets.noninteractive.bg_stroke.color = border`, and
    `widgets.{inactive,hovered,active}.weak_bg_fill` a step lighter than
    `surface` (buttons use `weak_bg_fill`). Corner radius 6 for widgets, 10
    for windows and modals.
  - `spacing`: `item_spacing = vec2(8.0, 8.0)`, `button_padding = vec2(12.0, 5.0)`,
    `interact_size.y = 28.0`, `menu_margin` and `window_margin` 12.
  - `text_styles`: `Small 12`, `Body 14`, `Button 14`, `Monospace 14`,
    `Heading 19`, plus two named styles: `TextStyle::Name("display".into())`
    at 30 (the device ID) and `TextStyle::Name("display_small".into())` at
    24 (the one-time password). Both named styles use the monospace family.
- If decision 2 is Inter: `ctx.set_fonts(...)` with `include_bytes!` of the
  two TTF files from `apps/desktop/assets/fonts/`, Inter Regular first in
  `Proportional`, and a second family `FontFamily::Name("strong".into())`
  with Inter SemiBold for headings and the `strong()` helper below.
  egui's `strong()` only changes colour; with this family it changes weight.

Call `theme::install` in three places:

- `main_window::run`, inside the `Box::new(move |cc| …)` closure, before the
  app is built: `crate::theme::install(&cc.egui_ctx);`
- `cm::run`, in the same spot.
- `viewer::ViewerApp::new`, right after `egui::Context::default()`.

Remove the per-call `.size(28.0)` and `.size(22.0)` in
`main_window.rs` once the named styles exist.

**Tests (in `theme.rs`):**

- `contrast` gives 21.0 for black on white and 1.0 for equal colours.
- For both palettes: `text` on `bg` and on `surface` ≥ 7.0; `text_weak` on
  `surface` ≥ 4.5; `on_accent` on `accent` ≥ 4.5; `on_accent` on `danger`
  ≥ 4.5; `danger_text`, `warn`, `success` and `accent_text` on `surface` ≥ 4.5.
  If a value fails, change the value, not the test.
- `install` on a fresh `Context` sets the named text styles (look them up
  in `ctx.style_of(Theme::Dark).text_styles`).

#### 1.2 Add `apps/desktop/src/widgets.rs`

Small helpers so every window looks the same. Each takes `&mut egui::Ui`
and returns a `Response` (or what `egui` returns), so callers keep using
`.clicked()` and `.on_hover_text()`.

| Helper | What it draws |
|---|---|
| `primary_button(ui, text)` | `Button::new(RichText::new(text).color(on_accent)).fill(accent)`, min height 28 |
| `danger_button(ui, text)` | same with `danger` fill |
| `quiet_button(ui, text)` | `Button::new(text).frame(false)` for low-weight actions (✕ remove) |
| `card(ui, add_contents)` | `Frame::new().fill(surface).stroke(border).corner_radius(10).inner_margin(16)`, full available width |
| `section_title(ui, text)` | `Heading` text in `strong` family and `text` colour (not grey) |
| `field_label(ui, text)` | `Small` text in `text_weak`, used above values ("Your ID") |
| `status_dot(ui, level)` | a 8 px circle in `success` / `warn` / `danger_text` / `text_weak` painted with `ui.painter().circle_filled`, then the label |
| `copy_button(ui, id_salt, value)` | "Copy"; after a click shows "Copied ✓" in `success` for 1.5 s; see below |
| `banner(ui, level, text)` | a tinted full-width frame (warn or danger tint at 15% alpha) with the text, for "2 remote sessions connected" and "Not connected to the host" |

`copy_button` keeps its "copied at" time in egui memory, so no caller needs
a field:

```rust
pub fn copy_button(ui: &mut egui::Ui, id_salt: &str, value: &str) -> egui::Response {
    let id = ui.id().with(("copy", id_salt));
    let now = ui.input(|i| i.time);
    let copied_at: Option<f64> = ui.data(|d| d.get_temp(id));
    let recent = copied_at.is_some_and(|t| now - t < 1.5);
    let label = if recent { "Copied ✓" } else { "Copy" };
    let response = ui.button(label);
    if response.clicked() {
        ui.ctx().copy_text(value.to_owned());
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    if recent {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
    response
}
```

Keep the 1.5 s decision in a pure function (`fn copied_label(copied_at: Option<f64>, now: f64) -> &'static str`)
and unit-test it: `None` → "Copy", 0.5 s later → "Copied ✓", 2 s later → "Copy".

Wire both modules into `lib.rs` (`mod theme; mod widgets;`) and add the
docs reference to the module comments (`theme.rs`: "15 §2").

#### 1.3 Window and executable icon

1. Add `apps/desktop/assets/kanodesk-256.png` (square, transparent
   background, readable at 16 px: a simple monitor outline with a
   key-shaped cutout, or the letter K on the accent colour) and
   `kanodesk.ico` (16, 24, 32, 48, 64, 256) generated from it.
2. `apps/desktop/src/icon.rs`:
   ```rust
   const PNG: &[u8] = include_bytes!("../assets/kanodesk-256.png");
   /// The app icon for eframe windows.
   pub fn egui_icon() -> egui::IconData {
       eframe::icon_data::from_png_bytes(PNG).expect("the bundled icon is a valid PNG")
   }
   /// The app icon for the viewer's winit window.
   pub fn winit_icon() -> Option<winit::window::Icon> {
       let i = egui_icon();
       winit::window::Icon::from_rgba(i.rgba, i.width, i.height).ok()
   }
   ```
   `eframe::icon_data` is available on desktop targets without new features.
3. Main window and CM: `.with_icon(std::sync::Arc::new(crate::icon::egui_icon()))`
   on their `ViewportBuilder`.
4. Viewer: `.with_window_icon(crate::icon::winit_icon())` on
   `Window::default_attributes()` in `resumed`.
5. Executable icon (Explorer, taskbar pin, installer): add
   `embed-resource` (or `winresource`) to `[workspace.dependencies]`, then
   as a `[target.'cfg(windows)'.build-dependencies]` of `kanodesk-desktop`,
   and a `build.rs` that compiles `assets/kanodesk.rc`
   (`1 ICON "kanodesk.ico"`) on Windows only. Linux builds skip it.

**Tests:** `icon.rs` test that `egui_icon()` decodes, is square and at least
64 px, and that `winit_icon()` is `Some`.

**Check by hand:** both windows and the taskbar show the icon; dark and
light Windows themes both look right (Settings → Personalisation → Colours).

**Done when:** all three windows use the theme, the icon shows everywhere,
the contrast tests pass, and the screenshots of Home, Settings and the
viewer dialogs show the new buttons and colours.

---

## Phase 2: Truthful status and errors

### Finding 1: "Connecting to the ID server…" hides failures

**Seen:** with the ID server down, Home showed a grey "Connecting to the ID
server…" in every capture over 40 s while `host.log` logged `timed out`
every 32–34 s.

**Cause:** in `crates/host/src/presence.rs` the retry loop sets
`IdServerState::Connecting` at the start of each attempt. An attempt takes
up to 30 s to time out and the pause before the next one starts at 2 s, so
`Failed` is visible only during the pause.

#### Host change (no IPC change)

1. Add a pure function next to `set_state`:
   ```rust
   /// The state to show when an attempt starts: a failure stays visible
   /// (the main window says "retrying") until an attempt succeeds.
   fn attempt_state(previous: &IdServerState) -> Option<IdServerState> {
       match previous {
           IdServerState::Failed(_) => None,
           _ => Some(IdServerState::Connecting),
       }
   }
   ```
2. In `run`, replace the first `set_state(&tx, IdServerState::Connecting);`
   in the loop with:
   ```rust
   let previous = tx.borrow().state.clone();
   if let Some(s) = attempt_state(&previous) {
       set_state(&tx, s);
   }
   ```
   Keep the `Connecting` after `Ok(())` (a dropped link that was online
   really is reconnecting).
3. Update the module comment ("When the ID server connection drops or an
   attempt fails…") to say the failure stays reported while retrying.

**Tests (presence.rs):** `attempt_state(Failed)` is `None`;
`attempt_state(Connecting)`, `(Online)` and `(NotConfigured)` give
`Some(Connecting)`.

#### Plain words for raw errors (shared with finding 2)

Raw errors such as `transport: connection: timed out` reach the user in
the main window and the viewer. Add one function both use, in
`crates/client/src/session.rs` next to `EndReason::notice`:

```rust
/// A raw network error in plain words: "it did not answer", "the address
/// could not be found". Falls back to "an error occurred (see details)".
pub fn plain_cause(raw: &str) -> &'static str
```

Build the table from the real strings, not guesses: grep the `Display`
impls and `map_err(|e| e.to_string())` sites in `crates/transport`,
`crates/rendezvous` and `crates/client` and quinn's/tokio's error texts for
timeouts, refused connections, DNS failures and unreachable networks.
Starting table (match lowercase substrings, first hit wins):

| Raw text contains | Plain cause |
|---|---|
| `timed out`, `timeout` | it did not answer |
| `refused` | it refused the connection |
| `lookup`, `resolve`, `no such host`, `dns`, `name or service` | the address could not be found |
| `unreachable`, `no route` | this computer has no route to it (check the network) |
| `key`, `certificate`, `handshake` | the secure connection could not be set up |
| anything else | an error occurred (see details) |

**Tests:** one case per real string found by the grep, plus the fallback.

#### Main window status line

1. In `main_model.rs`, replace `link_text(&HostOverview) -> (String, bool)`
   with:
   ```rust
   /// How bad a status is: drives the dot's colour.
   pub enum Level { Ok, Busy, Problem }

   /// The ID-server status as the main window shows it.
   pub struct LinkStatus {
       pub level: Level,
       /// Next to the dot: "Online", "Connecting…", "Offline", "Not set up".
       pub short: &'static str,
       /// The sentence under the ID.
       pub text: String,
       /// The raw error, for the tooltip.
       pub detail: Option<String>,
       /// Whether to offer "Open Settings".
       pub settings: bool,
   }

   pub fn link_status(o: &HostOverview) -> LinkStatus
   ```
   With `server = o.id_server_address.as_deref().unwrap_or("the ID server")`:

   | State | level | short | text | settings |
   |---|---|---|---|---|
   | `Online` | Ok | Online | Others can reach this desk by its ID. | no |
   | `Connecting` | Busy | Connecting… | Connecting to {server}… | no |
   | `NotConfigured` | Problem | Not set up | Set an ID server in Settings so others can reach this desk. | yes |
   | `Failed(e)` | Problem | Offline | Can't reach {server}: {plain_cause(e)}. Retrying automatically. | yes |

   `detail` is `Some(e)` for `Failed`.
2. Do the same for `viewer_link_text` (viewer-only Linux build): return a
   `LinkStatus` (Ok with "ID server: {server}", or Problem with settings).
3. In `main_window::this_desk`, draw `widgets::status_dot` with `short`
   in the card header (phase 3 places it), the sentence under the ID in
   `text_weak` (Ok/Busy) or `warn` (Problem), the raw error on hover
   (`.on_hover_text(detail)`), and when `settings` is true a
   `quiet_button("Open Settings")` that switches `model.page` to
   `Page::Settings` (same code path as the tab).

**Tests (main_model.rs):** one per row of the table, including that the
address appears and that `Failed("transport: connection: timed out")`
gives "did not answer".

**Check by hand:** point Settings at an address with nothing listening;
within one attempt (about 30 s) the dot turns red and stays red across
retries; start `kanodesk-idserver` and it turns green.

### Finding 2: the viewer's failure path

**Seen:** 30 s on "Contacting the ID server…" with no sign of progress,
then *"Could not use the ID server: transport: connection: timed out. Check
the address in Settings and your network."* under a pure-red title, with
no way to open Settings.

#### 2.1 Plain notice text with details

In `crates/client/src/session.rs`:

1. Add two fields to `Notice`:
   ```rust
   /// The raw error, shown under "Details" (None when the text says it all).
   pub detail: Option<String>,
   /// An extra action the dialog offers.
   pub action: Option<NoticeAction>,
   ```
   and `pub enum NoticeAction { OpenSettings }`.
2. Add `EndReason::NoIdServer` for "no ID server is set" (today the viewer
   builds `EndReason::IdServer(problem)` and the message reads "Could not
   use the ID server: no ID server is set…"). Notice: title "No ID server
   set", text "Set the ID server address and key in Settings, then try
   again.", `retry: false`, action `OpenSettings`. In `viewer::Setup::load`
   keep `problem` for a bad key but use the new variant when address or key
   is empty (add a `missing_server: bool` to `Setup`).
3. Rewrite the notices that embed raw errors, moving the raw text to
   `detail`:

   | Reason | Title | Text | detail | action |
   |---|---|---|---|---|
   | `IdServer(e)` | Can't reach the ID server | The ID server {plain_cause(e)}. Check the address in Settings and that this computer is online. | `e` | OpenSettings |
   | `ServerNotTrusted` | Wrong ID server key | (unchanged) | – | OpenSettings |
   | `Unreachable(e)` | Can't reach the device | Neither a direct nor a relayed connection worked: {plain_cause(e)}. | `e` | – |
   | `ConnectionLost(e)` | Connection lost | The connection to the device broke: {plain_cause(e)}. | `e` | – |
   | `Incompatible(e)` | Incompatible versions | This KanoDesk and the remote one cannot talk to each other. Update one of them. | `e` | – |
   | `Failed(e)` | Connection failed | Something went wrong while connecting. | `e` | – |

   The `notice()` helper gains `detail: None, action: None` defaults.
4. Update the session tests that check notices (around line 990: the
   `all` list) to also assert that no `text` contains `transport:` or
   `connection:` and that every reason with a raw string has `detail`.

#### 2.2 The end dialog

In `viewer.rs`, `Phase::Ended`:

- Title in `text` colour with a small `danger_text` "⚠" glyph before it
  when `notice.error`, instead of the whole heading in red.
- `notice.text`, then a weak line "ID server: {address}" when the reason is
  an ID-server one (the viewer's `Setup.server` holds the address; pass it
  into `ViewerModel` as `pub server: Option<String>` in `ViewerModel::new`).
- When `detail` is set: `egui::CollapsingHeader::new("Details").default_open(false)`
  showing the raw text in `Monospace`, selectable (`egui::Label::new(..).selectable(true)`),
  and a `copy_button`.
- Buttons, left to right: primary first. If `retry`: `primary_button("Try again")`,
  then "Open Settings" (if action), then "Close". Otherwise "Open Settings"
  as primary if present, then "Close" (primary when it is the only one).
- Keys: Enter runs the primary button, Esc runs Close. Use
  `ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))`. Do not use
  `ModalResponse::should_close()`: it also closes on a backdrop click, and a
  stray click must not end the window.

"Open Settings" is `UiAction::OpenSettings`: spawn
`crate::paths::spawn_self(&["--settings"])` and keep the viewer open (the
user may want Try again after fixing it).

#### 2.3 The `--settings` role

- `lib.rs`: `Role::Main { page: Page }` (or a new `Role::Settings`);
  `parse_role(["--settings"])` gives it; `--settings x` is an error. Extend
  the `roles` test.
- `main_window::run(start: Page)` sets `model.page = start`, and the form is
  loaded as it is when switching to the Settings tab.
- `--help` text lists the new flag.

#### 2.4 Progress while connecting

1. `ViewerModel` gets `stage_since: Instant`, set in `new`/`restart` and
   whenever `SessionEvent::Stage` changes the stage (`apply` receives `now`).
2. Pure function in `viewer_model.rs`:
   ```rust
   /// A hint for a stage that takes long: None for the first 8 s.
   pub fn slow_hint(stage: Stage, waited: Duration, server: Option<&str>) -> Option<String>
   ```
   - `IdServer` after 8 s: "The ID server{ at server} has not answered yet. It may be offline or blocked by a firewall."
   - `Connecting` after 8 s: "Trying direct and relayed paths to the device…"
   - `WaitingForApproval`: never (the remote person may take time; the host
     has its own timeout).
   - `LoggingIn` after 8 s: "This is taking longer than usual."
3. The connecting modal shows the stage line with the elapsed seconds once
   past 3 s ("Contacting the ID server… 12 s"), then the hint in
   `text_weak`, then Cancel. Esc cancels. The modal repaints once a second
   while open (`ctx.request_repaint_after(Duration::from_secs(1))`).

**Tests (viewer_model.rs):** `slow_hint` before and after 8 s for each
stage, with and without the address; `stage_since` moves when the stage
changes and not when the same stage repeats.

### Finding 3: viewer toolbar active before there is a session

**Seen:** while connecting, Fit and Balanced are live and there is a
Disconnect button that does the same as the dialog's Cancel.

1. Pure function in `viewer_model.rs`:
   ```rust
   /// What the toolbar shows in the current phase.
   pub struct ToolbarSpec {
       pub controls: bool,      // display, view, quality, actions
       pub status: String,      // right of the ID
       pub end_button: Option<&'static str>, // "Disconnect" while streaming
   }
   pub fn toolbar(&self) -> ToolbarSpec
   ```
   - `Starting(stage)`: no controls, status = `stage_text(stage)` without
     the ellipsis, no end button (the modal has Cancel).
   - `Password(_)`: no controls, status "Password needed", no end button.
   - `Streaming`: controls, `status_text()`, end button "Disconnect".
   - `Ended(_)`: no controls, status "Ended", no end button (the modal has Close).
2. `viewer.rs` toolbar draws from the spec. `Disconnect` becomes a
   `danger_button`.

**Tests:** one per phase.

### Finding 7: stale error and a weak Connect box

**Seen:** "An ID is 9 digits…" stayed while the field was being fixed; it
is cleared only after a successful connect.

1. Pure functions in `main_model.rs`:
   ```rust
   /// Keeps up to 9 digits and groups them: "123456" -> "123 456".
   pub fn format_id_input(text: &str) -> String
   /// "6 of 9 digits" while incomplete, None when empty or complete.
   pub fn id_progress(text: &str) -> Option<String>
   ```
   `format_id_input` drops every non-digit (so pasted `123-456-789` and
   `ID: 123 456 789` work) and stops at 9 digits.
2. In `remote_desk`:
   - After the `TextEdit`, if `edit.changed()`: reformat
     `self.connect_text = format_id_input(&self.connect_text)`, clear
     `self.connect_error`, and move the cursor to the end
     (`TextEdit::load_state` / `state.cursor.set_char_range(...)` then
     `state.store`), because reformatting can shift it.
   - `let valid = parse_target(&self.connect_text).is_ok();`
     `Connect` is a `primary_button` added with `ui.add_enabled(valid, …)`;
     Enter in the field connects only when `valid`.
   - Under the field: the error in `danger_text` if set, else `id_progress`
     in `text_weak`.
   - The field uses `TextStyle::Name("display_small")`-sized monospace (or
     18 px) and fills the card width minus the button.
3. `open_viewer` errors ("Set the ID server and its key in Settings
   first.") get an "Open Settings" quiet button next to them.

**Tests:** `format_id_input` on digits, spaces, dashes, letters, more than
9 digits and empty; `id_progress` for 0, 4 and 9 digits. If
`egui_kittest` is adopted: typing "12345" leaves Connect disabled; typing
four more digits enables it; editing after an error clears the error.

**Phase 2 done when:** the four findings above are fixed, the tests pass,
and a run against an unreachable ID server shows a red "Offline" status in
the main window within one attempt and a plain-language viewer error with
Details, Try again and Open Settings.

---

## Phase 3: Home and the connection manager

### Finding 6: Home hierarchy

**Seen:** headings as grey as body text, the password greyer than the ID,
Copy with no feedback, the bottom half of the window empty, the main
button styled like every other button.

#### Layout

```
┌ KanoDesk  Home  Settings ─────────────────────────────── v0.1.0 ┐
│ ┌ This desk ───────────── ● Online ┐ ┌ Control a remote desk ──┐ │
│ │ YOUR ID                          │ │ [ 123 456 789     ][Connect]│
│ │ 245 225 859            [Copy]    │ │ 6 of 9 digits             │ │
│ │ Others can reach this desk by ID │ │                           │ │
│ │                                  │ │ RECENT                    │ │
│ │ ONE-TIME PASSWORD                │ │ ┌ LAPTOP          [Connect]┐│
│ │ wim58rzm         [Copy] [↻ New]  │ │ │ 123 456 789 · 5 min ago ✕││
│ │ Works for one login.             │ │ └──────────────────────────┘│
│ │                                  │ │                           │ │
│ │ 🔒 Others connect with the password.                         │ │
│ │ ⚠ 2 remote sessions connected    │ │                           │ │
│ └──────────────────────────────────┘ └───────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

#### Steps (`main_window.rs`)

1. Top bar: keep it; make the tabs `selectable_label`s with `Body` size and
   the app name in the `strong` family. Panel `inner_margin` 8/12.
2. `CentralPanel` with `inner_margin` 16. `ui.columns(2, …)` stays, each
   column wrapped in `widgets::card`. Cards take the full column height
   (`ui.set_min_height(ui.available_height())`) so the empty space reads as
   card, not as nothing.
3. This desk card:
   - Header row: `section_title("This desk")` left; right-aligned
     `status_dot(level, short)` from `link_status` (phase 2).
   - `field_label("Your ID")`; the ID in `display` style, `text` colour;
     `copy_button` on the same row, vertically centred with
     `ui.with_layout(Layout::left_to_right(Align::Center), …)`.
   - The status sentence under it (phase 2).
   - `field_label("One-time password")`; password in `display_small`, same
     colour as the ID (not grey); `copy_button`; a quiet "↻ New" button
     with tooltip "Replace the one-time password now".
   - The approve text with a small leading glyph; "A permanent password is
     set too." in `text_weak`.
   - `active_sessions > 0`: `banner(warn, "2 remote sessions are connected to this desk")`.
   - Not connected states (spinner while starting, error when the host
     could not start) render inside the same card so the layout does not
     jump.
4. Remote card: phase 2's connect box at the top; `field_label("Recent")`;
   recent rows:
   - Each row is a `Frame` with hover fill (`ui.interact(rect, id, Sense::click())`
     then repaint the frame with `widgets.hovered.weak_bg_fill` when hovered).
   - Left: label in `strong`, then ID (monospace) · ago · path in `text_weak`.
   - Right: "Connect" (normal button); "✕" `quiet_button` shown only while
     the row is hovered, tooltip "Remove from the list".
   - Double-clicking the row connects.
   - Empty state: a short weak line plus a hint "Enter an ID above to
     connect." (already close to this).
5. Bottom reply bar (`reply` panel): use `banner` with ok → success tint,
   error → danger tint, and an ✕ to dismiss early.

**Tests:** labels and texts stay in `main_model.rs` and are covered by
phase 2's tests. If `egui_kittest` is adopted: clicking Copy shows
"Copied ✓", and the ✕ on a recent row removes it.

**Check by hand:** at 640 × 440 (the minimum) and at 1280 × 800, nothing
overlaps or clips; the ID and password are the two most visible things.

### Finding 9: connection manager

**Seen in code:** the header identifies "This computer" by the first 16
characters of the host's device key; Accept, Deny and Disconnect look the
same; the time left to answer is small "N s left" text.

#### Header

- Option A (recommended): remove the device-key line. The header becomes:
  the approve sentence, and the one-time password with a `copy_button` when
  there is one. When not connected: `banner(warn, "Not connected to the KanoDesk host. Retrying…")`.
- Option B: add `id: Option<String>` to `kanodesk_ipc::cm::HostStatus` (at
  the end), fill it from presence in the host (`engine.rs` `status()` needs
  the presence status passed in), show "This computer: 245 225 859", and
  bump both `CM_PROTOCOL` (2 → 3) and `CONTROL_PROTOCOL` (1 → 2) because
  `HostOverview` embeds `HostStatus`. Update the IPC round-trip tests.

#### Request card (`SessionState::AwaitingApproval`)

1. `card` with an accent-tinted border (2 px `accent_text`) so a request
   stands out from active sessions.
2. Title: "{describe(info)} wants to control this computer" in `strong`.
3. Sub-line in `text_weak`: "from {address} · {agent}" and, when a
   password was used, "· used the {p} password". The viewer's device key
   (`short_key`) goes in the tooltip of that line, labelled "Device
   fingerprint".
4. "Allow:" then the Keyboard and Mouse checkboxes.
5. Buttons: `primary_button("Accept")` (still disabled during the
   anti-spoof delay, label "Accept (2)" counting down), then
   `danger_button("Deny")`. No keyboard shortcut for Accept: the delay
   exists so nothing can accept the request by accident.
6. Expiry: a thin `egui::ProgressBar::new(fraction).desired_height(4.0)`
   under the buttons and "Ends without an answer in 23 s" in `text_weak`.

Model changes (`cm_model.rs`):

- `Entry` gets `wait_total_ms: Option<u64>`: on the first
  `AwaitingApproval` for an id, `expires_unix_ms - now_unix_ms` (the
  model's `apply` needs `now_unix_ms` as well as `now`; `cm.rs` has it).
- `pub fn expiry_fraction(entry: &Entry, now_unix_ms: u64) -> Option<f32>`:
  remaining / total, clamped to 0..=1, `None` when not waiting.

#### Active and ended cards

- Active: title `describe(info)`; "Connected 00:12:31 · {password text}"
  in `text_weak`; permission checkboxes; `danger_button("Disconnect")`
  right-aligned.
- Ended: whole card at 60% opacity (`ui.multiply_opacity(0.6)`), "Ended:
  {reason}".
- Order: requests first, then active, then ended (sort in `entries()` or
  in the view).

**Tests (cm_model.rs):** `expiry_fraction` at the start (1.0), halfway
(0.5), after expiry (0.0) and for an active session (`None`); the total is
taken once and does not reset when the host re-sends the same session;
ordering puts requests first.

**Check by hand:** `kanodesk --server --approve-mode click`, connect with
`spike-viewer --ask-host` (README, "Host and connection manager"); the
request stands out, Accept counts down, the bar shrinks, Deny is red,
Disconnect is red, the window still comes to the front.

---

## Phase 4: Settings

### Finding 4: misaligned labels and a cut-off key

**Seen:** three grids give three label column widths (about 88, 182 and
115 px); the 64-character server key is cut off at 360 px.

1. A constant `LABEL_WIDTH: f32 = 180.0` in `main_window.rs` and
   `.min_col_width(LABEL_WIDTH)` on every settings `Grid` (Network,
   Security, Viewer). The value column gets `.max_col_width(...)` so long
   fields do not push the window.
2. Constrain the page: inside the `ScrollArea`, `ui.set_max_width(680.0)`
   so lines stay readable on wide windows.
3. Each section (`Network`, `Security`, `Permanent password`, `Viewer`) is a
   `card` with `section_title`, one grid, and 16 px between cards.
4. Server key field: `TextEdit::multiline(&mut self.form.server_key)`
   with `desired_rows(2)`, monospace, `desired_width(f32::INFINITY)`; 32
   characters per line fit at the minimum window width. Hint text "64 hex
   characters, printed by kanodesk-idserver".
5. Live hint under the key, from a pure function in `main_model.rs`:
   ```rust
   pub enum KeyHint { Empty, Counting(usize), Invalid, Valid }
   pub fn key_hint(text: &str) -> KeyHint
   ```
   It ignores whitespace and newlines. Shows "✓ Valid key" (`success`),
   "48 of 64 characters" (`text_weak`) or "Only 0–9 and a–f are allowed"
   (`danger_text`).
6. `SettingsForm::apply` must remove internal whitespace from the key
   (`key.split_whitespace().collect::<String>()`), because the multiline
   field and pasted keys can contain line breaks. Add a test.
7. ID server field: live hint "Port 21116 is used when none is given" in
   `text_weak`; the existing "cannot contain spaces" check also shows live.
8. Locked fields (administrator): keep them disabled and add a 🔒 glyph
   with tooltip "Set by your administrator".

**Tests:** `key_hint` for empty, 10 valid chars, 64 valid chars, 64 chars
with spaces and a newline, and a non-hex character; `apply` accepts a key
split over two lines.

### Finding 5: inconsistent saving

**Seen in code:** leaving Settings and coming back reloads the form and
silently drops edits (`main_window.rs`, where `new_page == Page::Settings`
reloads); the permanent password Set and Remove act at once while the rest
waits for Save; Remove is always enabled and does not ask.

#### Unsaved changes

1. `MainApp` keeps `saved_form: SettingsForm` (the form as loaded or last
   saved). Pure method on the form:
   ```rust
   /// Whether the saved fields differ (the password fields do not count:
   /// they are applied on their own).
   pub fn differs_from(&self, saved: &SettingsForm) -> bool
   ```
2. Entering Settings reloads the form **only when it is not dirty**.
3. A footer on the Settings page only: `egui::Panel::bottom("settings_footer")`
   with "Unsaved changes" (`warn`) on the left when dirty, and on the right
   `Revert` (enabled when dirty) and `primary_button("Save")` (enabled
   when dirty). The footer replaces the buttons at the end of the scroll
   area so Save is always visible.
4. Leaving with unsaved changes (clicking Home) opens a modal: "Save your
   changes?" with `primary_button("Save")`, "Discard" and "Cancel". Save
   runs `save_settings` and only switches page if it succeeded.
5. Closing the window with unsaved changes: in `ui`, check
   `ctx.input(|i| i.viewport().close_requested())`; if dirty, send
   `ViewportCommand::CancelClose` and open the same modal; its Save and
   Discard then send `ViewportCommand::Close`.
6. After a successful save, `saved_form = form.clone()` and show the
   "Settings saved." reply (already sent by the host or noted locally).

#### Permanent password card

1. Pass the overview's `has_permanent_password` into `settings` (the
   snapshot already exists in `ui`).
2. The card shows the current state: "● Set" (`success`) or "Not set"
   (`text_weak`), and the line "Applies right away; it does not wait for
   Save." so the two saving models are explained, not hidden.
3. Fields as today; Enter in the second field submits. Button text: "Set
   password" when none is set, "Change password" when one is.
4. "Remove" is a `danger_button`, enabled only when one is set, and opens a
   confirm modal: "Remove the permanent password? Only the one-time
   password (or your acceptance) will let people in." with
   `danger_button("Remove")` and "Cancel".
5. The host's reply (ok or error) already comes back as `HostToUi::Reply`;
   clear `password_problem` when an ok reply arrives after a Set.

**Tests:** `differs_from` ignores the password fields and notices a
changed approve mode; with `egui_kittest`: Save is disabled until a field
changes; Remove is disabled with `has_permanent_password = false`.

**Check by hand:** change the approve mode, click Home: the modal appears;
Discard really discards; Save really saves (check `kanodesk.toml`); close
the window with changes: the modal appears and the window closes after
Save or Discard.

---

## Phase 5: Keyboard and screen readers

### Finding 10: password dialog traps focus

**Seen in code:** `edit.request_focus()` runs every frame
(`viewer.rs`, password modal), so Tab can never reach Connect or Cancel,
and Esc does nothing.

1. Focus once: `if !self.password_focused { edit.request_focus(); self.password_focused = true; }`,
   with `password_focused: bool` on `ViewerApp` reset to `false` whenever
   the phase changes into `Password` (in `on_session_event`, compare the
   phase before and after `model.apply`). On a wrong password, focus the
   field again (reset the flag on `PasswordPrompt::Wrong`).
2. Esc cancels (`consume_key(Modifiers::NONE, Key::Escape)` → `UiAction::Close`).
3. Enter submits only when the field is not empty (as today).
4. Add a "Show" toggle (`quiet_button("👁")`) that flips `.password(...)`
   for this frame; the state lives in egui memory.
5. Apply the same rule to the main window's permanent password fields
   (they do not steal focus today; check Tab order: new → again → Set →
   Remove).

**Check by hand:** with only the keyboard: Tab moves field → Show →
Connect → Cancel; Esc closes; Enter submits; after a wrong password the
cursor is back in the empty field.

### Finding 12: the viewer is invisible to screen readers

**Seen:** `eframe` enables `accesskit` for the main window and CM, but the
viewer uses `egui-winit` with `default-features = false` and only
`clipboard` and `links` (`Cargo.toml`, workspace dependencies).

1. In `apps/desktop/Cargo.toml`, turn on the feature for this crate only:
   `egui-winit = { workspace = true, features = ["accesskit"] }` (and keep
   the Linux `x11`/`wayland` lines, adding `accesskit` there too).
2. `UserEvent` gets `AccessKit(egui_winit::accesskit_winit::Event)` and
   `impl From<accesskit_winit::Event> for UserEvent`.
3. AccessKit requires the window to be created hidden and shown after the
   adapter exists. In `resumed`: `.with_visible(false)` on the attributes,
   create the `egui_winit::State`, call
   `state.init_accesskit(event_loop, &window, self.proxy.clone())`, then
   `window.set_visible(true)`.
4. In `user_event`, handle `UserEvent::AccessKit(e)` by its
   `window_event`: initial tree requested → `self.egui_ctx.enable_accesskit()`
   and redraw; action requested → `state.on_accesskit_action_request(request)`
   and redraw; deactivated → nothing. Check the variant names against the
   `accesskit_winit` version `egui-winit 0.36.2` pulls in.
5. `on_window_event` already forwards window events to the adapter.
6. Give icon-only buttons (✕, ↻, 👁, ⋯) accessible names:
   `.on_hover_text(...)` alone is not a name; use
   `response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Remove from the list"))`.
7. Linux: `accesskit_winit` uses `accesskit_unix` (D-Bus over `zbus`, pure
   Rust); rebuild with `packaging/linux/build.sh check` to make sure it
   links in the Debian image.

**Check by hand:** Windows Narrator (Ctrl+Win+Enter) reads the connecting
dialog, the password dialog and the end dialog's text and buttons.

---

## Phase 6: The streaming view

### Finding 11: fixed toolbar, no fullscreen, hidden shortcuts

**Seen in code:** the 34 pt toolbar is always drawn and the video is laid
out below it; there is no fullscreen; toasts are drawn inline in the
toolbar and get squeezed at the 480 px minimum width; relative mouse and
Translate keyboard are only reachable by shortcuts.

#### 6.1 One command path for toolbar and shortcuts

1. Add `ViewerCommand::ToggleFullscreen` in `kanodesk_client::input`, bound
   to `Ctrl+Alt+Shift+F` the same way `+M` and `+K` are matched (with a
   test in that crate: the chord yields the command and sends no key
   events to the host).
2. In `viewer.rs`, move the body of `handle_key`'s `match command` into
   `fn run_command(&mut self, event_loop, command: ViewerCommand, now)`.
   `handle_key` calls it; new `UiAction::Command(ViewerCommand)` calls it
   too. Fullscreen:
   `window.set_fullscreen(if on { Some(Fullscreen::Borderless(None)) } else { None })`,
   then `update_view()`.

#### 6.2 Toolbar layout

From left to right while streaming:

- The ID (strong), then the host name from `remote.hostname` in `text_weak`.
- Display picker (as today), view and quality combos.
- An actions menu button "⋯" (`egui::containers::menu::MenuButton` or
  `ui.menu_button`) with:
  - Send Ctrl+Alt+Del — shortcut hint "Ctrl+Alt+End" on the right;
  - Keyboard: Keys by position / Characters as typed — "Ctrl+Alt+Shift+K";
  - Relative mouse — "Ctrl+Alt+Shift+M" (checkbox);
  - Fullscreen — "Ctrl+Alt+Shift+F" (checkbox).
  The standalone "Ctrl+Alt+Del" button goes into the menu.
- Status chips (right side, before Disconnect): path chip ("LAN", "Direct",
  "Relayed" in `warn`), "View only" chip when no permissions, fps in
  `text_weak`. Pure function `status_chips(&self) -> Vec<Chip { text, level }>`
  in `viewer_model.rs` replaces `status_text` (keep `status_text` if the
  CLI uses it).
- `danger_button("Disconnect")`.

#### 6.3 Toolbar visibility

1. `ViewerApp` gets `toolbar: ToolbarMode { Docked, Floating { shown: bool, pinned: bool } }`.
   Windowed: `Docked` (today's behaviour). Fullscreen: `Floating`.
2. `fn toolbar_height(&self) -> f32` returns `TOOLBAR` when docked and 0
   when floating; `mapping()` uses it instead of the constant, so the video
   fills the screen in fullscreen. Call `update_view()` whenever the mode
   changes.
3. Floating: the toolbar is an `egui::Area` anchored at top centre with a
   `surface_raised` frame and rounded bottom corners. Shown when the
   pointer is within 6 px of the top edge, or over the toolbar, or when
   pinned (a 📌 toggle on the toolbar). Hidden 1.5 s after the pointer
   leaves. While hidden, a small handle (40 × 6 px) stays at top centre.
4. Pointer input: the existing rule ("nothing goes to the host while the
   pointer is over egui") already covers the floating toolbar, because it
   checks `egui_wants_pointer_input()`.
5. Pure function for the timing so it can be tested:
   `fn floating_visible(pointer_y: Option<f32>, over_toolbar: bool, pinned: bool, left_at: Option<Instant>, now: Instant) -> bool`.

#### 6.4 Toasts

Draw toasts as an `egui::Area` anchored bottom centre, `surface_raised`
frame, max width 480, wrapping text, fading out over the last 0.5 s of
`TOAST`. Remove them from the toolbar row.

#### 6.5 Narrow windows

When `ui.available_width()` is under 720 px, hide the view and quality
combos from the row and add them to the "⋯" menu as submenus. The ID,
chips and Disconnect stay.

**Tests:** `kanodesk_client::input` for the new chord; `viewer_model.rs`
for `status_chips` (relayed → warn, view only, fps) and
`floating_visible` (top edge shows, pinned shows, 1.5 s after leaving
hides).

**Check by hand** (two machines or host and viewer on one machine, README
"Controlling the host"): enter and leave fullscreen with the chord and the
menu; the toolbar hides and reappears at the top edge; clicks on the
floating toolbar never reach the remote side; the menu toggles relative
mouse and Translate exactly like the shortcuts; toasts show at the bottom
and do not resize the toolbar; at 480 px wide nothing overlaps.

---

## How to check the windows by hand

The audit used two small PowerShell helpers (in the session scratchpad,
not in the repo): one finds a window by its handle and saves it with
`PrintWindow`, the other clicks at a position inside a window. Consider
adding them as `packaging/dev/screenshot.ps1` and `click.ps1` so every
phase can be checked the same way:

1. `cargo build --release -p kanodesk-desktop`
2. Start `target\release\kanodesk.exe`, wait 6 s, capture Home.
3. Click Settings, capture; type an invalid ID, capture.
4. Start `kanodesk.exe --connect 123456789` with an unreachable ID server,
   capture at 4 s and at 35 s.
5. Close the main window normally (it stops the host it started).

Compare against the layout sketches above, at the minimum window size and
at a large one, in Windows dark and light mode. A screenshot showing the
change is part of "done" for every phase.

## Risks

- **Fonts and licences:** Inter is OFL, not MIT/Apache. Bundling it is
  allowed, but ship its licence and list it in `README.md`.
- **Executable icon build step:** `embed-resource` needs the Windows
  resource compiler (it ships with the MSVC toolchain already used here);
  keep it Windows-only so the Linux Docker build is unaffected.
- **AccessKit hidden-window start:** if the window is shown before
  `init_accesskit`, screen readers never see it. Test with Narrator, not
  only by reading the code.
- **Reformatting the ID field** can fight the cursor in egui's `TextEdit`;
  set the cursor explicitly after each reformat and test pasting into the
  middle of the text.
- **Option B of finding 9** changes two IPC protocols. Host and UI ship in
  one binary today, so skew only happens with two installs of different
  versions; the version check already rejects that cleanly.
- **Keep the safety rules:** no keyboard shortcut for Accept in the
  connection manager; nothing reaches the remote side while the pointer is
  over any toolbar, menu or dialog; the Accept delay stays as it is.
