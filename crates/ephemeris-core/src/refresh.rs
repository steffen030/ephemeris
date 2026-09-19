//! Refresh scheduler — damage coalescing + ghosting mitigation (task
//! ephemeris-2ql.7, ADR ephemeris-btg).
//!
//! e-ink panels trade refresh quality for speed.  A live pen stroke wants the
//! fastest (`Fast`, A2/DU-like) update; a UI/widget change wants a crisper
//! `Partial` (grayscale) update; and every few updates the panel must do a
//! `Full`/`Clear` waveform to flush the ghosting that fast updates leave
//! behind.  Deciding this per [`Display::present`](ephemeris_pal::Display::present)
//! call is the job of the [`RefreshScheduler`].
//!
//! The scheduler is device-agnostic and pure: callers [`submit`] damage rects
//! tagged with a [`DamageSource`] as they happen, then [`flush`] once per frame
//! to obtain the [`Present`] (coalesced damage + chosen [`RefreshMode`]) to
//! hand to the [`Display`](ephemeris_pal::Display).  This keeps the ghosting
//! policy in one shared place rather than scattered through the UI.
//!
//! [`submit`]: RefreshScheduler::submit
//! [`flush`]: RefreshScheduler::flush

use ephemeris_pal::display::{Rect, RefreshMode};

/// What produced a damage region.  Higher-quality sources win when several
/// kinds of change accumulate in the same frame (see [`DamageSource::priority`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageSource {
    /// Live pen ink — wants the fastest (`Fast`/A2) refresh.
    Ink,
    /// UI / widget change — wants a crisp `Partial` refresh.
    Ui,
    /// Whole-screen change (page or view switch) — forces a `Clear` refresh to
    /// flush ghosting outright.
    ScreenChange,
}

impl DamageSource {
    /// Ordering used to pick a frame's [`RefreshMode`] when multiple sources
    /// contributed damage.  A larger value dominates: a UI change mixed with
    /// live ink is rendered `Partial` (so text stays crisp), and any
    /// screen change dominates everything.
    fn priority(self) -> u8 {
        match self {
            DamageSource::Ink => 0,
            DamageSource::Ui => 1,
            DamageSource::ScreenChange => 2,
        }
    }
}

/// Tunable thresholds for the [`RefreshScheduler`].
#[derive(Debug, Clone, Copy)]
pub struct RefreshConfig {
    /// Force a `Full` refresh after this many consecutive `Fast`/`Partial`
    /// frames, to clear accumulated ghosting.  `0` disables periodic fulls.
    pub full_refresh_interval: u32,
    /// If coalescing leaves more than this many rects, union them into a single
    /// bounding rect (bounds the per-frame damage list).
    pub max_damage_rects: usize,
    /// Two rects are merged when the gap between them is `<=` this many pixels.
    /// Larger values coalesce more aggressively (fewer, bigger updates).
    pub coalesce_gap: u32,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            full_refresh_interval: 20,
            max_damage_rects: 8,
            coalesce_gap: 16,
        }
    }
}

/// The decision produced by [`RefreshScheduler::flush`] for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Present {
    /// Refresh waveform to request from the display.
    pub mode: RefreshMode,
    /// Coalesced damage rects (bounding-box union of submitted damage).
    pub damage: Vec<Rect>,
}

/// Decides a [`RefreshMode`] and coalesced damage per frame, and injects
/// periodic `Full` refreshes to keep e-ink ghosting in check.
#[derive(Debug, Clone)]
pub struct RefreshScheduler {
    config: RefreshConfig,
    /// Damage submitted since the last [`flush`](RefreshScheduler::flush).
    pending: Vec<Rect>,
    /// Highest-priority source seen since the last flush (`None` if idle).
    source: Option<DamageSource>,
    /// `Fast`/`Partial` frames emitted since the last `Full`/`Clear`.
    since_full: u32,
}

impl Default for RefreshScheduler {
    fn default() -> Self {
        Self::new(RefreshConfig::default())
    }
}

impl RefreshScheduler {
    /// Create a scheduler with the given thresholds.
    pub fn new(config: RefreshConfig) -> Self {
        Self {
            config,
            pending: Vec::new(),
            source: None,
            since_full: 0,
        }
    }

    /// Record a damage region for the next frame.  Zero-area rects are ignored.
    ///
    /// Damage is coalesced at [`flush`](RefreshScheduler::flush) time, so rapid
    /// bursts submitted between frames collapse into as few updates as possible.
    pub fn submit(&mut self, rect: Rect, source: DamageSource) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.pending.push(rect);
        self.source = Some(match self.source {
            Some(existing) if existing.priority() >= source.priority() => existing,
            _ => source,
        });
    }

    /// Whether any damage is waiting to be flushed.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Consume the accumulated damage and decide the frame's refresh.
    ///
    /// Returns `None` when nothing was submitted since the last flush (no
    /// present needed).  Otherwise the pending damage is coalesced, a
    /// [`RefreshMode`] is chosen from the dominant [`DamageSource`] and the
    /// ghosting counter, and internal state is reset for the next frame.
    pub fn flush(&mut self) -> Option<Present> {
        let source = self.source.take()?;
        let rects = std::mem::take(&mut self.pending);
        let damage = coalesce(
            &rects,
            self.config.coalesce_gap,
            self.config.max_damage_rects,
        );

        let mode = self.decide_mode(source);
        match mode {
            RefreshMode::Full | RefreshMode::Clear => self.since_full = 0,
            RefreshMode::Fast | RefreshMode::Partial => self.since_full += 1,
        }

        Some(Present { mode, damage })
    }

    /// Pick the refresh mode for the dominant source, upgrading to `Full` when
    /// the periodic ghosting-mitigation interval is due.
    fn decide_mode(&self, source: DamageSource) -> RefreshMode {
        // A screen change always clears ghosting outright.
        if source == DamageSource::ScreenChange {
            return RefreshMode::Clear;
        }
        // Periodic full flush: this frame would be the Nth fast/partial.
        let interval = self.config.full_refresh_interval;
        if interval != 0 && self.since_full + 1 >= interval {
            return RefreshMode::Full;
        }
        match source {
            DamageSource::Ink => RefreshMode::Fast,
            DamageSource::Ui => RefreshMode::Partial,
            DamageSource::ScreenChange => unreachable!("handled above"),
        }
    }
}

/// Merge a set of damage rects, joining any pair whose gap is `<= gap` pixels
/// into their bounding box.  If more than `max` rects remain, they are unioned
/// into a single bounding rect to bound the per-frame damage list.
fn coalesce(rects: &[Rect], gap: u32, max: usize) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    for &r in rects {
        let mut merged = r;
        // Restart the scan after each merge: the grown rect may now touch rects
        // we already passed (transitive coalescing).
        let mut i = 0;
        while i < out.len() {
            if close(out[i], merged, gap) {
                merged = union(out[i], merged);
                out.swap_remove(i);
                i = 0;
            } else {
                i += 1;
            }
        }
        out.push(merged);
    }

    if out.len() > max && !out.is_empty() {
        let mut acc = out[0];
        for &r in &out[1..] {
            acc = union(acc, r);
        }
        out = vec![acc];
    }
    out
}

/// Whether `a` and `b` overlap or are within `gap` pixels of each other.
fn close(a: Rect, b: Rect, gap: u32) -> bool {
    // Inflate `a` by `gap` on every side (saturating at the buffer origin) and
    // test for overlap with `b`.
    let ax0 = a.x.saturating_sub(gap);
    let ay0 = a.y.saturating_sub(gap);
    let ax1 = a.x.saturating_add(a.width).saturating_add(gap);
    let ay1 = a.y.saturating_add(a.height).saturating_add(gap);

    let bx1 = b.x.saturating_add(b.width);
    let by1 = b.y.saturating_add(b.height);

    ax0 < bx1 && b.x < ax1 && ay0 < by1 && b.y < ay1
}

/// Bounding box of two rects.
fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = a.x.saturating_add(a.width).max(b.x.saturating_add(b.width));
    let y1 =
        a.y.saturating_add(a.height)
            .max(b.y.saturating_add(b.height));
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(present: &Option<Present>) -> Option<RefreshMode> {
        present.as_ref().map(|p| p.mode)
    }

    #[test]
    fn idle_flush_yields_nothing() {
        let mut sched = RefreshScheduler::default();
        assert!(!sched.has_pending());
        assert_eq!(sched.flush(), None);
    }

    #[test]
    fn zero_area_damage_is_ignored() {
        let mut sched = RefreshScheduler::default();
        sched.submit(Rect::new(0, 0, 0, 10), DamageSource::Ink);
        sched.submit(Rect::new(0, 0, 10, 0), DamageSource::Ink);
        assert!(!sched.has_pending());
        assert_eq!(sched.flush(), None);
    }

    #[test]
    fn ink_stream_is_fast() {
        let mut sched = RefreshScheduler::default();
        for _ in 0..5 {
            sched.submit(Rect::new(10, 10, 4, 4), DamageSource::Ink);
            assert_eq!(modes(&sched.flush()), Some(RefreshMode::Fast));
        }
    }

    #[test]
    fn ui_change_is_partial() {
        let mut sched = RefreshScheduler::default();
        sched.submit(Rect::new(0, 0, 100, 40), DamageSource::Ui);
        assert_eq!(modes(&sched.flush()), Some(RefreshMode::Partial));
    }

    #[test]
    fn mixed_ink_and_ui_prefers_partial() {
        let mut sched = RefreshScheduler::default();
        sched.submit(Rect::new(0, 0, 4, 4), DamageSource::Ink);
        sched.submit(Rect::new(200, 0, 100, 40), DamageSource::Ui);
        assert_eq!(modes(&sched.flush()), Some(RefreshMode::Partial));
    }

    #[test]
    fn screen_change_clears_and_resets_counter() {
        let cfg = RefreshConfig {
            full_refresh_interval: 100,
            ..RefreshConfig::default()
        };
        let mut sched = RefreshScheduler::new(cfg);
        // Build up some partial frames.
        for _ in 0..5 {
            sched.submit(Rect::new(0, 0, 10, 10), DamageSource::Ui);
            sched.flush();
        }
        assert_eq!(sched.since_full, 5);

        sched.submit(Rect::new(0, 0, 800, 600), DamageSource::ScreenChange);
        assert_eq!(modes(&sched.flush()), Some(RefreshMode::Clear));
        assert_eq!(sched.since_full, 0, "clear resets the ghosting counter");
    }

    #[test]
    fn periodic_full_after_interval() {
        let cfg = RefreshConfig {
            full_refresh_interval: 4,
            ..RefreshConfig::default()
        };
        let mut sched = RefreshScheduler::new(cfg);

        let mut seq = Vec::new();
        for _ in 0..9 {
            sched.submit(Rect::new(0, 0, 4, 4), DamageSource::Ink);
            seq.push(sched.flush().unwrap().mode);
        }

        // Every 4th frame is a Full to flush ghosting; the counter resets after.
        assert_eq!(
            seq,
            vec![
                RefreshMode::Fast,
                RefreshMode::Fast,
                RefreshMode::Fast,
                RefreshMode::Full,
                RefreshMode::Fast,
                RefreshMode::Fast,
                RefreshMode::Fast,
                RefreshMode::Full,
                RefreshMode::Fast,
            ]
        );
    }

    #[test]
    fn interval_zero_disables_periodic_full() {
        let cfg = RefreshConfig {
            full_refresh_interval: 0,
            ..RefreshConfig::default()
        };
        let mut sched = RefreshScheduler::new(cfg);
        for _ in 0..50 {
            sched.submit(Rect::new(0, 0, 4, 4), DamageSource::Ink);
            assert_eq!(modes(&sched.flush()), Some(RefreshMode::Fast));
        }
    }

    #[test]
    fn adjacent_damage_coalesces_into_one() {
        let mut sched = RefreshScheduler::new(RefreshConfig {
            coalesce_gap: 16,
            ..RefreshConfig::default()
        });
        // Three rects marching right, each within the gap of the previous.
        sched.submit(Rect::new(0, 0, 10, 10), DamageSource::Ink);
        sched.submit(Rect::new(12, 0, 10, 10), DamageSource::Ink);
        sched.submit(Rect::new(24, 0, 10, 10), DamageSource::Ink);

        let present = sched.flush().unwrap();
        assert_eq!(present.damage.len(), 1, "chained rects merge transitively");
        assert_eq!(present.damage[0], Rect::new(0, 0, 34, 10));
    }

    #[test]
    fn distant_damage_stays_separate() {
        let mut sched = RefreshScheduler::new(RefreshConfig {
            coalesce_gap: 4,
            ..RefreshConfig::default()
        });
        sched.submit(Rect::new(0, 0, 10, 10), DamageSource::Ui);
        sched.submit(Rect::new(500, 500, 10, 10), DamageSource::Ui);

        let present = sched.flush().unwrap();
        assert_eq!(present.damage.len(), 2);
    }

    #[test]
    fn too_many_rects_union_into_bounding_box() {
        let mut sched = RefreshScheduler::new(RefreshConfig {
            coalesce_gap: 0,
            max_damage_rects: 3,
            ..RefreshConfig::default()
        });
        // Five well-separated rects — none coalesce, exceeding max → single box.
        for i in 0..5u32 {
            sched.submit(Rect::new(i * 100, 0, 10, 10), DamageSource::Ui);
        }
        let present = sched.flush().unwrap();
        assert_eq!(present.damage.len(), 1);
        // Bounding box spans x:[0, 410], y:[0, 10].
        assert_eq!(present.damage[0], Rect::new(0, 0, 410, 10));
    }

    #[test]
    fn flush_resets_pending() {
        let mut sched = RefreshScheduler::default();
        sched.submit(Rect::new(0, 0, 10, 10), DamageSource::Ink);
        assert!(sched.has_pending());
        sched.flush();
        assert!(!sched.has_pending());
        assert_eq!(sched.flush(), None);
    }
}
