//! Validated virtual desktop topology, shared by initial and dynamic negotiation.
use crate::{Error, Limits, Result};

/// A monitor in remote desktop coordinates. Exactly one monitor starts at (0, 0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    /// Signed horizontal position relative to the primary monitor.
    pub left: i32,
    /// Signed vertical position relative to the primary monitor.
    pub top: i32,
    /// Even pixel width, between 200 and 8192.
    pub width: u16,
    /// Pixel height, between 200 and 8192.
    pub height: u16,
    /// Identifies the unique monitor whose origin is (0, 0).
    pub primary: bool,
    /// Desktop scaling percentage, between 100 and 500.
    pub scale: u32,
}
impl Monitor {
    /// Create an unscaled primary monitor at the desktop origin.
    pub fn primary(width: u16, height: u16) -> Self {
        Self {
            left: 0,
            top: 0,
            width,
            height,
            primary: true,
            scale: 100,
        }
    }
}

pub(crate) fn validate(monitors: &[Monitor], limits: Limits) -> Result<(u16, u16)> {
    if monitors.is_empty()
        || monitors.len() > 16
        || monitors.iter().filter(|m| m.primary).count() != 1
    {
        return Err(Error::Invalid("monitor count or primary monitor"));
    }
    let mut left = i64::MAX;
    let mut top = i64::MAX;
    let mut right = i64::MIN;
    let mut bottom = i64::MIN;
    for (index, monitor) in monitors.iter().enumerate() {
        if !(200..=8192).contains(&monitor.width)
            || !(200..=8192).contains(&monitor.height)
            || !monitor.width.is_multiple_of(2)
            || !(100..=500).contains(&monitor.scale)
            || (monitor.primary && (monitor.left != 0 || monitor.top != 0))
        {
            return Err(Error::Invalid("monitor dimensions or origin"));
        }
        let x = i64::from(monitor.left);
        let y = i64::from(monitor.top);
        let r = x + i64::from(monitor.width);
        let b = y + i64::from(monitor.height);
        for previous in &monitors[..index] {
            if x < i64::from(previous.left) + i64::from(previous.width)
                && r > i64::from(previous.left)
                && y < i64::from(previous.top) + i64::from(previous.height)
                && b > i64::from(previous.top)
            {
                return Err(Error::Invalid("overlapping monitors"));
            }
        }
        left = left.min(x);
        top = top.min(y);
        right = right.max(r);
        bottom = bottom.max(b);
    }
    let width = u16::try_from(right - left).map_err(|_| Error::Limit("monitor desktop width"))?;
    let height = u16::try_from(bottom - top).map_err(|_| Error::Limit("monitor desktop height"))?;
    limits.frame_bytes(width, height)?;
    // All monitors must touch the connected desktop, including corner adjacency.
    let mut reached = vec![false; monitors.len()];
    reached[0] = true;
    for _ in 0..monitors.len() {
        for i in 0..monitors.len() {
            if !reached[i] {
                reached[i] = monitors
                    .iter()
                    .enumerate()
                    .any(|(j, b)| reached[j] && adjacent(&monitors[i], b));
            }
        }
    }
    if reached.contains(&false) {
        return Err(Error::Invalid("disconnected monitor layout"));
    }
    Ok((width, height))
}
fn adjacent(a: &Monitor, b: &Monitor) -> bool {
    let (ax, ay, bx, by) = (
        i64::from(a.left),
        i64::from(a.top),
        i64::from(b.left),
        i64::from(b.top),
    );
    ax <= bx + i64::from(b.width)
        && ax + i64::from(a.width) >= bx
        && ay <= by + i64::from(b.height)
        && ay + i64::from(a.height) >= by
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_allow_negative_origins_but_reject_overlap_and_detached_monitors() {
        let mut monitors = vec![
            Monitor::primary(1280, 720),
            Monitor {
                left: -1280,
                top: 0,
                width: 1280,
                height: 720,
                primary: false,
                scale: 100,
            },
        ];
        assert_eq!(validate(&monitors, Limits::default()).unwrap(), (2560, 720));
        monitors[1].left = -1278;
        assert!(validate(&monitors, Limits::default()).is_err());
        monitors[1].left = -1300;
        assert!(validate(&monitors, Limits::default()).is_err());
    }
}
