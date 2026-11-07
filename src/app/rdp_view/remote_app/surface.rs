//! Retain obscured pixels separately for each Standard RemoteApp window.
use rustrdp::remote_app::{Rect, RemoteWindow};
use rustterm::rdp::{RdpFrame, RdpTile};
use std::sync::{Arc, Weak};

const TILE: u16 = 510;

#[derive(Default)]
pub(super) struct Surface {
    pub tiles: Vec<Arc<RdpTile>>,
    pub size: (u16, u16),
    last_frame: u64,
    last_window: Option<RemoteWindow>,
    last_sources: Vec<Weak<RdpTile>>,
}

/// With local decorations, render only the remote client area. Popup windows
/// retain their entire surface, including their server-drawn menu borders.
pub(super) fn content(window: &RemoteWindow) -> Rect {
    let mut rect = window.bounds();
    if decorated(window) && window.client_size.0 > 0 && window.client_size.1 > 0 {
        rect.left = window.client_offset.0;
        rect.top = window.client_offset.1;
        rect.right = rect.left.saturating_add(window.client_size.0 as i32);
        rect.bottom = rect.top.saturating_add(window.client_size.1 as i32);
    }
    rect
}

pub(super) fn decorated(window: &RemoteWindow) -> bool {
    window.style & 0x00c0_0000 == 0x00c0_0000
        && window.client_size.0 > 0
        && window.client_size.1 > 0
}

pub(super) fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let r = Rect {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    };
    (r.left < r.right && r.top < r.bottom).then_some(r)
}

impl Surface {
    pub fn update(&mut self, window: &RemoteWindow, frame: &RdpFrame) {
        if self.last_frame == frame.sequence && self.last_window.as_ref() == Some(window) {
            return;
        }
        let unchanged_geometry = self.last_window.as_ref() == Some(window);
        self.last_frame = frame.sequence;
        self.last_window = Some(window.clone());
        let region = content(window);
        let size = (
            (region.right - region.left).max(0) as u16,
            (region.bottom - region.top).max(0) as u16,
        );
        if self.size != size {
            self.size = size;
            self.tiles.clear();
            for y in (0..size.1).step_by(usize::from(TILE)) {
                for x in (0..size.0).step_by(usize::from(TILE)) {
                    let width = TILE.min(size.0 - x);
                    let height = TILE.min(size.1 - y);
                    self.tiles.push(Arc::new(RdpTile {
                        x,
                        y,
                        width,
                        height,
                        pixels: vec![0; usize::from(width) * usize::from(height) * 4],
                    }));
                }
            }
        }
        let visible: Vec<_> = match &window.visibility {
            Some(rects) => rects
                .iter()
                .filter_map(|r| {
                    intersect(
                        region,
                        Rect {
                            left: r.left.saturating_add(window.offset.0),
                            top: r.top.saturating_add(window.offset.1),
                            right: r.right.saturating_add(window.offset.0),
                            bottom: r.bottom.saturating_add(window.offset.1),
                        },
                    )
                })
                .collect(),
            None => Vec::new(), // Never copy another application's pixels before visibility arrives.
        };
        for destination in &mut self.tiles {
            let bounds = Rect {
                left: region.left + i32::from(destination.x),
                top: region.top + i32::from(destination.y),
                right: region.left + i32::from(destination.x) + i32::from(destination.width),
                bottom: region.top + i32::from(destination.y) + i32::from(destination.height),
            };
            let mut pixels = None;
            for (source_index, source) in frame.tiles.iter().enumerate() {
                if unchanged_geometry
                    && self
                        .last_sources
                        .get(source_index)
                        .is_some_and(|previous| previous.ptr_eq(&Arc::downgrade(source)))
                {
                    continue;
                }
                let source_bounds = Rect {
                    left: i32::from(source.x),
                    top: i32::from(source.y),
                    right: i32::from(source.x) + i32::from(source.width),
                    bottom: i32::from(source.y) + i32::from(source.height),
                };
                let Some(shared) = intersect(bounds, source_bounds) else {
                    continue;
                };
                for visible in &visible {
                    let Some(copy) = intersect(shared, *visible) else {
                        continue;
                    };
                    let bytes = (copy.right - copy.left) as usize * 4;
                    for y in copy.top..copy.bottom {
                        let src = ((y - source_bounds.top) as usize * usize::from(source.width)
                            + (copy.left - source_bounds.left) as usize)
                            * 4;
                        let dst = ((y - bounds.top) as usize * usize::from(destination.width)
                            + (copy.left - bounds.left) as usize)
                            * 4;
                        if destination.pixels[dst..dst + bytes] != source.pixels[src..src + bytes] {
                            pixels.get_or_insert_with(|| destination.pixels.clone())
                                [dst..dst + bytes]
                                .copy_from_slice(&source.pixels[src..src + bytes]);
                        }
                    }
                }
            }
            if let Some(pixels) = pixels {
                *destination = Arc::new(RdpTile {
                    x: destination.x,
                    y: destination.y,
                    width: destination.width,
                    height: destination.height,
                    pixels,
                });
            }
        }
        self.last_sources = frame.tiles.iter().map(Arc::downgrade).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> RemoteWindow {
        RemoteWindow {
            id: 42,
            offset: (1, 1),
            visible_offset: (1, 1),
            size: (2, 2),
            show_state: 5,
            visibility: Some(vec![Rect {
                left: 0,
                top: 0,
                right: 2,
                bottom: 2,
            }]),
            ..Default::default()
        }
    }

    fn frame(sequence: u64, pixel: u8) -> RdpFrame {
        RdpFrame {
            sequence,
            published_at: std::time::Instant::now(),
            acknowledgement: None,
            width: 4,
            height: 4,
            bits_per_pixel: 32,
            tiles: vec![Arc::new(RdpTile {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
                pixels: vec![pixel; 64],
            })],
            hide_local_pointer: false,
            remote_windows: vec![],
        }
    }

    #[test]
    fn occluding_windows_cannot_overwrite_cached_application_pixels() {
        let mut surface = Surface::default();
        let mut remote = window();
        surface.update(&remote, &frame(1, 7));
        assert_eq!(surface.tiles[0].pixels, vec![7; 16]);
        remote.visibility = Some(vec![Rect {
            left: 0,
            top: 0,
            right: 1,
            bottom: 2,
        }]);
        surface.update(&remote, &frame(2, 9));
        assert_eq!(
            surface.tiles[0].pixels,
            vec![9, 9, 9, 9, 7, 7, 7, 7, 9, 9, 9, 9, 7, 7, 7, 7]
        );
        remote.visibility = Some(vec![]);
        let retained = surface.tiles[0].clone();
        surface.update(&remote, &frame(3, 2));
        assert!(Arc::ptr_eq(&retained, &surface.tiles[0]));
    }

    #[test]
    fn client_origin_is_absolute_and_unchanged_tiles_are_shared() {
        let mut remote = window();
        remote.size = (4, 4);
        remote.style = 0x00c0_0000;
        remote.client_offset = (2, 2);
        remote.client_size = (2, 2);
        assert_eq!(
            content(&remote),
            Rect {
                left: 2,
                top: 2,
                right: 4,
                bottom: 4
            }
        );
        let mut surface = Surface::default();
        surface.update(&remote, &frame(1, 1));
        let retained = surface.tiles[0].clone();
        surface.update(&remote, &frame(2, 1));
        assert!(Arc::ptr_eq(&retained, &surface.tiles[0]));
    }

    #[test]
    fn missing_visibility_and_offscreen_pixels_stay_blank() {
        let mut remote = window();
        remote.offset = (-1, -1);
        remote.visible_offset = remote.offset;
        remote.visibility = None;
        let mut surface = Surface::default();
        surface.update(&remote, &frame(1, 9));
        assert!(surface.tiles[0].pixels.iter().all(|p| *p == 0));
        remote.visibility = Some(vec![Rect {
            left: 0,
            top: 0,
            right: 2,
            bottom: 2,
        }]);
        surface.update(&remote, &frame(2, 9));
        assert_eq!(
            surface.tiles[0].pixels,
            vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 9, 9, 9]
        );
    }
}
