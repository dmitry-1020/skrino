//! Pure region/monitor geometry. No OS calls here so it is unit-tested on any
//! platform.
//!
//! Coordinates are physical pixels in virtual-screen space (origin at the
//! primary monitor's top-left, negative to the left/above), matching what the
//! selection overlay and `skrino-capture` produce. WGC captures one monitor at
//! a time, so a requested region is resolved to a single monitor and expressed
//! in that monitor's local pixels, with width/height forced even (H.264 needs
//! even dimensions) and never smaller than [`MIN_DIM`].

/// Minimum crop side. H.264 tolerates small frames, but sub-32px regions are
/// almost always an accidental click; we widen them to something encodable.
pub(crate) const MIN_DIM: u32 = 32;

/// A rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    fn center(&self) -> (i32, i32) {
        (self.x + self.width as i32 / 2, self.y + self.height as i32 / 2)
    }

    fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
}

/// Round down to the nearest even number (clearing the low bit).
pub(crate) fn even_floor(v: u32) -> u32 {
    v & !1
}

/// Area of the overlap of two rectangles (0 when disjoint). `i64` to avoid
/// overflow on large virtual desktops.
fn intersect_area(a: &Rect, b: &Rect) -> i64 {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = a.right().min(b.right());
    let y1 = a.bottom().min(b.bottom());
    if x1 > x0 && y1 > y0 {
        (x1 - x0) as i64 * (y1 - y0) as i64
    } else {
        0
    }
}

/// Pick the monitor that owns a region: the one containing its center, else the
/// one it overlaps most, else the first. Never panics for a non-empty slice.
pub(crate) fn choose_monitor(monitors: &[Rect], region: &Rect) -> usize {
    let (cx, cy) = region.center();
    if let Some(i) = monitors.iter().position(|m| m.contains(cx, cy)) {
        return i;
    }
    let mut best = 0usize;
    let mut best_area = -1i64;
    for (i, m) in monitors.iter().enumerate() {
        let area = intersect_area(m, region);
        if area > best_area {
            best_area = area;
            best = i;
        }
    }
    best
}

/// Resolved capture geometry: which monitor to capture and the crop rectangle
/// in that monitor's local pixels. `width`/`height` are even and >= [`MIN_DIM`]
/// (clamped to the monitor), so they can be handed straight to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CropPlan {
    pub monitor_index: usize,
    pub local_x: u32,
    pub local_y: u32,
    pub width: u32,
    pub height: u32,
}

/// Full-monitor plan (used for `region == None` and as a fallback when a region
/// does not actually intersect its chosen monitor).
fn full_monitor_plan(index: usize, m: Rect) -> CropPlan {
    CropPlan {
        monitor_index: index,
        local_x: 0,
        local_y: 0,
        width: even_floor(m.width).max(MIN_DIM),
        height: even_floor(m.height).max(MIN_DIM),
    }
}

/// Turn a virtual-screen region (or `None` for the primary monitor) into a
/// single-monitor [`CropPlan`]. Returns `None` only when `monitors` is empty.
pub(crate) fn plan_region(
    monitors: &[Rect],
    region: Option<Rect>,
    primary_index: usize,
) -> Option<CropPlan> {
    if monitors.is_empty() {
        return None;
    }
    let primary_index = primary_index.min(monitors.len() - 1);

    let Some(region) = region else {
        return Some(full_monitor_plan(primary_index, monitors[primary_index]));
    };

    let idx = choose_monitor(monitors, &region);
    let m = monitors[idx];

    // Clamp the region to the monitor in virtual coordinates.
    let x0 = region.x.max(m.x);
    let y0 = region.y.max(m.y);
    let x1 = region.right().min(m.right());
    let y1 = region.bottom().min(m.bottom());
    if x1 <= x0 || y1 <= y0 {
        // Center-chosen monitor with no positive overlap: record it whole.
        return Some(full_monitor_plan(idx, m));
    }

    let monitor_w = even_floor(m.width).max(MIN_DIM);
    let monitor_h = even_floor(m.height).max(MIN_DIM);

    // Even, minimum-enforced, never larger than the monitor.
    let mut w = even_floor((x1 - x0) as u32).max(MIN_DIM).min(monitor_w);
    let mut h = even_floor((y1 - y0) as u32).max(MIN_DIM).min(monitor_h);
    w = even_floor(w);
    h = even_floor(h);

    // Local offset; shift back inside the monitor if the min/even widening
    // pushed the crop past the right/bottom edge.
    let mut local_x = (x0 - m.x) as u32;
    let mut local_y = (y0 - m.y) as u32;
    if local_x + w > m.width {
        local_x = m.width.saturating_sub(w);
    }
    if local_y + h > m.height {
        local_y = m.height.saturating_sub(h);
    }

    Some(CropPlan {
        monitor_index: idx,
        local_x,
        local_y,
        width: w,
        height: h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }

    #[test]
    fn even_floor_rounds_down_to_even() {
        assert_eq!(even_floor(0), 0);
        assert_eq!(even_floor(1), 0);
        assert_eq!(even_floor(2), 2);
        assert_eq!(even_floor(1081), 1080);
        assert_eq!(even_floor(1920), 1920);
    }

    #[test]
    fn none_region_records_primary_monitor_evened() {
        let monitors = [r(0, 0, 1920, 1080), r(1920, 0, 2560, 1440)];
        let plan = plan_region(&monitors, None, 0).unwrap();
        assert_eq!(
            plan,
            CropPlan { monitor_index: 0, local_x: 0, local_y: 0, width: 1920, height: 1080 }
        );
    }

    #[test]
    fn none_region_honors_primary_index() {
        let monitors = [r(0, 0, 1920, 1080), r(1920, 0, 2560, 1440)];
        let plan = plan_region(&monitors, None, 1).unwrap();
        assert_eq!(plan.monitor_index, 1);
        assert_eq!((plan.width, plan.height), (2560, 1440));
    }

    #[test]
    fn odd_monitor_size_is_evened() {
        let monitors = [r(0, 0, 1367, 769)];
        let plan = plan_region(&monitors, None, 0).unwrap();
        assert_eq!((plan.width, plan.height), (1366, 768));
    }

    #[test]
    fn region_inside_single_monitor_maps_to_local_coords() {
        let monitors = [r(0, 0, 1920, 1080)];
        let plan = plan_region(&monitors, Some(r(100, 200, 640, 480)), 0).unwrap();
        assert_eq!(
            plan,
            CropPlan { monitor_index: 0, local_x: 100, local_y: 200, width: 640, height: 480 }
        );
    }

    #[test]
    fn odd_region_dimensions_are_floored_even() {
        let monitors = [r(0, 0, 1920, 1080)];
        let plan = plan_region(&monitors, Some(r(10, 10, 641, 481)), 0).unwrap();
        assert_eq!((plan.width, plan.height), (640, 480));
        assert_eq!((plan.local_x, plan.local_y), (10, 10));
    }

    #[test]
    fn tiny_region_is_widened_to_minimum() {
        let monitors = [r(0, 0, 1920, 1080)];
        let plan = plan_region(&monitors, Some(r(500, 500, 10, 8)), 0).unwrap();
        assert_eq!((plan.width, plan.height), (MIN_DIM, MIN_DIM));
    }

    #[test]
    fn region_at_right_edge_shifts_back_inside_monitor() {
        let monitors = [r(0, 0, 1920, 1080)];
        // 10px wide near the far right; widening to 32 would overflow, so the
        // origin must move left to keep the crop on the monitor.
        let plan = plan_region(&monitors, Some(r(1915, 500, 10, 10)), 0).unwrap();
        assert_eq!((plan.width, plan.height), (MIN_DIM, MIN_DIM));
        assert!(plan.local_x + plan.width <= 1920);
        assert_eq!(plan.local_x, 1920 - MIN_DIM);
    }

    #[test]
    fn region_partly_off_monitor_is_clamped() {
        let monitors = [r(0, 0, 1920, 1080)];
        // Starts left of and above the monitor, extends onto it.
        let plan = plan_region(&monitors, Some(r(-100, -50, 400, 300)), 0).unwrap();
        assert_eq!((plan.local_x, plan.local_y), (0, 0));
        // Visible part is 300x250, evened.
        assert_eq!((plan.width, plan.height), (300, 250));
    }

    #[test]
    fn region_spanning_two_monitors_picks_the_one_with_the_center() {
        let monitors = [r(0, 0, 1920, 1080), r(1920, 0, 1920, 1080)];
        // Center at x = 1800 -> left monitor.
        let plan = plan_region(&monitors, Some(r(1600, 100, 400, 200)), 0).unwrap();
        assert_eq!(plan.monitor_index, 0);
        assert_eq!(plan.local_x, 1600);
        // Clamped to the left monitor's right edge: 1920 - 1600 = 320.
        assert_eq!(plan.width, 320);
    }

    #[test]
    fn negative_coordinate_monitor_maps_to_local() {
        // Secondary monitor sits to the left of the primary.
        let monitors = [r(0, 0, 1920, 1080), r(-1920, 0, 1920, 1080)];
        let plan = plan_region(&monitors, Some(r(-1800, 100, 200, 200)), 0).unwrap();
        assert_eq!(plan.monitor_index, 1);
        assert_eq!((plan.local_x, plan.local_y), (120, 100));
        assert_eq!((plan.width, plan.height), (200, 200));
    }

    #[test]
    fn center_in_gap_falls_back_to_largest_overlap() {
        // Two monitors with a gap between them; center lands in the gap.
        let monitors = [r(0, 0, 1000, 1000), r(2000, 0, 1000, 1000)];
        // Region 900..2100, center 1500 (in the gap). Overlaps left by 100px
        // wide, right by 100px wide, equal area -> first wins deterministically.
        let plan = plan_region(&monitors, Some(r(900, 100, 1200, 200)), 0).unwrap();
        assert!(plan.monitor_index == 0 || plan.monitor_index == 1);
    }

    #[test]
    fn empty_monitor_list_returns_none() {
        assert!(plan_region(&[], None, 0).is_none());
        assert!(plan_region(&[], Some(r(0, 0, 100, 100)), 0).is_none());
    }
}
