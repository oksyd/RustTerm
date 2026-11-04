use super::{Action, ChannelEvent};
use crate::{Error, Limits, Monitor, Result, wire::Reader};

pub(super) struct DisplayControl {
    limits: Limits,
    max_area: Option<u128>,
    max_monitors: u32,
    pending_layout: Option<Vec<Monitor>>,
    desktop_scale: u32,
}

impl DisplayControl {
    pub(super) fn new(limits: Limits) -> Self {
        Self {
            limits,
            max_area: None,
            max_monitors: 0,
            pending_layout: None,
            desktop_scale: 100,
        }
    }
    pub(super) fn configure(&mut self, monitors: &[Monitor], desktop_scale: u32) {
        self.pending_layout = (!monitors.is_empty()).then(|| monitors.to_vec());
        self.desktop_scale = desktop_scale;
    }
    pub(super) fn closed(&mut self) {
        self.max_area = None;
        self.max_monitors = 0;
    }
    pub(super) fn resize(&mut self, width: u16, height: u16) -> Result<Vec<Action>> {
        self.layout(vec![Monitor {
            scale: self.desktop_scale,
            ..Monitor::primary(width, height)
        }])
    }
    pub(super) fn layout(&mut self, monitors: Vec<Monitor>) -> Result<Vec<Action>> {
        self.check_layout(&monitors)?;
        self.desktop_scale = monitors
            .iter()
            .find(|monitor| monitor.primary)
            .expect("validated monitor layout has a primary")
            .scale;
        self.pending_layout = Some(monitors);
        self.send_layout()
    }
    pub(super) fn receive(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        let mut reader = Reader::new(data);
        let kind = reader.u32_le()?;
        let length = reader.u32_le()? as usize;
        if length != data.len() {
            return Err(Error::Invalid("display control PDU length"));
        }
        if kind != 5 {
            return Err(Error::Unsupported("display control PDU"));
        }
        let monitors = reader.u32_le()?;
        let factor_a = reader.u32_le()?;
        let factor_b = reader.u32_le()?;
        reader.finish()?;
        if monitors == 0 || factor_a == 0 || factor_b == 0 {
            return Err(Error::Invalid("display control capabilities"));
        }
        self.max_monitors = monitors;
        self.max_area = Some(u128::from(monitors) * u128::from(factor_a) * u128::from(factor_b));
        let mut actions = vec![Action::Event(ChannelEvent::DisplayReady)];
        if let Some(monitors) = &self.pending_layout
            && let Err(error) = self.check_layout(monitors)
        {
            self.pending_layout = None;
            actions.push(Action::Event(ChannelEvent::RequestRejected(error)));
        }
        actions.extend(self.send_layout()?);
        Ok(actions)
    }

    fn check_layout(&self, monitors: &[Monitor]) -> Result<()> {
        crate::monitor::validate(monitors, self.limits)?;
        if self.max_monitors != 0 && monitors.len() as u32 > self.max_monitors {
            return Err(Error::Limit("server monitor count"));
        }
        let area: u128 = monitors
            .iter()
            .map(|m| u128::from(m.width) * u128::from(m.height))
            .sum();
        if self.max_area.is_some_and(|max| area > max) {
            return Err(Error::Limit("server display area"));
        }
        Ok(())
    }
    fn send_layout(&mut self) -> Result<Vec<Action>> {
        let (Some(_), Some(monitors)) = (self.max_area, &self.pending_layout) else {
            return Ok(Vec::new());
        };
        self.check_layout(monitors)?;
        let mut data = Vec::new();
        for value in [
            2,
            16 + monitors.len() as u32 * 40,
            40,
            monitors.len() as u32,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        for monitor in monitors {
            for value in [
                u32::from(monitor.primary),
                monitor.left as u32,
                monitor.top as u32,
                u32::from(monitor.width),
                u32::from(monitor.height),
                0,
                0,
                0,
                monitor.scale,
                100,
            ] {
                data.extend_from_slice(&value.to_le_bytes());
            }
        }
        self.pending_layout = None;
        Ok(vec![Action::Send(data)])
    }
}
