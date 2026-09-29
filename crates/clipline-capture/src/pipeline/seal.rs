use std::io;

use crate::traits::{AudioPacket, EncodedPacket};


pub(crate) fn sealed_video_durations(
    packets: &[EncodedPacket],
    boundary_pts_s: f64,
    timescale: u32,
) -> io::Result<Vec<f64>> {
    if timescale == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "video timescale must be nonzero",
        ));
    }
    let Some(first) = packets.first() else {
        return Ok(Vec::new());
    };
    if !boundary_pts_s.is_finite() {
        let minimum_duration_s = 1.0 / f64::from(timescale);
        return Ok((0..packets.len())
            .map(|index| {
                let next_pts_s = packets
                    .get(index + 1)
                    .map(|next| next.pts_s)
                    .unwrap_or(boundary_pts_s);
                if next_pts_s.is_finite() {
                    (next_pts_s - packets[index].pts_s).max(minimum_duration_s)
                } else {
                    packets[index].duration_s
                }
            })
            .collect());
    }

    let scale = f64::from(timescale);
    let total_ticks_f = (boundary_pts_s - first.pts_s) * scale;
    if !total_ticks_f.is_finite() || total_ticks_f > u64::MAX as f64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid sealed video GOP boundary",
        ));
    }
    let sample_count = u64::try_from(packets.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "video GOP sample count exceeds timeline capacity",
        )
    })?;
    // Encoded inter frames cannot be dropped safely: later frames may depend
    // on them. If pathological finite stamps provide fewer ticks than
    // samples, retain every packet and extend only enough to assign the
    // positive durations required by the MP4 writer.
    let total_ticks = (total_ticks_f.max(0.0).round() as u64).max(sample_count);

    let mut previous_end = 0_u64;
    let mut durations = Vec::with_capacity(packets.len());
    for (index, packet) in packets.iter().enumerate() {
        let next_pts_s = packets
            .get(index + 1)
            .map(|next| next.pts_s)
            .unwrap_or(boundary_pts_s);
        let interval_ticks = (next_pts_s - packet.pts_s) * scale;
        if !interval_ticks.is_finite() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid video sample interval",
            ));
        }

        let desired_end_f = (next_pts_s - first.pts_s) * scale;
        if !desired_end_f.is_finite() || desired_end_f > u64::MAX as f64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid video sample timestamp",
            ));
        }
        let desired_end = desired_end_f.max(0.0).round() as u64;
        let remaining = sample_count - index as u64 - 1;
        let earliest_end = previous_end + 1;
        let latest_end = total_ticks - remaining;
        let end = desired_end.clamp(earliest_end, latest_end);
        durations.push((end - previous_end) as f64 / scale);
        previous_end = end;
    }
    debug_assert_eq!(previous_end, total_ticks);
    Ok(durations)
}

pub(crate) fn drop_audio_before_timeline(pending_audio: &mut [Vec<AudioPacket>], timeline_start_s: f64) {
    for pending in pending_audio {
        pending.retain(|packet| packet.pts_s >= timeline_start_s - 1e-9);
    }
}
