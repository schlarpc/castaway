//! The picker: a title, a list you can tap, and a way back (D38).
//!
//! One screen type serves every "choose one of these" the panel needs — GameStream hosts,
//! then that host's apps, then media files and output devices. It knows nothing about any
//! of them: items carry an opaque id that `app` gave it and hands back on a press, the
//! same way the transport strip knows nothing about protocols and only about capabilities
//! (D33).
//!
//! Layout and hit-testing are pure and share one source of truth, so a row cannot be drawn
//! where it cannot be pressed. Rendering is CPU into RGBA8, like every other surface here.

use crate::error::PipelineError;
use crate::shape::{self, Rect};
use crate::text::{self, Rgba};
use crate::theme;

/// The design height every dimension scales from, matching the idle screen's.
const DESIGN_HEIGHT: f32 = 720.0;

/// One row.
#[derive(Debug, Clone, PartialEq)]
pub struct PickerItem {
    /// Opaque identity, echoed back when pressed.
    pub id: String,
    /// The line a person reads.
    pub title: String,
    /// A dimmer second line — an address, a codec, a path.
    pub detail: Option<String>,
    /// Whether this row is the one currently in effect — a choice list's checkmark.
    ///
    /// Marked rows draw a check where unmarked ones draw the go-somewhere chevron,
    /// because in a choice list a row is a selection, not a doorway, and drawing the
    /// chevron anyway would promise a screen that never comes.
    pub marked: bool,
}

impl PickerItem {
    /// A row with no detail line.
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            detail: None,
            marked: false,
        }
    }

    /// Builder-style detail setter.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Builder-style current-choice marker.
    #[must_use]
    pub const fn with_marked(mut self, marked: bool) -> Self {
        self.marked = marked;
        self
    }
}

/// What the list is doing, when it is not simply showing items.
///
/// Its own type rather than an empty `items` vec, because "still looking", "nothing
/// found" and "it went wrong" are three different things to a person standing in front of
/// the panel, and an empty list says none of them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PickerStatus {
    /// Showing what it has.
    Ready,
    /// Working — discovering, pairing, fetching. Carries what to say.
    Busy(String),
    /// Finished, with nothing to show. Carries why, in a person's terms.
    Empty(String),
    /// Failed. Carries the reason, which for a host is usually the host's own words.
    Failed(String),
}

/// A list screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Picker {
    /// Heading.
    pub title: String,
    /// Optional line under the heading — where these came from.
    pub subtitle: Option<String>,
    /// The rows.
    pub items: Vec<PickerItem>,
    /// What the list is doing.
    pub status: PickerStatus,
    /// How far the list is scrolled, in rows. Fractional so a drag moves smoothly rather
    /// than snapping a row at a time.
    ///
    /// Kept in rows rather than pixels because the model does not know the panel's size —
    /// the same reason every other screen carries a model and lets the render thread
    /// decide what it measures.
    pub scroll: f32,
    /// Who this screen belongs to, for updates that arrive long after the press.
    ///
    /// Nothing renders it. It exists because `back` is answered render-side and never
    /// reaches `app`, so a slow producer — an update downloading for four minutes (#360)
    /// — has no way to know the person walked off. Tagging the screen lets
    /// [`crate::render_pipeline::RenderCommand::ReplaceScreenTagged`] ask *the stack*
    /// whether this is still the screen on top, rather than having the sender guess.
    pub tag: Option<String>,
}

impl Picker {
    /// A busy picker, which is what one always starts as.
    #[must_use]
    pub fn loading(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            items: Vec::new(),
            status: PickerStatus::Busy(message.into()),
            scroll: 0.0,
            tag: None,
        }
    }

    /// Builder-style subtitle setter.
    #[must_use]
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Builder-style owner tag — see [`Self::tag`].
    #[must_use]
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Replace the rows, moving to [`PickerStatus::Ready`] — or to
    /// [`PickerStatus::Empty`] with `empty_message` if there are none.
    #[must_use]
    pub fn with_items(mut self, items: Vec<PickerItem>, empty_message: impl Into<String>) -> Self {
        self.status = if items.is_empty() {
            PickerStatus::Empty(empty_message.into())
        } else {
            PickerStatus::Ready
        };
        // A new list starts at the top: keeping a scroll offset across a refresh would
        // leave someone looking at blank space where their row used to be.
        self.scroll = 0.0;
        self.items = items;
        self
    }

    /// Scroll by `rows`, clamped to what there is to see.
    ///
    /// `visible` is how many rows the panel can show, which only the layout knows.
    pub fn scroll_by(&mut self, rows: f32, visible: usize) {
        let max = (self.items.len().saturating_sub(visible)) as f32;
        self.scroll = (self.scroll + rows).clamp(0.0, max);
    }

    /// Whether there is anything above or below what is showing.
    #[must_use]
    pub fn overflow(&self, visible: usize) -> (bool, bool) {
        let max = (self.items.len().saturating_sub(visible)) as f32;
        (self.scroll > 0.01, self.scroll < max - 0.01)
    }

    /// Mark it failed.
    #[must_use]
    pub fn failed(mut self, why: impl Into<String>) -> Self {
        self.status = PickerStatus::Failed(why.into());
        self
    }

    /// Mark it busy again, with what to say while it is. The other half of
    /// [`Self::failed`], for a screen whose *later* states are waits too — a download
    /// reporting its way to 100% is not "loading", it is loading over and over.
    #[must_use]
    pub fn busy(mut self, message: impl Into<String>) -> Self {
        self.status = PickerStatus::Busy(message.into());
        self
    }
}

/// What a press on the picker means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerHit {
    /// The back affordance.
    Back,
    /// A row, by its id.
    Item(String),
}

/// Where everything lands, in device pixels. One layout, two consumers (D33).
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The back affordance's touch target.
    pub back: Rect,
    /// Rows, in order, paired with their item id.
    pub rows: Vec<(String, Rect)>,
    /// Where the heading's baseline sits.
    pub title_baseline: f32,
    /// Scale factor from the design height.
    pub scale: f32,
    /// How many rows the panel can show at once — what `scroll_by` needs to clamp.
    pub visible: usize,
    /// Pixels from one row's top to the next, for turning a drag into rows.
    pub row_step: f32,
}

impl Layout {
    /// What a device-pixel point hits, if anything.
    #[must_use]
    pub fn hit(&self, px: f32, py: f32) -> Option<PickerHit> {
        if self.back.contains(px, py) {
            return Some(PickerHit::Back);
        }
        self.rows
            .iter()
            .find(|(_, r)| r.contains(px, py))
            .map(|(id, _)| PickerHit::Item(id.clone()))
    }
}

/// Lay the picker out for a `width`×`height` surface.
#[must_use]
pub fn layout(picker: &Picker, width: u32, height: u32) -> Layout {
    let s = height as f32 / DESIGN_HEIGHT;
    let margin = 90.0 * s;
    // Generous, because this is a wall panel and the target *is* the row (Fitts' law,
    // same reasoning as the transport strip's buttons).
    let row_h = 84.0 * s;
    let row_gap = 12.0 * s;
    // Below the back affordance, which owns the top-left corner down to 156*s.
    let list_top = 330.0 * s;
    let list_w = (width as f32 - margin * 2.0).min(1100.0 * s);

    // The back affordance is a large corner target rather than a small chevron: someone
    // reaching for it is usually not looking at it.
    let back = Rect {
        x: margin - 20.0 * s,
        y: 60.0 * s,
        w: 200.0 * s,
        h: 96.0 * s,
    };

    // Only as many rows as fit; the rest are not laid out, so they cannot be pressed
    // where they are not drawn. Scrolling is deliberately not here yet — see the module
    // note in docs/app-shell.md about what a long list needs.
    let room = (height as f32 - list_top - 80.0 * s).max(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let capacity = ((room + row_gap) / (row_h + row_gap)).floor().max(0.0) as usize;

    // Whole rows only, and only ones fully on screen: a half-drawn row at the bottom is
    // one a finger can reach and a person cannot read.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let first = picker.scroll.max(0.0).floor() as usize;
    let rows = picker
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(capacity)
        .map(|(i, item)| {
            let slot = i as f32 - picker.scroll;
            (
                item.id.clone(),
                Rect {
                    x: margin,
                    y: list_top + (row_h + row_gap) * slot,
                    w: list_w,
                    h: row_h,
                },
            )
        })
        .filter(|(_, r)| r.y >= list_top - row_h * 0.5 && r.y + r.h <= height as f32)
        .collect();

    Layout {
        back,
        rows,
        title_baseline: 232.0 * s,
        scale: s,
        visible: capacity,
        row_step: row_h + row_gap,
    }
}

/// Which item a panel-normalized point hits.
#[must_use]
pub fn hit(picker: &Picker, width: u32, height: u32, x: f32, y: f32) -> Option<PickerHit> {
    layout(picker, width, height).hit(x * width as f32, y * height as f32)
}

struct Palette {
    bg_top: Rgba,
    bg_bottom: Rgba,
    title: Rgba,
    subtitle: Rgba,
    row_bg: Rgba,
    row_title: Rgba,
    row_detail: Rgba,
    /// The current-choice check.
    current: Rgba,
    back: Rgba,
    status: Rgba,
    failed: Rgba,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            // Same ramp as the idle screen: the picker is the same surface, one level in.
            bg_top: theme::BG_TOP,
            bg_bottom: theme::BG_BOTTOM,
            title: theme::TEXT,
            subtitle: theme::ACCENT,
            row_bg: theme::PLATE,
            row_title: theme::TEXT_BODY,
            row_detail: theme::TEXT_DIM,
            current: theme::ACCENT,
            back: theme::TEXT_DIM,
            status: theme::TEXT_DIM,
            // dma.space coral, for the one state that is bad news.
            failed: theme::CORAL,
        }
    }
}

/// Draw the picker into a fresh RGBA8 buffer.
///
/// # Errors
/// [`PipelineError`] if the bundled fonts fail to load.
pub fn render(picker: &Picker, width: u32, height: u32) -> Result<Vec<u8>, PipelineError> {
    let f = text::fonts()?;
    let pal = Palette::default();
    let l = layout(picker, width, height);
    let s = l.scale;

    let mut buf = vec![0u8; (width * height * 4) as usize];
    text::fill_gradient(&mut buf, width, height, pal.bg_top, pal.bg_bottom);

    // Back: a chevron and the word, because a bare arrow on an appliance is a guess.
    let (bx, by) = (l.back.x + 34.0 * s, l.back.y + l.back.h / 2.0);
    shape::chevron(
        &mut buf,
        width,
        height,
        bx,
        by,
        15.0 * s,
        4.0 * s,
        pal.back,
        shape::Facing::Left,
    );
    text::draw_text(
        &mut buf,
        width,
        height,
        bx + 30.0 * s,
        by + text::ascent(&f.regular, 26.0 * s) * 0.36,
        "Back",
        26.0 * s,
        pal.back,
        &f.regular,
    );

    // Heading.
    text::draw_text(
        &mut buf,
        width,
        height,
        90.0 * s,
        l.title_baseline,
        &picker.title,
        52.0 * s,
        pal.title,
        &f.bold,
    );
    if let Some(sub) = &picker.subtitle {
        text::draw_text(
            &mut buf,
            width,
            height,
            90.0 * s,
            l.title_baseline + 42.0 * s,
            sub,
            26.0 * s,
            pal.subtitle,
            &f.regular,
        );
    }

    // Rows.
    for (item, (_, rect)) in picker.items.iter().zip(&l.rows) {
        shape::rounded_rect(&mut buf, width, height, *rect, 14.0 * s, pal.row_bg);
        let tx = rect.x + 28.0 * s;
        // Neither line wraps, so a long one is a line that runs off the plate, across the
        // chevron and off the side of the panel — which is what a settings summary written
        // as a sentence did. The right edge clears the chevron (centred at rect.w - 40·s,
        // arms 11·s) with a gap, so text stops before the mark rather than under it.
        let avail = (rect.w - 68.0 * s - 28.0 * s).max(0.0);
        let has_detail = item.detail.is_some();
        let title_baseline = if has_detail {
            rect.y + rect.h * 0.42
        } else {
            rect.y + rect.h * 0.62
        };
        text::draw_text(
            &mut buf,
            width,
            height,
            tx,
            title_baseline,
            &text::ellipsize(&f.bold, &item.title, 30.0 * s, avail),
            30.0 * s,
            pal.row_title,
            &f.bold,
        );
        if let Some(detail) = &item.detail {
            text::draw_text(
                &mut buf,
                width,
                height,
                tx,
                rect.y + rect.h * 0.78,
                &text::ellipsize(&f.regular, detail, 22.0 * s, avail),
                22.0 * s,
                pal.row_detail,
                &f.regular,
            );
        }
        let (cx, cy) = (rect.x + rect.w - 40.0 * s, rect.y + rect.h / 2.0);
        if item.marked {
            // A check: this row is the one in effect right now.
            let thickness = 3.2 * s;
            let (ax, ay) = (cx - 11.0 * s, cy + 0.5 * s);
            let (bx, by) = (cx - 3.5 * s, cy + 8.0 * s);
            let (ex, ey) = (cx + 11.0 * s, cy - 8.0 * s);
            for (x0, y0, x1, y1) in [(ax, ay, bx, by), (bx, by, ex, ey)] {
                shape::fill_sdf(
                    &mut buf,
                    width,
                    height,
                    Rect::around(cx, cy, 16.0 * s),
                    pal.current,
                    |px, py| shape::sd_segment(px, py, x0, y0, x1, y1) - thickness / 2.0,
                );
            }
        } else {
            // A chevron on the right: this row goes somewhere.
            shape::chevron(
                &mut buf,
                width,
                height,
                cx,
                cy,
                11.0 * s,
                3.2 * s,
                pal.row_detail,
                shape::Facing::Right,
            );
        }
    }

    // A hint that there is more, top and bottom. A list that simply stops looks like a
    // list that ended, and someone who cannot see that it scrolls will not try.
    let (more_above, more_below) = picker.overflow(l.visible);
    for (show, cy, facing) in [
        (more_above, 300.0 * s, shape::Facing::Left),
        (more_below, height as f32 - 46.0 * s, shape::Facing::Right),
    ] {
        if !show {
            continue;
        }
        // A chevron rotated by drawing it on its side: the same mark as everywhere else,
        // pointing the way there is more to go.
        let cx = 90.0 * s + 1100.0 * s / 2.0;
        let a = 10.0 * s;
        let (dy0, dy1) = if facing == shape::Facing::Left {
            (a, -a)
        } else {
            (-a, a)
        };
        for dx in [-a, a] {
            shape::fill_sdf(
                &mut buf,
                width,
                height,
                Rect::around(cx, cy, a * 2.5),
                pal.row_detail,
                |px, py| shape::sd_segment(px, py, cx, cy + dy1, cx + dx, cy + dy0) - 2.4 * s,
            );
        }
    }

    // Status, where the list would be. Never *instead of* rows that exist: a picker that
    // found three hosts and then failed refreshing still shows the three.
    let status_line = match &picker.status {
        PickerStatus::Ready => None,
        PickerStatus::Busy(m) | PickerStatus::Empty(m) => Some((m.as_str(), pal.status)),
        PickerStatus::Failed(m) => Some((m.as_str(), pal.failed)),
    };
    if let Some((msg, colour)) = status_line {
        // With no rows the message sits where the first row would (the list's own top,
        // plus a baseline's worth), not at 280·s — that number landed exactly on the
        // subtitle's descenders, and "Searching…" printed through the tagline.
        let y = l
            .rows
            .last()
            .map_or(365.0 * s, |(_, r)| r.y + r.h + 60.0 * s);
        text::draw_text(
            &mut buf,
            width,
            height,
            90.0 * s,
            y,
            msg,
            28.0 * s,
            colour,
            &f.regular,
        );
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn picker() -> Picker {
        Picker::loading("Moonlight", "Looking for hosts…").with_items(
            vec![
                PickerItem::new("a", "somepc").with_detail("10.0.0.7"),
                PickerItem::new("b", "loungebox").with_detail("10.0.0.9"),
            ],
            "none found",
        )
    }

    #[test]
    fn every_drawn_row_is_pressable_at_its_own_centre() {
        // D33's rule: a control cannot be drawn where it cannot be pressed. The layout is
        // the only source of truth for both, so this asserts they agree.
        let p = picker();
        let l = layout(&p, 1920, 1080);
        assert_eq!(l.rows.len(), 2);
        for (id, rect) in &l.rows {
            let (cx, cy) = rect.center();
            assert_eq!(l.hit(cx, cy), Some(PickerHit::Item(id.clone())));
        }
    }

    #[test]
    fn the_heading_clears_the_back_affordance() {
        // They shared the top-left corner and overlapped, which looked like a rendering
        // fault rather than a layout one.
        let p = picker();
        let l = layout(&p, 1920, 1080);
        // The heading's cap-height sits above its baseline; 56px of ascent at this scale.
        let heading_top = l.title_baseline - 56.0 * l.scale;
        assert!(
            heading_top >= l.back.y + l.back.h * 0.5,
            "the heading runs into the back target"
        );
    }

    #[test]
    fn rows_do_not_overlap_each_other_or_the_back_target() {
        let p = picker();
        let l = layout(&p, 1920, 1080);
        for w in l.rows.windows(2) {
            let (a, b) = (w[0].1, w[1].1);
            assert!(a.y + a.h <= b.y, "rows overlap");
        }
        for (_, r) in &l.rows {
            assert!(r.y >= l.back.y + l.back.h, "a row overlaps the back target");
        }
    }

    #[test]
    fn back_is_hittable_and_is_not_a_row() {
        let p = picker();
        let l = layout(&p, 1920, 1080);
        let (cx, cy) = l.back.center();
        assert_eq!(l.hit(cx, cy), Some(PickerHit::Back));
    }

    #[test]
    fn a_press_on_nothing_is_nothing() {
        let p = picker();
        let l = layout(&p, 1920, 1080);
        assert_eq!(l.hit(1900.0, 1060.0), None);
    }

    #[test]
    fn scrolling_brings_later_rows_into_reach_and_stops_at_the_end() {
        let many: Vec<_> = (0..40)
            .map(|i| PickerItem::new(format!("id{i}"), format!("host {i}")))
            .collect();
        let mut p = Picker::loading("Many", "…").with_items(many, "none");
        let l = layout(&p, 1920, 1080);
        let visible = l.visible;
        assert!(visible < 40, "the point of the test");
        // The first row is on screen and the last is not.
        assert!(l
            .hit(l.rows[0].1.center().0, l.rows[0].1.center().1)
            .is_some());
        assert!(!l.rows.iter().any(|(id, _)| id == "id39"));

        p.scroll_by(1000.0, visible);
        let l = layout(&p, 1920, 1080);
        assert!(
            l.rows.iter().any(|(id, _)| id == "id39"),
            "scrolled to the end, the last row should be reachable"
        );
        // ...and it cannot go further, so there is no blank space past the end.
        let at_end = p.scroll;
        p.scroll_by(50.0, visible);
        assert!(
            (p.scroll - at_end).abs() < f32::EPSILON,
            "clamped at the end"
        );
    }

    #[test]
    fn a_scrolled_row_is_still_pressable_where_it_is_drawn() {
        // The rule that holds everywhere: drawn and pressable come from one layout.
        let many: Vec<_> = (0..40)
            .map(|i| PickerItem::new(format!("id{i}"), format!("host {i}")))
            .collect();
        let mut p = Picker::loading("Many", "…").with_items(many, "none");
        p.scroll_by(5.0, layout(&p, 1920, 1080).visible);
        let l = layout(&p, 1920, 1080);
        for (id, rect) in &l.rows {
            let (cx, cy) = rect.center();
            assert_eq!(l.hit(cx, cy), Some(PickerHit::Item(id.clone())));
            assert!(rect.y + rect.h <= 1080.0, "a row runs off the panel");
        }
    }

    #[test]
    fn a_refreshed_list_goes_back_to_the_top() {
        // Keeping the offset would leave someone looking at blank space where their row
        // used to be.
        let mut p = Picker::loading("Many", "…").with_items(
            (0..40)
                .map(|i| PickerItem::new(format!("id{i}"), format!("h{i}")))
                .collect(),
            "none",
        );
        p.scroll_by(10.0, 5);
        assert!(p.scroll > 0.0);
        let p = p.with_items(vec![PickerItem::new("a", "only one")], "none");
        assert_eq!(p.scroll, 0.0);
    }

    #[test]
    fn overflow_says_which_way_there_is_more() {
        let mut p = Picker::loading("Many", "…").with_items(
            (0..40)
                .map(|i| PickerItem::new(format!("id{i}"), format!("h{i}")))
                .collect(),
            "none",
        );
        let visible = layout(&p, 1920, 1080).visible;
        assert_eq!(p.overflow(visible), (false, true), "at the top");
        p.scroll_by(3.0, visible);
        assert_eq!(p.overflow(visible), (true, true), "in the middle");
        p.scroll_by(1000.0, visible);
        assert_eq!(p.overflow(visible), (true, false), "at the end");
    }

    #[test]
    fn more_items_than_fit_are_not_laid_out_so_they_cannot_be_pressed_unseen() {
        // The failure this prevents: rows laid out past the bottom of the panel are
        // invisible, and a press near the edge would select one nobody could see.
        let many: Vec<_> = (0..200)
            .map(|i| PickerItem::new(format!("id{i}"), format!("host {i}")))
            .collect();
        let p = Picker::loading("Many", "…").with_items(many, "none");
        let l = layout(&p, 1920, 1080);
        assert!(l.rows.len() < 200);
        for (_, r) in &l.rows {
            assert!(r.y + r.h <= 1080.0, "a laid-out row runs off the panel");
        }
    }

    #[test]
    fn a_normalized_panel_touch_lands_on_the_row_under_it() {
        let p = picker();
        let (w, h) = (1920, 1080);
        let l = layout(&p, w, h);
        let (id, rect) = l.rows[1].clone();
        let (cx, cy) = rect.center();
        assert_eq!(
            hit(&p, w, h, cx / w as f32, cy / h as f32),
            Some(PickerHit::Item(id))
        );
    }

    #[test]
    fn status_distinguishes_looking_from_found_nothing() {
        // Three different things to someone standing in front of the panel, and an empty
        // list says none of them.
        let busy = Picker::loading("x", "Looking…");
        assert!(matches!(busy.status, PickerStatus::Busy(_)));
        let empty = Picker::loading("x", "Looking…").with_items(vec![], "No hosts on this network");
        assert!(matches!(empty.status, PickerStatus::Empty(_)));
        let failed = Picker::loading("x", "Looking…").failed("the host refused");
        assert!(matches!(failed.status, PickerStatus::Failed(_)));
    }

    #[test]
    fn a_marked_row_draws_its_check_not_a_doorway() {
        // Same list twice, differing only in which row claims to be current: the pixels
        // must differ, or "this one is in effect" is a field nobody can see.
        let plain = Picker::loading("Output", "…")
            .with_items(vec![PickerItem::new("a", "Speakers")], "none");
        let marked = Picker::loading("Output", "…").with_items(
            vec![PickerItem::new("a", "Speakers").with_marked(true)],
            "none",
        );
        assert!(!plain.items[0].marked, "unmarked is the default");
        let a = render(&plain, 1280, 720).unwrap();
        let b = render(&marked, 1280, 720).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_row_that_is_too_long_is_cut_rather_than_drawn_off_the_panel() {
        // Two rows differing only in text far past where the row ends. If the lines are
        // cut to the row, the two pictures are the same one; if they are not, the panel
        // is showing whatever a caller happened to write — which is how a settings
        // summary written as a sentence ran off the side of the glass.
        let of = |tail: &str| {
            // The run of A's is longer than any row, so both variants are cut inside it
            // and the tails differ only where nothing may be drawn.
            let over = format!("{}{tail}", "A".repeat(200));
            Picker::loading("Settings", "…").with_items(
                vec![PickerItem::new("a", format!("Check for updates {over}"))
                    .with_detail(format!("Build 957 {over}"))],
                "none",
            )
        };
        let x = render(&of("xxxxxxxxxx"), 1280, 720).unwrap();
        let y = render(&of("yyyyyyyyyy"), 1280, 720).unwrap();
        assert!(x == y, "text past the row's edge reached the panel");
        // And a cut row is still a drawn row, not a blank one.
        let short = Picker::loading("Settings", "…").with_items(
            vec![PickerItem::new("a", "Check for updates").with_detail("Build 957")],
            "none",
        );
        assert!(x != render(&short, 1280, 720).unwrap());
    }

    #[test]
    fn it_rasterizes_at_panel_scale() {
        let p = picker();
        let buf = render(&p, 1280, 720).unwrap();
        assert_eq!(buf.len(), 1280 * 720 * 4);
        // Something was drawn over the gradient: the rows are lighter than the background.
        assert!(buf.iter().any(|b| *b > 0x30));
    }
}
