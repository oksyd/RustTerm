use super::*;

impl<R: tokio::io::AsyncRead + Unpin> Worker<'_, R> {
    pub(super) async fn captured(&mut self, value: Option<audio::Captured>) -> Result<Step> {
        let outputs = match value {
            Some(audio::Captured::Opened {
                generation,
                available,
            }) => {
                if !available {
                    self.media.capture = None;
                    notify_rejection(
                        RdpError::MicrophoneUnavailable,
                        None,
                        self.events,
                        &mut self.last_rejection,
                    )
                    .await?;
                }
                self.session.confirm_capture(generation, available)?
            }
            Some(audio::Captured::Samples {
                generation,
                data,
                transmission,
                discontinuity,
            }) => {
                if transmission.is_cancelled()
                    || self.media.locked.load(Ordering::Acquire)
                    || !self.media.microphone_enabled.load(Ordering::Acquire)
                {
                    return Ok(Step::Skip);
                }
                let mut outputs = if discontinuity {
                    self.session.reset_capture(generation)?
                } else {
                    Vec::new()
                };
                outputs.extend(self.session.capture_samples(generation, &data)?);
                return Ok(Step::Capture(bundle_capture(outputs), transmission));
            }
            Some(audio::Captured::Failed) | None => {
                self.media.capture = None;
                notify_rejection(
                    RdpError::MicrophoneUnavailable,
                    None,
                    self.events,
                    &mut self.last_rejection,
                )
                .await?;
                self.session.close_capture()?
            }
        };
        Ok(Step::Outputs(outputs))
    }
    pub(super) async fn drive(&mut self, value: Option<rustrdp::DriveCompletion>) -> Result<Step> {
        let outputs = match value {
            Some(completion) => self.session.complete_drive(completion)?,
            None => return Err(RdpError::FileUnavailable),
        };
        Ok(Step::Outputs(outputs))
    }
    pub(super) async fn audio_completed(
        &mut self,
        value: Option<audio::Completion>,
    ) -> Result<Step> {
        let outputs = match value {
            Some(audio::Completion::Consumed { token, elapsed_ms }) => {
                self.session.confirm_audio(token, elapsed_ms)?
            }
            Some(audio::Completion::Unavailable) => {
                notify_rejection(
                    RdpError::AudioUnavailable,
                    None,
                    self.events,
                    &mut self.last_rejection,
                )
                .await?;
                return Ok(Step::Skip);
            }
            None => {
                self.media.playback = None;
                notify_rejection(
                    RdpError::AudioUnavailable,
                    None,
                    self.events,
                    &mut self.last_rejection,
                )
                .await?;
                return Ok(Step::Skip);
            }
        };
        Ok(Step::Outputs(outputs))
    }
}
