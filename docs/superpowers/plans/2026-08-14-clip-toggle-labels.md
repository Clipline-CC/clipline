# Clip Toggle Labels

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Make the below-timeline scissors control readable. It currently has no label, so people
do not know it is the Clip action.

## Design

The `#trim-mode-toggle` button keeps the scissors icon and its far-right placement. It gains a
visible label **to the left of the icon**:

- Idle (not in clip mode): `Clip`
- Active (clip / trim mode): `Close`

The export row still says `Create Clip` once clip mode is on. Text-then-icon on the toggle, versus
icon-then-text on export, keeps the two scissors buttons from looking like duplicates.

Title, `aria-label`, and the idle timeline hint follow the same Clip / Close wording.

## Task 1: Contract

- [ ] Failing test: `#trim-mode-toggle` contains `<span id="trim-mode-label">Clip</span>` before
      the scissors SVG; CSS lets the button grow (`width: auto`, `gap`) and sizes the icon instead
      of stretching it; `applyTimelineEditorPreference` writes `Close` / `Clip` onto the label.

## Task 2: Markup, style, wiring

- [ ] Add the label span in `index.html`.
- [ ] Restyle `#trim-mode-toggle` as a compact labeled control.
- [ ] Sync the label, title, and aria-label in `applyTimelineEditorPreference`.

## Task 3: Verify

- [ ] `cargo test --workspace` green, `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Launch the app and confirm Clip / Close next to the scissors icon.
