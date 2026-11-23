//! Silence trimming: drops long runs of quiet audio before scaletempo and
//! moves later buffers earlier to close the gap. The pipeline then reports
//! positions on the shortened timeline, so `Trim::correct` maps them back to
//! the file's. The offset starts over on every flush or new segment, which
//! a seek always brings.

use gstreamer as gst;
use gst::prelude::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Peak level below which audio counts as silent: about -30 dBFS.
const SILENCE_PEAK: f32 = 0.031;
/// The start of each silent run that's kept, so speech keeps its pauses.
const KEEP_NS: u64 = 250_000_000;
/// How many offset changes are remembered for `correct`. The sink plays
/// well under a second behind the probe, which this covers many times over.
const HISTORY: usize = 512;

#[derive(Clone, Default)]
pub(crate) struct Trim(Arc<Inner>);

#[derive(Default)]
struct Inner {
    enabled: AtomicBool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    silent_run_ns: u64,
    offset_ns: u64,
    /// (shortened-timeline position, offset from there on), oldest first.
    changes: VecDeque<(u64, u64)>,
}

impl Trim {
    /// Turning it off keeps the time already removed, so positions stay
    /// consistent until the next seek.
    pub(crate) fn set_enabled(&self, on: bool) {
        self.0.enabled.store(on, Ordering::Relaxed);
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Relaxed)
    }

    /// Maps a position the pipeline reported to the file's timeline.
    pub(crate) fn correct(&self, reported_ms: u64) -> u64 {
        let state = self.0.state.lock().unwrap();
        let reported_ns = reported_ms * 1_000_000;
        let offset = state.changes.iter().rev().find(|(at, _)| *at <= reported_ns).map_or(0, |(_, offset)| *offset);
        (reported_ns + offset) / 1_000_000
    }

    fn reset(&self) {
        *self.0.state.lock().unwrap() = State::default();
    }

    /// Wraps `filter` (scaletempo, or None) in a bin that converts to 32-bit
    /// float, trims, then runs the filter.
    pub(crate) fn build_bin(&self, filter: Option<gst::Element>) -> anyhow::Result<gst::Element> {
        let bin = gst::Bin::new();
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let caps = gst::Caps::builder("audio/x-raw").field("format", "F32LE").field("layout", "interleaved").build();
        let capsfilter = gst::ElementFactory::make("capsfilter").property("caps", &caps).build()?;
        bin.add_many([&convert, &capsfilter])?;
        convert.link(&capsfilter)?;
        let last = match filter {
            Some(filter) => {
                bin.add(&filter)?;
                capsfilter.link(&filter)?;
                filter
            }
            None => capsfilter.clone(),
        };
        let sink_pad = convert.static_pad("sink").expect("audioconvert has a sink pad");
        let src_pad = last.static_pad("src").expect("filters have a src pad");
        bin.add_pad(&gst::GhostPad::with_target(&sink_pad)?)?;
        bin.add_pad(&gst::GhostPad::with_target(&src_pad)?)?;

        let trim = self.clone();
        let probe_pad = capsfilter.static_pad("src").expect("capsfilter has a src pad");
        probe_pad.add_probe(gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            match &mut info.data {
                Some(gst::PadProbeData::Event(event)) => {
                    if matches!(event.type_(), gst::EventType::FlushStop | gst::EventType::Segment) {
                        trim.reset();
                    }
                    gst::PadProbeReturn::Ok
                }
                Some(gst::PadProbeData::Buffer(buffer)) => trim.on_buffer(pad, buffer),
                _ => gst::PadProbeReturn::Ok,
            }
        });
        Ok(bin.upcast())
    }

    fn on_buffer(&self, pad: &gst::Pad, buffer: &mut gst::Buffer) -> gst::PadProbeReturn {
        let Some(pts) = buffer.pts() else { return gst::PadProbeReturn::Ok };
        let pts = pts.nseconds();
        let enabled = self.is_enabled();
        let duration = buffer.duration().map(|d| d.nseconds()).or_else(|| duration_from_caps(pad, buffer.size()));
        let mut state = self.0.state.lock().unwrap();
        if enabled {
            if let Some(duration) = duration.filter(|_| is_silent(buffer)) {
                state.silent_run_ns += duration;
                if state.silent_run_ns > KEEP_NS {
                    let at = pts.saturating_sub(state.offset_ns);
                    state.offset_ns += duration;
                    let offset = state.offset_ns;
                    // Consecutive drops land on the same spot; one entry does.
                    match state.changes.back_mut() {
                        Some(last) if last.0 == at => last.1 = offset,
                        _ => state.changes.push_back((at, offset)),
                    }
                    if state.changes.len() > HISTORY {
                        state.changes.pop_front();
                    }
                    return gst::PadProbeReturn::Drop;
                }
            } else {
                state.silent_run_ns = 0;
            }
        } else {
            state.silent_run_ns = 0;
        }
        if state.offset_ns > 0 {
            let shifted = pts.saturating_sub(state.offset_ns);
            buffer.make_mut().set_pts(gst::ClockTime::from_nseconds(shifted));
        }
        gst::PadProbeReturn::Ok
    }
}

fn is_silent(buffer: &gst::Buffer) -> bool {
    let Ok(map) = buffer.map_readable() else { return false };
    map.chunks_exact(4).all(|sample| f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]).abs() < SILENCE_PEAK)
}

fn duration_from_caps(pad: &gst::Pad, size: usize) -> Option<u64> {
    let caps = pad.current_caps()?;
    let structure = caps.structure(0)?;
    let rate = structure.get::<i32>("rate").ok()?;
    let channels = structure.get::<i32>("channels").ok()?;
    let frames = size as u64 / (4 * u64::try_from(channels).ok()?.max(1));
    Some(frames * 1_000_000_000 / u64::try_from(rate).ok()?.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrected_positions_follow_the_offset_in_force_at_that_point() {
        let trim = Trim::default();
        {
            let mut state = trim.0.state.lock().unwrap();
            state.changes.push_back((1_000_000_000, 500_000_000));
            state.changes.push_back((3_000_000_000, 2_000_000_000));
        }
        assert_eq!(trim.correct(900), 900);
        assert_eq!(trim.correct(1_000), 1_500);
        assert_eq!(trim.correct(2_999), 3_499);
        assert_eq!(trim.correct(4_000), 6_000);
        trim.reset();
        assert_eq!(trim.correct(4_000), 4_000);
    }
}
