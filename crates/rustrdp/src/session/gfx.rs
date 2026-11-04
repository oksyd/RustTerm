use super::{Event, FrameCallback, GraphicsFrame, Session};
use crate::{Error, Result, gfx::FrameEvent};
#[cfg(test)]
mod tests;

impl Session {
    pub(super) fn graphics_pipeline(
        &mut self,
        data: &[u8],
        events: &mut Vec<Event>,
        on_frame: &mut Option<&mut FrameCallback<'_>>,
    ) -> Result<()> {
        let gfx = self
            .gfx
            .as_mut()
            .ok_or(Error::Invalid("graphics channel state"))?;
        let info = &mut self.info;
        let acknowledgements = &mut self.frame_acknowledgements;
        gfx.receive(data, &mut self.graphics, |event, graphics| {
            match event {
                FrameEvent::Begin(frame_id) => events.push(Event::GraphicsFrameBegin { frame_id }),
                FrameEvent::End { id, total } => {
                    let acknowledgement = Some(acknowledgements.complete_graphics(id, total)?);
                    if let Some(callback) = on_frame.as_deref_mut() {
                        let damage = graphics.take_damage();
                        callback(GraphicsFrame {
                            id,
                            bits_per_pixel: 32,
                            graphics,
                            damage,
                            acknowledgement: acknowledgement.clone(),
                        });
                    }
                    events.push(Event::GraphicsFrameEnd {
                        frame_id: id,
                        acknowledgement,
                    });
                }
                FrameEvent::Reset { width, height } => {
                    info.width = width;
                    info.height = height;
                    info.color_depth = 32;
                    events.push(Event::Resized { width, height });
                }
            }
            Ok(())
        })?;
        events.push(Event::GraphicsChanged);
        Ok(())
    }
}
