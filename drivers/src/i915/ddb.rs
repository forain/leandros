//! Display buffer (DDB) split, pure arithmetic (host-tested in
//! drivers/i915-tests). Linux gen9 skl_ddb_get_pipe_allocation_limits and the
//! single-plane-plus-cursor case of skl_crtc_allocate_plane_ddb.

/// skl_ddb_entry_write: start | (end - 1) << 16, end exclusive; 0 = empty.
pub fn ddb_entry(start: u32, end: u32) -> u32 { if end == 0 { 0 } else { (end - 1) << 16 | start } }

/// The pipe's slice of the DDB: each active pipe is weighted by its
/// hdisplay (`widths[p] == 0` = pipe off). `[start, end)`.
pub fn pipe_limits(ddb_size: u32, widths: &[u32], pipe: usize) -> Option<(u32, u32)> {
    let total: u64 = widths.iter().map(|&w| w as u64).sum();
    if pipe >= widths.len() || widths[pipe] == 0 { return None; }
    let before: u64 = widths[..pipe].iter().map(|&w| w as u64).sum();
    let start = (ddb_size as u64 * before / total) as u32;
    let end = (ddb_size as u64 * (before + widths[pipe] as u64) / total) as u32;
    Some((start, end))
}

/// skl_ddb_get_pipe_allocation_limits (display 9: no DBuf slices) and
/// skl_crtc_allocate_plane_ddb for one universal plane plus the cursor: the
/// cursor gets max(32 with one active pipe else 8, `cursor_min`) blocks at
/// the top of the pipe's range, the single plane everything below it (the
/// data-rate split of the left-over blocks gives one plane all of them).
/// Returns `(plane, cursor)` as `[start, end)`.
pub fn skl_ddb_plan(ddb_size: u32, widths: &[u32], pipe: usize, cursor_min: u32) -> Option<((u32, u32), (u32, u32))> {
    let (start, end) = pipe_limits(ddb_size, widths, pipe)?;
    let active = widths.iter().filter(|&&w| w != 0).count();
    let cur = cursor_min.max(if active == 1 { 32 } else { 8 });
    if end <= start + cur { return None; }
    Some(((start, end - cur), (end - cur, end)))
}

/// Do two `[start, end)` ranges share a block?
pub fn overlaps(a: (u32, u32), b: (u32, u32)) -> bool { a.0 < b.1 && b.0 < a.1 && a.0 < a.1 && b.0 < b.1 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pipe_takes_everything() {
        assert_eq!(skl_ddb_plan(892, &[1920], 0, 0), Some(((0, 860), (860, 892))));
        assert_eq!(skl_ddb_plan(892, &[1920, 0, 0], 0, 0), Some(((0, 860), (860, 892))));
        assert_eq!(skl_ddb_plan(892, &[0, 1920], 0, 0), None);
    }

    #[test]
    fn equal_widths_split_in_half() {
        // Linux: 892 * 1920 / 3840 = 446 per pipe, cursor 8.
        assert_eq!(pipe_limits(892, &[1920, 1920], 0), Some((0, 446)));
        assert_eq!(pipe_limits(892, &[1920, 1920], 1), Some((446, 892)));
        assert_eq!(skl_ddb_plan(892, &[1920, 1920], 0, 0), Some(((0, 438), (438, 446))));
        assert_eq!(skl_ddb_plan(892, &[1920, 1920], 1, 0), Some(((446, 884), (884, 892))));
    }

    #[test]
    fn proportional_to_width() {
        // 1920 + 1280: A = 892*1920/3200 = 535, B = rest.
        assert_eq!(pipe_limits(892, &[1920, 1280], 0), Some((0, 535)));
        assert_eq!(pipe_limits(892, &[1920, 1280], 1), Some((535, 892)));
        // three pipes: ranges tile the DDB without gaps or overlaps
        let w = [1920, 1366, 3840];
        let mut prev = 0;
        for p in 0..3 {
            let (s, e) = pipe_limits(892, &w, p).unwrap();
            assert_eq!(s, prev);
            assert!(e > s);
            prev = e;
        }
        assert_eq!(prev, 892);
    }

    #[test]
    fn cursor_min_and_too_small() {
        assert_eq!(skl_ddb_plan(892, &[1920, 1920], 0, 16), Some(((0, 430), (430, 446))));
        assert_eq!(skl_ddb_plan(10, &[1, 1], 0, 8), None);
    }

    #[test]
    fn entry_encoding_and_overlap() {
        assert_eq!(ddb_entry(0, 446), 445 << 16);
        assert_eq!(ddb_entry(446, 892), 891 << 16 | 446);
        assert_eq!(ddb_entry(0, 0), 0);
        assert!(!overlaps((0, 446), (446, 892)));
        assert!(overlaps((0, 447), (446, 892)));
        assert!(!overlaps((5, 5), (0, 10)));
    }
}
