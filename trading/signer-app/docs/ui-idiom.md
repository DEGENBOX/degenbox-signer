# Signer-app UI idiom (calm pass, 2026-06)

ONE visual dialect across all 8 tab surfaces. Phase 1 applied it to the shell +
Solana surfaces; phase 2 applies the SAME rules to `pages/Perps*` +
`features/perps-*`. Shared CSS lives in `src/styles/app.css` ("calm pass"
section), shared components in `src/components/{ui,ShellSection}.tsx`.

## Decoration budget

- Corner brackets (`.corners`) appear ONLY on: the `Modal` shell, `.empty-hero`,
  the unlock card, and ONE signature element per page (e.g. SolPositions' total
  sum rail). Never on ordinary cards, buttons, badges or list cells.
- One glow focus per screen: `.btn.primary` already glows; never stack
  `.glow-accent` on it. Page-level glow = the signature element only
  (total rail on Positions, terminal on Account).
- `[ BRACKET ]` microcopy (`.hud-label.brackets`) ONLY for section-header counts
  and true status readouts (health pill, tabbar mode, terminal head). Role
  markers are badges: `.badge.accent` (primary), `.badge` (neutral/remote).

## Structure & rhythm

- Page head: `<h1>` + `<p class="page-sub">`; page-level actions go in
  `<div class="page-head"> … <span class="page-actions">`.
- Section head: `ShellSection` (`num`, `title`, `count?`, `actions?`) — or the
  identical inline markup `.shell-section-head > .section-num + .shell-section-title
  + .hud-label.brackets(count) + .head-meta(actions)`. NO hand-rolled
  `section-num + hud-label` rows.
- Vertical rhythm: sections 28px apart (`.shell-section`), blocks/cards 14px
  (`.card`, `.kpi-strip`, banners). No ad-hoc inline margins for these gaps.

## KPI / summary numbers

- THE readout band is `.kpi-strip` + `Kpi` (components/ui): mono label,
  tabular-nums value, optional `sub`, `tone="pos"|"neg"`, `loading` skeleton.
  Do not use @degenbox/ui `Stat` tiles or hand-rolled stat grids on tab pages.

## Tables

- `table.table`; numeric columns get `th.num`/`td.num` (right-aligned, mono,
  tabular-nums); labels left. Row hover comes from `.table tbody tr:hover td`.
- Row actions: `.btn.sm` (4px 9px / 12px) or `.btn.xs` (3px 7px / 11px) —
  never inline padding/font-size styles.
- Expand affordance: ONE `<ChevronRight className={`chev ${open ? "open" : ""}`}>`
  (rotates 90°); expanded content wrapped in `<div className="expand-in">`.
- Empty tables/lists render `EmptyState` (icon + line + mono hint) inside the
  `<td colSpan>` — never a bare "no items" string. Page-level zero states use
  `EmptyHero` (icon + title + desc + one primary action).

## Dialogs

- Always `Modal` from components/ui (or `DangerConfirm` for type-to-confirm
  destructive actions — the components/ui one, NOT @degenbox/ui's).
- Width tiers: 420 (confirm) · 520 (form) · 620 (editor/matrix). Nothing else.
- Footer: `<div className="modal-foot">` — hairline top border, Cancel left of
  the primary; destructive extras (Delete/Disarm/Clear) push left with
  `style={{ marginRight: "auto" }}`. Success states may center a single Done.
- Title row: `modal-head` with the X dismiss — provided by `Modal`, never
  re-rolled.

## Motion (CSS-only, no libs)

- Dialogs: entrance 300ms `cubic-bezier(0.16,1,0.3,1)` (backdrop fade + panel
  rise), exit 200ms ease-in — handled by `Modal` (`.closing` state, cached
  children). No work needed per dialog.
- Tab content: the shell wraps panes in `.tab-pane` (300ms rise) — pages do
  nothing.
- Row expands / inline panels: `.expand-in`.
- Chevrons: `.chev` / `.chev.open` (200ms rotate).
- Everything respects `prefers-reduced-motion`.

## Numbers & formatters

- `tabular-nums` wherever money/percent/counts render (`.num`, `.kpi-value`,
  Tailwind `tabular-nums`).
- Reuse existing helpers: `fmtUsd`/`shortAddr`/`timeAgo` (components/ui),
  `fmtSolAmt`/`compactNum`/`num`/`lamportsToSol` (features/positions/data.ts),
  `fmtSol` (@degenbox/ui). Do not add new formatters.

## States

- Every interactive element keeps hover/active/focus (`.btn`, `.choice`,
  `.menu`, global `:focus-visible`). Disabled-with-reason uses `title` (wrap in
  `DisabledHint` when the control is disabled).
- Loading: `SkeletonRows` in tables, `.skeleton` spans in cards, `Kpi loading`.
