//! MS-RDPERP 2.2.1: bounded Alternate Secondary Windowing Orders.
use super::{Rect, RemoteAppEvent, RemoteWindow, unicode};
use crate::{Error, Result, wire::Reader};
use std::collections::BTreeMap;

const MAX_WINDOWS: usize = 256;
const MAX_RECTS: usize = 4096;

#[derive(Default)]
pub(crate) struct Windows {
    windows: BTreeMap<u32, RemoteWindow>,
}

impl Windows {
    pub fn snapshot(&self) -> Vec<RemoteWindow> {
        self.windows.values().cloned().collect()
    }

    pub fn contains(&self, id: u32) -> bool {
        self.windows.contains_key(&id)
    }

    pub fn receive(&mut self, data: &[u8], slow_path: bool) -> Result<Vec<RemoteAppEvent>> {
        let mut r = Reader::new(data);
        if slow_path {
            r.skip(2)?;
        } // pad2OctetsA; updateType already consumed
        let count = usize::from(r.u16_le()?);
        if slow_path {
            r.skip(2)?;
        }
        if count > 4096 {
            return Err(Error::Limit("RemoteApp orders per update"));
        }
        let mut events = Vec::new();
        let mut event_bytes = 0usize;
        for _ in 0..count {
            // TS_SECONDARY (bit 1), altsec order type TS_WINDOW (11).
            if r.u8()? != (11 << 2 | 2) {
                return Err(Error::Unsupported("drawing order"));
            }
            let size = usize::from(r.u16_le()?);
            if size < 7 {
                return Err(Error::Invalid("window order size"));
            }
            let before = events.len();
            let mut order = Reader::new(r.take(size - 3)?);
            let flags = order.u32_le()?;
            match flags & 0x0700_0000 {
                0x0100_0000 => self.window(flags, &mut order, &mut events)?,
                0x0200_0000 => notification(flags, &mut order)?,
                0x0400_0000 => {
                    if flags & 1 != 0 {
                        events.push(RemoteAppEvent::Suspended);
                    } else {
                        let active_window = if flags & 0x20 != 0 {
                            Some(order.u32_le()?)
                        } else {
                            None
                        };
                        let z_order = if flags & 0x10 != 0 {
                            let count = usize::from(order.u8()?);
                            let mut ids = Vec::with_capacity(count);
                            for _ in 0..count {
                                ids.push(order.u32_le()?);
                            }
                            Some(ids)
                        } else {
                            None
                        };
                        events.push(RemoteAppEvent::Desktop {
                            active_window,
                            z_order,
                        });
                    }
                }
                _ => return Err(Error::Invalid("window order type")),
            }
            order.finish()?;
            if events.len() > before {
                event_bytes += match events.last() {
                    Some(RemoteAppEvent::Window(w)) => {
                        256 + w.title.len()
                            + 16 * (w.window_rects.len()
                                + w.visibility.as_ref().map_or(0, Vec::len))
                    }
                    _ => 2048,
                };
                if event_bytes > 4 * 1024 * 1024 {
                    return Err(Error::Limit("RemoteApp event batch"));
                }
            }
        }
        r.finish()?;
        Ok(events)
    }

    fn window(
        &mut self,
        flags: u32,
        r: &mut Reader<'_>,
        events: &mut Vec<RemoteAppEvent>,
    ) -> Result<()> {
        let id = r.u32_le()?;
        if flags & 0x4000_0000 != 0 {
            return icon(r);
        }
        if flags & 0x8000_0000 != 0 {
            return r.skip(3);
        }
        if flags & 0x2000_0000 != 0 {
            self.windows.remove(&id);
            events.push(RemoteAppEvent::Deleted(id));
            return Ok(());
        }
        let known = flags & 0x1000_0000 != 0 || self.windows.contains_key(&id);
        let mut window = if flags & 0x1000_0000 != 0 {
            if self.windows.len() >= MAX_WINDOWS && !self.windows.contains_key(&id) {
                return Err(Error::Limit("RemoteApp windows"));
            }
            RemoteWindow {
                id,
                ..Default::default()
            }
        } else {
            self.windows
                .get(&id)
                .cloned()
                .unwrap_or_else(|| RemoteWindow {
                    id,
                    ..Default::default()
                })
        };
        if flags & 0x02 != 0 {
            window.owner = r.u32_le()?;
        }
        if flags & 0x08 != 0 {
            window.style = r.u32_le()?;
            window.extended_style = r.u32_le()?;
        }
        if flags & 0x10 != 0 {
            window.show_state = r.u8()?;
        }
        if flags & 0x04 != 0 {
            let title = unicode(r)?;
            if title.encode_utf16().count() > 260 {
                return Err(Error::Limit("RemoteApp window title"));
            }
            window.title = title.chars().filter(|c| !c.is_control()).collect();
        }
        if flags & 0x4000 != 0 {
            window.client_offset = point(r)?;
        }
        if flags & 0x1_0000 != 0 {
            window.client_size = size(r)?;
        }
        if flags & 0x80 != 0 {
            window.resize_margins[0] = margin(r)?;
            window.resize_margins[2] = margin(r)?;
        }
        if flags & 0x0800_0000 != 0 {
            window.resize_margins[1] = margin(r)?;
            window.resize_margins[3] = margin(r)?;
        }
        if flags & 0x2_0000 != 0 {
            r.skip(1)?;
        }
        if flags & 0x4_0000 != 0 {
            r.skip(4)?;
        }
        if flags & 0x800 != 0 {
            window.offset = point(r)?;
        }
        if flags & 0x8000 != 0 {
            window.client_delta = point(r)?;
        }
        if flags & 0x400 != 0 {
            window.size = size(r)?;
        }
        if flags & 0x100 != 0 {
            window.window_rects = rects(r)?;
        }
        if flags & 0x1000 != 0 {
            window.visible_offset = point(r)?;
        }
        if flags & 0x200 != 0 {
            window.visibility = Some(rects(r)?);
        }
        if flags & 0x40_0000 != 0 {
            let _description = unicode(r)?;
        }
        for flag in [0x80_0000, 0x8_0000, 0x40, 0x01] {
            if flags & flag != 0 {
                r.skip(1)?;
            }
        }
        // Validate all bytes before committing partial updates to the window map.
        r.finish()?;
        if !known {
            return Ok(());
        }
        let rectangles =
            |w: &RemoteWindow| w.window_rects.len() + w.visibility.as_ref().map_or(0, Vec::len);
        if self
            .windows
            .values()
            .filter(|w| w.id != id)
            .map(rectangles)
            .sum::<usize>()
            + rectangles(&window)
            > 65536
        {
            return Err(Error::Limit("RemoteApp total rectangles"));
        }
        self.windows.insert(id, window.clone());
        events.push(RemoteAppEvent::Window(window));
        Ok(())
    }
}

fn point(r: &mut Reader<'_>) -> Result<(i32, i32)> {
    let point = (r.u32_le()? as i32, r.u32_le()? as i32);
    if !(-32768..=32767).contains(&point.0) || !(-32768..=32767).contains(&point.1) {
        return Err(Error::Limit("RemoteApp coordinates"));
    }
    Ok(point)
}
fn size(r: &mut Reader<'_>) -> Result<(u32, u32)> {
    let size = (r.u32_le()?, r.u32_le()?);
    if size.0 > 32767 || size.1 > 32767 {
        return Err(Error::Limit("RemoteApp window dimensions"));
    }
    Ok(size)
}
fn rects(r: &mut Reader<'_>) -> Result<Vec<Rect>> {
    let count = usize::from(r.u16_le()?);
    if count > MAX_RECTS {
        return Err(Error::Limit("RemoteApp window rectangles"));
    }
    let bytes = r.take(count * 8)?;
    let mut r = Reader::new(bytes);
    let mut rects = Vec::with_capacity(count);
    for _ in 0..count {
        let rect = Rect {
            left: i32::from(r.u16_le()?),
            top: i32::from(r.u16_le()?),
            right: i32::from(r.u16_le()?),
            bottom: i32::from(r.u16_le()?),
        };
        if rect.left > rect.right || rect.top > rect.bottom {
            return Err(Error::Invalid("RemoteApp rectangle"));
        }
        rects.push(rect);
    }
    Ok(rects)
}

// Icon suppression is advisory on older servers. Validate/consume these orders
// without decoding or allocating icon bitmaps. No icon caches are advertised.
fn icon(r: &mut Reader<'_>) -> Result<()> {
    r.skip(3)?;
    let bpp = r.u8()?;
    if !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err(Error::Invalid("RemoteApp icon depth"));
    }
    r.skip(4)?;
    let palette = if bpp <= 8 {
        usize::from(r.u16_le()?)
    } else {
        0
    };
    let mask = usize::from(r.u16_le()?);
    let color = usize::from(r.u16_le()?);
    r.skip(mask + palette + color)
}

fn notification(flags: u32, r: &mut Reader<'_>) -> Result<()> {
    r.skip(8)?;
    if flags & 0x2000_0000 != 0 {
        return Ok(());
    }
    if flags & 8 != 0 {
        r.skip(4)?;
    }
    if flags & 1 != 0 {
        unicode(r)?;
    }
    if flags & 2 != 0 {
        r.skip(8)?;
        unicode(r)?;
        unicode(r)?;
    }
    if flags & 4 != 0 {
        r.skip(4)?;
    }
    if flags & 0x4000_0000 != 0 {
        icon(r)?;
    }
    if flags & 0x8000_0000 != 0 {
        r.skip(3)?;
    }
    Ok(())
}

fn margin(r: &mut Reader<'_>) -> Result<u32> {
    let margin = r.u32_le()?;
    if margin > 32767 {
        return Err(Error::Limit("RemoteApp resize margins"));
    }
    Ok(margin)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn created() -> Vec<u8> {
        // One TS_WINDOW order: new, owner, style, show, title, position,
        // size, visible offset, visibility. The frame is independent of the parser.
        let mut body = vec![42, 0, 0, 0];
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0x10cf_0000u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&[5, 2, 0, b'A', 0]);
        for n in [10u32, 20, 300, 200, 10, 20] {
            body.extend_from_slice(&n.to_le_bytes());
        }
        body.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0x2c, 1, 200, 0]);
        order(0x1100_1e1e, &body)
    }

    fn order(flags: u32, body: &[u8]) -> Vec<u8> {
        let mut packet = vec![1, 0, 0x2e];
        packet.extend_from_slice(&((body.len() + 7) as u16).to_le_bytes());
        packet.extend_from_slice(&flags.to_le_bytes());
        packet.extend_from_slice(body);
        packet
    }

    #[test]
    fn create_partial_update_and_delete_keep_correct_window_identity() {
        let mut windows = Windows::default();
        let events = windows.receive(&created(), false).unwrap();
        let RemoteAppEvent::Window(initial) = &events[0] else {
            panic!("window event");
        };
        assert_eq!(initial.id, 42);
        assert_eq!(initial.title, "A");
        assert_eq!(initial.offset, (10, 20));
        assert_eq!(initial.size, (300, 200));
        assert_eq!(
            initial.visibility.as_ref().unwrap(),
            &[Rect {
                left: 0,
                top: 0,
                right: 300,
                bottom: 200
            }]
        );
        let renamed = order(0x0100_0004, &[42, 0, 0, 0, 2, 0, b'B', 0]);
        let changed = windows.receive(&renamed, false).unwrap();
        let RemoteAppEvent::Window(changed) = &changed[0] else {
            panic!("window event");
        };
        assert_eq!(changed.title, "B");
        assert_eq!(changed.offset, initial.offset);
        assert_eq!(changed.visibility, initial.visibility);
        // MS-RDPERP 4.1.1.2 deletion layout.
        let deleted = order(0x2100_0000, &[42, 0, 0, 0]);
        assert_eq!(
            windows.receive(&deleted, false).unwrap(),
            [RemoteAppEvent::Deleted(42)]
        );
        assert!(!windows.contains(42));
        assert!(windows.receive(&renamed, false).unwrap().is_empty());
    }

    #[test]
    fn slow_path_padding_and_every_truncated_prefix_are_checked() {
        let fast = created();
        let mut slow = vec![0, 0, 1, 0, 0, 0];
        slow.extend_from_slice(&fast[2..]);
        let expected = Windows::default().receive(&fast, false).unwrap();
        assert_eq!(Windows::default().receive(&slow, true).unwrap(), expected);
        for length in 0..fast.len() {
            assert!(
                Windows::default().receive(&fast[..length], false).is_err(),
                "prefix {length}"
            );
        }
        for length in 0..slow.len() {
            assert!(
                Windows::default().receive(&slow[..length], true).is_err(),
                "prefix {length}"
            );
        }
        let mut trailing = fast.clone();
        trailing.push(0);
        assert!(Windows::default().receive(&trailing, false).is_err());
    }

    #[test]
    fn empty_visibility_is_retained_and_failed_updates_do_not_mutate_state() {
        let mut windows = Windows::default();
        windows.receive(&created(), false).unwrap();
        windows
            .receive(&order(0x0100_0200, &[42, 0, 0, 0, 0, 0]), false)
            .unwrap();
        assert_eq!(windows.windows[&42].visibility, Some(Vec::new()));
        let before = windows.snapshot();
        assert!(
            windows
                .receive(
                    &order(
                        0x0100_0400,
                        &[42, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0]
                    ),
                    false
                )
                .is_err()
        );
        assert_eq!(windows.snapshot(), before);
    }

    #[test]
    fn desktop_notifications_and_unknown_order_classes_are_distinct() {
        let mut windows = Windows::default();
        assert_eq!(
            windows
                .receive(
                    &order(0x0400_0030, &[42, 0, 0, 0, 2, 42, 0, 0, 0, 43, 0, 0, 0]),
                    false
                )
                .unwrap(),
            [RemoteAppEvent::Desktop {
                active_window: Some(42),
                z_order: Some(vec![42, 43])
            }]
        );
        assert_eq!(
            windows.receive(&order(0x0400_0001, &[]), false).unwrap(),
            [RemoteAppEvent::Suspended]
        );
        assert!(windows.receive(&[1, 0, 1], false).is_err());
        assert!(windows.receive(&order(0x0300_0000, &[]), false).is_err());
    }
}
