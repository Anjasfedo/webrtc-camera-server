//! Camera capability probing via GstDeviceMonitor.
//!
//! At startup we ask GStreamer what the real camera can actually deliver —
//! every (format, width, height, framerate) combination it advertises — so the
//! client can build resolution / framerate dropdowns from truth instead of
//! hardcoded guesses. The camera ties these together (e.g. a webcam may do
//! 1080p only at 30fps in NV12 but 5fps in YUY2), so we expose concrete combos.

use anyhow::Result;
use gstreamer as gst;
use gstreamer::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// One concrete capture mode the camera advertises.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CameraCap {
    /// Raw pixel format, e.g. "NV12"/"YUY2". `None` for encoded media (MJPEG).
    pub format: Option<String>,
    /// GStreamer media type, e.g. "video/x-raw" or "image/jpeg".
    pub media: String,
    pub width: i32,
    pub height: i32,
    /// Framerate rounded to the nearest whole fps (num/den collapsed).
    pub framerate: u32,
}

/// Probe the first matching camera and return its advertised capture modes.
///
/// `prefer_api` filters to a specific `device.api` property (e.g.
/// "mediafoundation" on Windows so the list matches the `mfvideosrc` pipeline).
/// Falls back to any `Video/Source` device if no match. Returns an empty vec
/// (not an error) if probing finds nothing — the client then keeps its static
/// defaults rather than the server failing to boot.
pub fn probe_camera(prefer_api: Option<&str>) -> Result<Vec<CameraCap>> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    monitor
        .start()
        .map_err(|e| anyhow::anyhow!("DeviceMonitor failed to start: {e}"))?;

    let devices = monitor.devices();

    // Pick the device whose `device.api` matches `prefer_api`, else the first.
    let mut chosen: Option<gst::Device> = None;
    let mut fallback: Option<gst::Device> = None;
    for device in devices {
        if fallback.is_none() {
            fallback = Some(device.clone());
        }
        if let (Some(want), Some(props)) = (prefer_api, device.properties()) {
            if props.get::<String>("device.api").ok().as_deref() == Some(want) {
                chosen = Some(device);
                break;
            }
        }
    }
    let device = chosen.or(fallback);

    let caps_list = match device {
        Some(d) => {
            info!("Probing camera caps: {}", d.display_name());
            d.caps()
        }
        None => {
            warn!("No Video/Source camera found; capability list will be empty");
            monitor.stop();
            return Ok(Vec::new());
        }
    };

    let mut out: Vec<CameraCap> = Vec::new();
    if let Some(caps) = caps_list {
        for s in caps.iter() {
            collect_caps(s, &mut out);
        }
    }

    monitor.stop();

    // Dedupe (different colorimetry/chroma variants collapse to the same combo)
    // and sort high-res-first so the dropdown reads naturally.
    out.dedup();
    out.sort_by(|a, b| {
        (b.width, b.height, b.framerate, &b.format).cmp(&(a.width, a.height, a.framerate, &a.format))
    });
    out.dedup();

    info!("Camera advertises {} capture modes", out.len());
    Ok(out)
}

/// Expand one caps structure into `CameraCap`s. width/height/framerate may be
/// fixed, a range, or a list; we only emit fixed combos (ranges are flattened
/// to their max, which is the useful upper bound for a dropdown).
fn collect_caps(s: &gst::StructureRef, out: &mut Vec<CameraCap>) {
    let media = s.name().to_string();
    let format = s.get::<String>("format").ok();

    let widths = read_ints(s, "width");
    let heights = read_ints(s, "height");
    let rates = read_framerates(s);

    for &w in &widths {
        for &h in &heights {
            for &fps in &rates {
                if w > 0 && h > 0 && fps > 0 {
                    out.push(CameraCap {
                        format: format.clone(),
                        media: media.clone(),
                        width: w,
                        height: h,
                        framerate: fps,
                    });
                }
            }
        }
    }
}

/// Read an integer field that may be fixed, a range (use min+max), or a list.
fn read_ints(s: &gst::StructureRef, field: &str) -> Vec<i32> {
    let Ok(v) = s.value(field) else {
        return Vec::new();
    };
    let t = v.type_();
    if t == i32::static_type() {
        v.get::<i32>().ok().into_iter().collect()
    } else if t == gst::IntRange::<i32>::static_type() {
        match v.get::<gst::IntRange<i32>>() {
            Ok(r) => vec![r.min(), r.max()],
            Err(_) => Vec::new(),
        }
    } else if t == gst::List::static_type() {
        match v.get::<gst::List>() {
            Ok(list) => list.iter().filter_map(|i| i.get::<i32>().ok()).collect(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    }
}

/// Read `framerate`, collapsing fixed / range / list into whole-fps values.
fn read_framerates(s: &gst::StructureRef) -> Vec<u32> {
    let Ok(v) = s.value("framerate") else {
        return Vec::new();
    };
    let t = v.type_();
    if t == gst::Fraction::static_type() {
        v.get::<gst::Fraction>().ok().map(frac_fps).into_iter().collect()
    } else if t == gst::FractionRange::static_type() {
        match v.get::<gst::FractionRange>() {
            Ok(r) => vec![frac_fps(r.max())],
            Err(_) => Vec::new(),
        }
    } else if t == gst::List::static_type() {
        match v.get::<gst::List>() {
            Ok(list) => list
                .iter()
                .filter_map(|i| i.get::<gst::Fraction>().ok())
                .map(frac_fps)
                .collect(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    }
}

/// Collapse a `Fraction` framerate to whole fps (rounded).
fn frac_fps(f: gst::Fraction) -> u32 {
    let (num, den) = (f.numer(), f.denom());
    if den == 0 {
        return 0;
    }
    ((num as f64 / den as f64).round()).max(0.0) as u32
}
