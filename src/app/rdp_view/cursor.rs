//! Local cursor composition, independent of desktop decoding and publication.
use super::*;
use gpui::{ContentMask, point, size};
use rustrdp::graphics::{Pointer, PointerState};

#[derive(Default)]
pub(super) struct PaintStats {
    pub uploads: u64,
    pub bytes: u64,
    pub errors: u64,
}

#[derive(Default)]
pub(super) struct CursorOverlay {
    state: Option<PointerState>,
    position: Option<(u16, u16)>,
    frame: Option<Arc<RdpFrame>>,
    prepared: Option<(u64, (u16, u16))>,
    image: Option<Arc<RenderImage>>,
    uploaded: bool,
    origin: (i32, i32),
    size: (u16, u16),
}

impl CursorOverlay {
    pub fn update(&mut self, state: Option<PointerState>) {
        if state.as_ref().map(|s| s.position_revision)
            != self.state.as_ref().map(|s| s.position_revision)
        {
            self.position = state.as_ref().map(|s| s.position);
        }
        self.prepared = None;
        self.state = state;
    }

    pub fn move_to(&mut self, position: (u16, u16)) -> bool {
        let changed = self.position != Some(position);
        self.position = Some(position);
        changed
            && self
                .state
                .as_ref()
                .is_some_and(|s| matches!(s.image, Pointer::Custom(_)))
    }

    pub fn hides_platform_cursor(&self) -> Option<bool> {
        self.state
            .as_ref()
            .map(|s| !matches!(s.image, Pointer::Default))
    }

    pub fn frame(&mut self, frame: Arc<RdpFrame>) {
        self.frame = Some(frame);
    }

    pub fn clear(&mut self, window: &Window) {
        super::render::retire_images(window, self.image.take().into_iter().collect());
        self.frame = None;
        self.prepared = None;
    }

    pub fn paint(
        &mut self,
        desktop: Bounds<Pixels>,
        visible: Bounds<Pixels>,
        window: &mut Window,
    ) -> PaintStats {
        let Some(PointerState {
            image: Pointer::Custom(shape),
            ..
        }) = &self.state
        else {
            super::render::retire_images(window, self.image.take().into_iter().collect());
            self.prepared = None;
            return PaintStats::default();
        };
        let (Some(frame), Some((x, y))) = (&self.frame, self.position) else {
            return PaintStats::default();
        };
        if frame.width == 0 || frame.height == 0 {
            return PaintStats::default();
        }
        let position = (x.min(frame.width - 1), y.min(frame.height - 1));
        let key = (frame.sequence, position);
        if self.prepared != Some(key) {
            let (width, height) = shape.size();
            let hotspot = shape.hotspot();
            let origin = (
                i32::from(position.0) - i32::from(hotspot.0),
                i32::from(position.1) - i32::from(hotspot.1),
            );
            let mut pixels = Vec::with_capacity(usize::from(width) * usize::from(height) * 4);
            for y in 0..height {
                for x in 0..width {
                    pixels.extend_from_slice(&background(
                        frame,
                        origin.0 + i32::from(x),
                        origin.1 + i32::from(y),
                    ));
                }
            }
            // Preserve alpha and legacy screen-AND/XOR pixels exactly, using
            // the same immutable desktop snapshot that is painted underneath.
            if shape.composite(&mut pixels).is_err() {
                return PaintStats {
                    errors: 1,
                    ..PaintStats::default()
                };
            }
            let tile = rustterm::rdp::RdpTile {
                x: 0,
                y: 0,
                width,
                height,
                pixels,
            };
            let image = Arc::new(RenderImage::new(vec![image::Frame::new(
                super::render::tile_buffer(&tile),
            )]));
            super::render::retire_images(window, self.image.replace(image).into_iter().collect());
            self.origin = origin;
            self.size = (width, height);
            self.prepared = Some(key);
            self.uploaded = false;
        }
        let Some(image) = &self.image else {
            return PaintStats::default();
        };
        let bounds = cursor_bounds(desktop, (frame.width, frame.height), self.origin, self.size);
        let clipped = bounds.intersect(&visible);
        if clipped.is_empty() {
            return PaintStats::default();
        }
        let padded = super::render::padded_tile_bounds(bounds, size(self.size.0, self.size.1));
        let painted = window.with_content_mask(Some(ContentMask { bounds: clipped }), |window| {
            window.paint_image(padded, Default::default(), image.clone(), 0, false)
        });
        match painted {
            Ok(()) if !self.uploaded => {
                self.uploaded = true;
                PaintStats {
                    uploads: 1,
                    bytes: (u64::from(self.size.0) + 2) * (u64::from(self.size.1) + 2) * 4,
                    errors: 0,
                }
            }
            Ok(()) => PaintStats::default(),
            Err(_) => PaintStats {
                errors: 1,
                ..PaintStats::default()
            },
        }
    }
}

fn cursor_bounds(
    desktop: Bounds<Pixels>,
    frame: (u16, u16),
    origin: (i32, i32),
    dimensions: (u16, u16),
) -> Bounds<Pixels> {
    let sx = desktop.size.width / f32::from(frame.0);
    let sy = desktop.size.height / f32::from(frame.1);
    Bounds::new(
        point(
            desktop.left() + sx * origin.0 as f32,
            desktop.top() + sy * origin.1 as f32,
        ),
        size(sx * f32::from(dimensions.0), sy * f32::from(dimensions.1)),
    )
}

fn background(frame: &RdpFrame, x: i32, y: i32) -> [u8; 4] {
    if x < 0 || y < 0 || x >= i32::from(frame.width) || y >= i32::from(frame.height) {
        return [0, 0, 0, 255];
    }
    let (x, y) = (x as u16, y as u16);
    let side = rustterm::rdp::RDP_TILE_SIZE;
    let columns = usize::from(frame.width.div_ceil(side));
    let index = usize::from(y / side) * columns + usize::from(x / side);
    let Some(tile) = frame.tiles.get(index) else {
        return [0, 0, 0, 255];
    };
    let offset = (usize::from(y - tile.y) * usize::from(tile.width) + usize::from(x - tile.x)) * 4;
    tile.pixels[offset..offset + 4]
        .try_into()
        .expect("validated desktop tile")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    #[test]
    fn cursor_hotspot_tracks_desktop_edges_during_zoom_and_pan() {
        for scale in [0.5, 1.0, 2.0] {
            let desktop = Bounds::new(
                point(px(-50.0), px(30.0)),
                size(px(1920.0 * scale), px(1080.0 * scale)),
            );
            let hotspot = (7, 9);
            for position in [(0, 0), (1919, 1079)] {
                let origin = (position.0 - hotspot.0, position.1 - hotspot.1);
                let bounds = cursor_bounds(desktop, (1920, 1080), origin, (32, 32));
                assert_eq!(
                    bounds.left() + px(hotspot.0 as f32 * scale),
                    desktop.left() + px(position.0 as f32 * scale)
                );
                assert_eq!(
                    bounds.top() + px(hotspot.1 as f32 * scale),
                    desktop.top() + px(position.1 as f32 * scale)
                );
                let visible = bounds.intersect(&desktop);
                assert!(!visible.is_empty());
                assert!(visible.left() >= desktop.left() && visible.right() <= desktop.right());
                assert!(visible.top() >= desktop.top() && visible.bottom() <= desktop.bottom());
            }
        }
    }

    #[test]
    fn shape_changes_preserve_local_motion_but_server_warps_replace_it() {
        let mut cursor = CursorOverlay::default();
        cursor.update(Some(PointerState {
            image: Pointer::Default,
            position: (4, 5),
            position_revision: 0,
        }));
        assert_eq!(cursor.position, Some((4, 5)));
        cursor.move_to((20, 30));
        cursor.update(Some(PointerState {
            image: Pointer::Hidden,
            position: (7, 8),
            position_revision: 0,
        }));
        assert_eq!(cursor.position, Some((20, 30)));
        assert_eq!(cursor.hides_platform_cursor(), Some(true));
        cursor.update(Some(PointerState {
            image: Pointer::Default,
            position: (9, 10),
            position_revision: 1,
        }));
        assert_eq!(cursor.position, Some((9, 10)));
        assert_eq!(cursor.hides_platform_cursor(), Some(false));
        cursor.update(None);
        assert_eq!(cursor.position, None);
        assert_eq!(cursor.hides_platform_cursor(), None);
    }

    #[test]
    fn cursor_background_reads_across_partial_desktop_tiles_and_clips_hotspots() {
        let side = rustterm::rdp::RDP_TILE_SIZE;
        let width = side + 2;
        let height = side + 1;
        let mut tiles = Vec::new();
        for (y, h) in [(0, side), (side, 1)] {
            for (x, w) in [(0, side), (side, 2)] {
                let mut pixels = Vec::new();
                for yy in y..y + h {
                    for xx in x..x + w {
                        pixels.extend([xx as u8, yy as u8, 123, 255]);
                    }
                }
                tiles.push(Arc::new(rustterm::rdp::RdpTile {
                    x,
                    y,
                    width: w,
                    height: h,
                    pixels,
                }));
            }
        }
        let frame = RdpFrame {
            width,
            height,
            bits_per_pixel: 32,
            tiles,
            remote_windows: Vec::new(),
            sequence: 1,
            published_at: std::time::Instant::now(),
            acknowledgement: None,
            hide_local_pointer: true,
        };
        for (x, y) in [
            (0, 0),
            (side - 1, side - 1),
            (side, side),
            (width - 1, height - 1),
        ] {
            assert_eq!(
                background(&frame, i32::from(x), i32::from(y)),
                [x as u8, y as u8, 123, 255]
            );
        }
        for (x, y) in [
            (-1, 0),
            (0, -1),
            (i32::from(width), 0),
            (0, i32::from(height)),
        ] {
            assert_eq!(background(&frame, x, y), [0, 0, 0, 255]);
        }
    }
}
