//! Camera enumeration + capability probing via GstDeviceMonitor.
//!
//! At startup we ask GStreamer which cameras are connected and what each can
//! actually deliver — every (format, width, height, framerate) combo it
//! advertises — so the client can build device / resolution / framerate
//! dropdowns from truth instead of hardcoded guesses. A camera ties these
//! together (e.g. 1080p only at 30fps in NV12 but 5fps in YUY2), so we expose
//! concrete combos per device.

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

/// One connected camera and its advertised capture modes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CameraDevice {
    /// Stable handle the pipeline uses to select this device. On mediafoundation
    /// this is `device.path` (fed to `mfvideosrc device-path=...`). Empty if the
    /// backend exposes no path (then we fall back to the default source).
    pub id: String,
    /// Human-readable name for the dropdown.
    pub name: String,
    /// The `device.api` this device belongs to, e.g. "mediafoundation".
    pub api: String,
    /// This device's real capture modes.
    pub caps: Vec<CameraCap>,
}

/// Probe all connected cameras and their advertised capture modes.
///
/// `only_api`, when set, keeps only devices whose `device.api` matches (e.g.
/// "mediafoundation" on Windows so the list matches the `mfvideosrc` pipeline).
/// Returns an empty vec (not an error) if probing finds nothing — the server
/// then runs without a device list rather than failing to boot.
pub fn probe_cameras(only_api: Option<&str>) -> Result<Vec<CameraDevice>> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    monitor
        .start()
        .map_err(|e| anyhow::anyhow!("DeviceMonitor failed to start: {e}"))?;

    let mut out: Vec<CameraDevice> = Vec::new();
    for device in monitor.devices() {
        let props = device.properties();
        let api = props
            .as_ref()
            .and_then(|p| p.get::<String>("device.api").ok())
            .unwrap_or_default();

        // Filter to the requested backend so we don't list the same physical
        // camera twice under different APIs.
        if let Some(want) = only_api {
            if api != want {
                continue;
            }
        }

        let id = props
            .as_ref()
            .and_then(|p| p.get::<String>("device.path").ok())
            .unwrap_or_default();
        let name = device.display_name().to_string();

        let mut caps: Vec<CameraCap> = Vec::new();
        if let Some(c) = device.caps() {
            for s in c.iter() {
                collect_caps(s, &mut caps);
            }
        }
        normalize_caps(&mut caps);

        info!("Camera: {name} ({api}) — {} modes", caps.len());
        out.push(CameraDevice { id, name, api, caps });
    }

    monitor.stop();

    if out.is_empty() {
        warn!("No matching Video/Source camera found; device list is empty");
    }
    Ok(out)
}

/// Dedupe (colorimetry/chroma variants collapse to the same combo) and sort
/// high-res-first so dropdowns read naturally.
fn normalize_caps(caps: &mut Vec<CameraCap>) {
    caps.dedup();
    caps.sort_by(|a, b| {
        (b.width, b.height, b.framerate, &b.format).cmp(&(a.width, a.height, a.framerate, &a.format))
    });
    caps.dedup();
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
