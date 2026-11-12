//! Compositor cursors do not wait for a desktop/UI frame on pointer motion.
//! GPUI owns the hit-tested cursor style (None inside the remote desktop), and
//! the native image is installed after that style has been applied.
use gpui::{Bounds, Pixels, Window};
use rustrdp::graphics::PointerShape;
use std::sync::Arc;

#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
use wayland::Cursor as Backend;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::Cursor as Backend;

pub(super) struct Image {
    width: u32,
    height: u32,
    hotspot: (i32, i32),
    scale: i32,
    pixels: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    width: u32,
    height: u32,
    hotspot: (i32, i32),
    scale: i32,
}

impl Geometry {
    fn new(
        size: (u16, u16),
        hotspot: (u16, u16),
        scale: (f32, f32),
        device_scale: f32,
    ) -> Option<Self> {
        Self::scaled(
            size,
            hotspot,
            scale,
            device_scale,
            cfg!(target_os = "linux"),
        )
    }

    fn scaled(
        size: (u16, u16),
        hotspot: (u16, u16),
        scale: (f32, f32),
        device_scale: f32,
        surface_coordinates: bool,
    ) -> Option<Self> {
        if size.0 == 0
            || size.1 == 0
            || ![scale.0, scale.1, device_scale]
                .iter()
                .all(|value| value.is_finite() && *value > 0.)
        {
            return None;
        }
        // Wayland uses integer buffer scaling and logical hotspots; Win32
        // uses physical pixels for both, including fractional monitor DPI.
        let (scale, buffer_scale) = if surface_coordinates {
            (scale, device_scale.ceil().clamp(1., 8.) as i32)
        } else {
            ((scale.0 * device_scale, scale.1 * device_scale), 1)
        };
        let width = (f32::from(size.0) * scale.0).round().max(1.) * buffer_scale as f32;
        let height = (f32::from(size.1) * scale.1).round().max(1.) * buffer_scale as f32;
        if !(1. ..=1024.).contains(&width) || !(1. ..=1024.).contains(&height) {
            return None;
        }
        Some(Self {
            width: width as u32,
            height: height as u32,
            hotspot: (
                ((f32::from(hotspot.0) * scale.0).round() as i32)
                    .min(width as i32 / buffer_scale - 1),
                ((f32::from(hotspot.1) * scale.1).round() as i32)
                    .min(height as i32 / buffer_scale - 1),
            ),
            scale: buffer_scale,
        })
    }
}

fn image(shape: &PointerShape, geometry: Geometry) -> Option<Image> {
    if shape.requires_background() {
        return None;
    }
    let (width, height) = shape.size();
    let mut pixels = vec![0; usize::from(width) * usize::from(height) * 4];
    shape.composite_overlay(&mut pixels).ok()?;
    Some(prepare_image(
        pixels,
        (u32::from(width), u32::from(height)),
        geometry,
    ))
}

fn prepare_image(mut pixels: Vec<u8>, dimensions: (u32, u32), geometry: Geometry) -> Image {
    // Resize in premultiplied space so transparent edges cannot bleed color.
    // Wayland consumes this directly; Win32 converts back when uploading.
    for pixel in pixels.as_chunks_mut::<4>().0 {
        for channel in 0..3 {
            pixel[channel] = ((u16::from(pixel[channel]) * u16::from(pixel[3]) + 127) / 255) as u8;
        }
        #[cfg(target_endian = "big")]
        pixel.reverse();
    }
    let buffer = image::RgbaImage::from_raw(dimensions.0, dimensions.1, pixels)
        .expect("validated cursor image");
    let pixels = if buffer.dimensions() == (geometry.width, geometry.height) {
        buffer.into_raw()
    } else {
        image::imageops::resize(
            &buffer,
            geometry.width,
            geometry.height,
            image::imageops::FilterType::Triangle,
        )
        .into_raw()
    };
    Image {
        width: geometry.width,
        height: geometry.height,
        hotspot: geometry.hotspot,
        scale: geometry.scale,
        pixels,
    }
}

#[derive(Default)]
pub(super) struct Cursor {
    backend: Option<Backend>,
    attempted: bool,
    prepared: Option<(Arc<PointerShape>, Geometry)>,
    active: bool,
}

impl Cursor {
    pub fn active(&self) -> bool {
        self.active
    }

    pub fn reset(&mut self) {
        self.active = false;
        self.prepared = None;
        if let Some(backend) = &mut self.backend {
            backend.suspend();
        }
    }

    pub fn apply(
        &mut self,
        shape: Option<&Arc<PointerShape>>,
        bounds: Option<Bounds<Pixels>>,
        scale: (f32, f32),
        window: &Window,
    ) -> bool {
        if !self.attempted {
            if bounds.is_none() || shape.is_none() {
                return false;
            }
            self.attempted = true;
            // GPUI's X11 raw-handle accessors panic. Other backends keep the
            // software overlay; never guess a native window by its title.
            #[cfg(target_os = "linux")]
            if gpui::guess_compositor() == "Wayland" {
                self.backend = Backend::new(window).ok();
            }
            #[cfg(windows)]
            {
                self.backend = Backend::new(window).ok();
            }
        }
        if let Some(backend) = &mut self.backend {
            let result = (|| {
                backend.poll()?;
                backend.set_bounds(bounds, window.scale_factor());
                let hovered = window.is_window_hovered()
                    && bounds.is_some_and(|bounds| bounds.contains(&window.mouse_position()));
                let desired = shape.and_then(|shape| {
                    (!shape.requires_background())
                        .then(|| {
                            Geometry::new(
                                shape.size(),
                                shape.hotspot(),
                                scale,
                                window.scale_factor(),
                            )
                        })
                        .flatten()
                        .map(|geometry| (shape, geometry))
                });
                if let Some((shape, geometry)) = desired {
                    if !self
                        .prepared
                        .as_ref()
                        .is_some_and(|(previous, previous_geometry)| {
                            Arc::ptr_eq(previous, shape) && *previous_geometry == geometry
                        })
                    {
                        if let Some(image) = image(shape, geometry) {
                            if !backend.prepare(&image)? {
                                return Ok(false);
                            }
                            self.prepared = Some((shape.clone(), geometry));
                        } else {
                            return Ok(false);
                        }
                    }
                    backend.show(hovered)
                } else {
                    // Inside a None hitbox, restore the software/hidden
                    // pointer. Outside it, GPUI has already chosen the local
                    // control's style; never overwrite that with None.
                    backend.hide(hovered)?;
                    Ok(false)
                }
            })();
            match result {
                Ok(active) => self.active = active,
                Err(_) => {
                    self.backend = None;
                    self.prepared = None;
                    self.active = false;
                }
            }
        }
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_cursor_buffers_and_hotspots_use_wayland_coordinate_spaces() {
        for fit in [0.25, 0.5, 1., 1.75, 2.] {
            for dpi in [1., 1.25, 1.5, 2., 3.] {
                let geometry = Geometry::scaled((33, 27), (7, 9), (fit, fit), dpi, true).unwrap();
                assert_eq!(geometry.scale, dpi.ceil() as i32);
                assert_eq!(geometry.width % geometry.scale as u32, 0);
                assert_eq!(geometry.height % geometry.scale as u32, 0);
                assert_eq!(
                    geometry.hotspot,
                    ((7. * fit).round() as i32, (9. * fit).round() as i32)
                );
                assert!((geometry.width as f32 / geometry.scale as f32 - 33. * fit).abs() <= 0.5);
            }
        }
        for scale in [0., -1., f32::NAN, f32::INFINITY, 1000.] {
            assert!(Geometry::new((32, 32), (0, 0), (scale, scale), 1.).is_none());
        }
        let tiny = Geometry::new((3, 3), (2, 2), (0.25, 0.25), 1.).unwrap();
        assert_eq!(tiny.hotspot, (0, 0));
    }

    #[test]
    fn windows_cursor_dimensions_and_hotspot_use_physical_pixels() {
        for fit in [0.25, 0.5, 1., 1.75, 2.] {
            for dpi in [1., 1.25, 1.5, 2., 3.] {
                let geometry = Geometry::scaled((33, 27), (7, 9), (fit, fit), dpi, false).unwrap();
                assert_eq!(geometry.scale, 1);
                assert_eq!(geometry.width, (33. * fit * dpi).round() as u32);
                assert_eq!(geometry.height, (27. * fit * dpi).round() as u32);
                assert_eq!(
                    geometry.hotspot,
                    (
                        (7. * fit * dpi).round() as i32,
                        (9. * fit * dpi).round() as i32
                    )
                );
            }
        }
        let tiny = Geometry::scaled((3, 3), (2, 2), (0.25, 0.25), 1.25, false).unwrap();
        assert_eq!((tiny.width, tiny.height, tiny.hotspot), (1, 1, (0, 0)));
        for dpi in [0., -1., f32::NAN, f32::INFINITY, 1000.] {
            assert!(Geometry::scaled((32, 32), (0, 0), (1., 1.), dpi, false).is_none());
        }
    }

    #[test]
    fn alpha_is_premultiplied_before_interpolation_without_color_fringes() {
        let geometry = Geometry {
            width: 2,
            height: 1,
            hotspot: (0, 0),
            scale: 1,
        };
        let converted = prepare_image(vec![200, 100, 50, 128, 255, 255, 255, 0], (2, 1), geometry);
        #[cfg(target_endian = "little")]
        assert_eq!(converted.pixels, [100, 50, 25, 128, 0, 0, 0, 0]);
        let scaled = prepare_image(
            vec![200, 100, 50, 128, 255, 255, 255, 0],
            (2, 1),
            Geometry {
                width: 8,
                ..geometry
            },
        );
        assert_eq!(scaled.pixels.len(), 32);
        #[cfg(target_endian = "little")]
        for pixel in scaled.pixels.as_chunks::<4>().0 {
            assert!(pixel[0] <= pixel[3]);
            assert!(pixel[1] <= pixel[0]);
            assert!(pixel[2] <= pixel[1]);
            if pixel[3] == 0 {
                assert_eq!(*pixel, [0; 4]);
            }
        }
    }
}
