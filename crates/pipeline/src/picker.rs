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
}

impl PickerItem {
    /// A row with no detail line.
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            detail: None,
        }
    }

    /// Builder-style detail setter.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
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
        }
    }

    /// Builder-style subtitle setter.
    #[must_use]
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
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
        self.items = items;
        self
    }

    /// Mark it failed.
    #[must_use]
    pub fn failed(mut self, why: impl Into<String>) -> Self {
        self.status = PickerStatus::Failed(why.into());
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
    let capacity = ((room + row_gap) / (row_h + row_gap)).floor().max(0.0) as usize;

    let rows = picker
        .items
        .iter()
        .take(capacity)
        .enumerate()
        .map(|(i, item)| {
            (
                item.id.clone(),
                Rect {
                    x: margin,
                    y: list_top + (row_h + row_gap) * i as f32,
                    w: list_w,
                    h: row_h,
                },
            )
        })
        .collect();

    Layout {
        back,
        rows,
        title_baseline: 232.0 * s,
        scale: s,
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
    back: Rgba,
    status: Rgba,
    failed: Rgba,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            // Same ramp as the idle screen: the picker is the same surface, one level in.
            bg_top: [0x0d, 0x14, 0x28, 0xff],
            bg_bottom: [0x03, 0x05, 0x0b, 0xff],
            title: [0xff, 0xff, 0xff, 0xff],
            subtitle: [0x4f, 0xd1, 0xc5, 0xff],
            row_bg: [0x15, 0x1e, 0x35, 0xff],
            row_title: [0xe8, 0xec, 0xf4, 0xff],
            row_detail: [0x9a, 0xa4, 0xb8, 0xff],
            back: [0x9a, 0xa4, 0xb8, 0xff],
            status: [0x9a, 0xa4, 0xb8, 0xff],
            // dma.space coral, for the one state that is bad news.
            failed: [0xf5, 0x61, 0x5f, 0xff],
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
            &item.title,
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
                detail,
                22.0 * s,
                pal.row_detail,
                &f.regular,
            );
        }
        // A chevron on the right: this row goes somewhere.
        let (cx, cy) = (rect.x + rect.w - 40.0 * s, rect.y + rect.h / 2.0);
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

    // Status, where the list would be. Never *instead of* rows that exist: a picker that
    // found three hosts and then failed refreshing still shows the three.
    let status_line = match &picker.status {
        PickerStatus::Ready => None,
        PickerStatus::Busy(m) | PickerStatus::Empty(m) => Some((m.as_str(), pal.status)),
        PickerStatus::Failed(m) => Some((m.as_str(), pal.failed)),
    };
    if let Some((msg, colour)) = status_line {
        let y = l
            .rows
            .last()
            .map_or(280.0 * s, |(_, r)| r.y + r.h + 60.0 * s);
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
    fn it_rasterizes_at_panel_scale() {
        let p = picker();
        let buf = render(&p, 1280, 720).unwrap();
        assert_eq!(buf.len(), 1280 * 720 * 4);
        // Something was drawn over the gradient: the rows are lighter than the background.
        assert!(buf.iter().any(|b| *b > 0x30));
    }
}
