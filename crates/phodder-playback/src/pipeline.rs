use gstreamer as gst;
use gst::prelude::*;

/// Thin wrapper over playbin. Every operation here is asynchronous on the
/// GStreamer side; the engine sequences them using bus messages.
pub(crate) struct Pipeline {
    playbin: gst::Element,
    pub(crate) trim: crate::trim::Trim,
}

/// The sound server sink autoaudiosink chose, readable from any thread.
/// pulsesink reports the device it's playing to, which changes when the
/// sound server moves the stream (a Bluetooth output disappearing, say),
/// paused or not. pipewiresink doesn't, so this is None with it.
/// Its change signal only fires while playing; a paused stream that moves
/// is seen by reading `current_device` again.
#[derive(Clone, Default)]
pub(crate) struct OutputProbe(std::sync::Arc<OutputProbeInner>);

#[derive(Default)]
pub(crate) struct OutputProbeInner {
    sink: std::sync::Mutex<Option<gst::glib::WeakRef<gst::Element>>>,
    changed: tokio::sync::Notify,
}

impl OutputProbe {
    pub(crate) fn current_device(&self) -> Option<String> {
        let sink = self.0.sink.lock().unwrap().as_ref()?.upgrade()?;
        let device: Option<String> = sink.property("current-device");
        device.filter(|d| !d.is_empty())
    }

    pub(crate) async fn changed(&self) {
        self.0.changed.notified().await;
    }

    fn track(&self, sink: &gst::Element) {
        *self.0.sink.lock().unwrap() = Some(sink.downgrade());
        let probe = self.clone();
        sink.connect_notify(Some("current-device"), move |_, _| probe.0.changed.notify_one());
        self.0.changed.notify_one();
    }
}

impl Pipeline {
    /// `audio_sink` overrides playbin's automatic sink choice (tests use a
    /// synced fakesink).
    pub(crate) fn new(audio_sink: Option<gst::Element>, output: OutputProbe) -> anyhow::Result<Self> {
        gst::init()?;
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("playbin").build())?;

        // Video and subtitle decoding are disabled: some feeds ship
        // video episodes, and an unlinked video stream stalls preroll.
        let flags = playbin.property_value("flags");
        let flags_class = gst::glib::FlagsClass::with_type(flags.type_()).expect("playbin flags are a flags type");
        let flags = flags_class
            .builder_with_value(flags)
            .expect("value matches its own flags class")
            .unset_by_nick("video")
            .unset_by_nick("text")
            .build()
            .expect("known playbin flag nicks");
        playbin.set_property_from_value("flags", &flags);

        if let Some(sink) = audio_sink {
            playbin.set_property("audio-sink", &sink);
        }
        // playbin creates its sink when playback starts, inside
        // autoaudiosink.
        if let Some(bin) = playbin.downcast_ref::<gst::Bin>() {
            bin.connect_deep_element_added(move |_, _, element| {
                label_as_media(element);
                if element.factory().is_some_and(|f| f.name() == "pulsesink") {
                    output.track(element);
                }
            });
        }
        Ok(Self { playbin, trim: crate::trim::Trim::default() })
    }

    pub(crate) fn bus(&self) -> gst::Bus {
        self.playbin.bus().expect("playbin always has a bus")
    }

    pub(crate) fn is_playbin(&self, obj: Option<&gst::Object>) -> bool {
        obj.is_some_and(|o| o == self.playbin.upcast_ref::<gst::Object>())
    }

    /// Caller must `unload()` first: playbin ignores a new `uri` until it
    /// passes through READY. Setting the URI on a PLAYING/PAUSED playbin
    /// left the previous file playing.
    pub(crate) fn open(&self, path: &str) -> anyhow::Result<gst::StateChangeSuccess> {
        // filename_to_uri escapes characters such as '#' and '%' that
        // break a hand-built file:// URI.
        let uri = gst::glib::filename_to_uri(path, None)?;
        self.open_uri(&uri)
    }

    pub(crate) fn open_uri(&self, uri: &str) -> anyhow::Result<gst::StateChangeSuccess> {
        // scaletempo keeps pitch constant at other rates. Reusing one across
        // episodes with different sample rates or channels crashes at rates other than 1.0.
        let scaletempo = gst::ElementFactory::make("scaletempo").build();
        if let Err(e) = &scaletempo {
            tracing::warn!("scaletempo unavailable, pitch will shift with playback rate: {e}");
        }
        match self.trim.build_bin(scaletempo.ok()) {
            Ok(filter) => self.playbin.set_property("audio-filter", &filter),
            Err(e) => tracing::warn!("audio filter unavailable, silence won't be trimmed: {e}"),
        }
        self.playbin.set_property("uri", uri);
        Ok(self.playbin.set_state(gst::State::Paused)?)
    }

    pub(crate) fn play(&self) -> anyhow::Result<()> {
        self.playbin.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub(crate) fn pause(&self) -> anyhow::Result<()> {
        self.playbin.set_state(gst::State::Paused)?;
        Ok(())
    }

    pub(crate) fn unload(&self) {
        if let Err(e) = self.playbin.set_state(gst::State::Null) {
            tracing::warn!("failed to set pipeline to NULL: {e}");
        }
    }

    /// Flushing seek that also carries the playback rate; every seek must
    /// pass the rate or GStreamer resets it to 1.0. Posts ASYNC_DONE when
    /// complete.
    pub(crate) fn seek(&self, position_ms: u64, rate: f64) -> bool {
        let event = gst::event::Seek::new(
            rate,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            gst::ClockTime::from_mseconds(position_ms),
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
        self.playbin.send_event(event)
    }

    pub(crate) fn position_ms(&self) -> Option<u64> {
        self.playbin.query_position::<gst::ClockTime>().map(|t| self.trim.correct(t.mseconds()))
    }

    pub(crate) fn duration_ms(&self) -> Option<u64> {
        self.playbin.query_duration::<gst::ClockTime>().map(|t| t.mseconds())
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.unload();
    }
}

/// Marks the stream as media for the sound server, whose policy can then
/// lower it for notifications, navigation prompts or a screen reader.
/// PulseAudio spells the role in lowercase; PipeWire capitalizes it.
fn label_as_media(element: &gst::Element) {
    let role = match element.factory().map(|f| f.name()) {
        Some(name) if name == "pulsesink" => "music",
        Some(name) if name == "pipewiresink" => "Music",
        _ => return,
    };
    if element.find_property("stream-properties").is_some() {
        let properties = gst::Structure::builder("props").field("media.role", role).build();
        element.set_property("stream-properties", &properties);
        tracing::debug!("labeled the audio stream as media.role={role}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_and_pipewire_sinks_are_labeled_as_music() {
        gst::init().unwrap();
        for (factory, role) in [("pulsesink", "music"), ("pipewiresink", "Music")] {
            // Not every test machine has both plugins.
            let Ok(sink) = gst::ElementFactory::make(factory).build() else { continue };
            label_as_media(&sink);
            let properties = sink.property::<gst::Structure>("stream-properties");
            assert_eq!(properties.get::<String>("media.role").unwrap(), role);
        }
    }
}
