//! Annex-B helpers for the H.264 access units carried in video packets.
//!
//! The wire payload is an Annex-B access unit (start codes, SPS/PPS repeated
//! on keyframes). Encoders that emit length-prefixed NALs are normalized
//! before packetization.

const START4: &[u8] = &[0, 0, 0, 1];

pub fn nal_type(nal_header: u8) -> u8 {
    nal_header & 0x1f
}

pub fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(START4)
}

/// Yields each NAL, not including its start code.
pub fn iter_annex_b_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let start = if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            Some(i + 4)
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            Some(i + 3)
        } else {
            None
        };
        if let Some(nal_start) = start {
            if let Some((_, prev_end)) = ranges.last_mut() {
                *prev_end = i;
            }
            ranges.push((nal_start, data.len()));
            i = nal_start;
        } else {
            i += 1;
        }
    }
    ranges.into_iter().filter_map(move |(start, end)| {
        if start < end && start < data.len() {
            Some(&data[start..end.min(data.len())])
        } else {
            None
        }
    })
}

pub fn contains_idr(annex_b: &[u8]) -> bool {
    iter_annex_b_nals(annex_b).any(|nal| nal.first().is_some_and(|b| nal_type(*b) == 5))
}

pub fn contains_parameter_sets(annex_b: &[u8]) -> bool {
    let mut sps = false;
    let mut pps = false;
    for nal in iter_annex_b_nals(annex_b) {
        match nal.first().map(|b| nal_type(*b)) {
            Some(7) => sps = true,
            Some(8) => pps = true,
            _ => {}
        }
    }
    sps && pps
}

/// Convert a buffer of big-endian length-prefixed NALs into Annex-B.
/// Returns `None` when the lengths do not cover the buffer exactly.
pub fn try_length_prefixed_to_annex_b(data: &[u8], length_size: usize) -> Option<Vec<u8>> {
    if data.is_empty() || length_size == 0 || length_size > 4 {
        return None;
    }
    let mut i = 0;
    let mut out = Vec::with_capacity(data.len() + 4 * 8);
    while i < data.len() {
        if i + length_size > data.len() {
            return None;
        }
        let mut len = 0usize;
        for byte in &data[i..i + length_size] {
            len = (len << 8) | *byte as usize;
        }
        i += length_size;
        if len == 0 || i + len > data.len() {
            return None;
        }
        out.extend_from_slice(START4);
        out.extend_from_slice(&data[i..i + len]);
        i += len;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Parse an AVCC `AVCDecoderConfigurationRecord` into Annex-B SPS/PPS.
pub fn avcc_record_to_annex_b(record: &[u8]) -> Option<Vec<u8>> {
    if record.len() < 7 || record[0] != 1 {
        return None;
    }
    let mut i = 5;
    let num_sps = (record[i] & 0x1f) as usize;
    i += 1;
    let mut out = Vec::new();
    for _ in 0..num_sps {
        if i + 2 > record.len() {
            return None;
        }
        let len = u16::from_be_bytes([record[i], record[i + 1]]) as usize;
        i += 2;
        if len == 0 || i + len > record.len() {
            return None;
        }
        out.extend_from_slice(START4);
        out.extend_from_slice(&record[i..i + len]);
        i += len;
    }
    if i >= record.len() {
        return None;
    }
    let num_pps = record[i] as usize;
    i += 1;
    for _ in 0..num_pps {
        if i + 2 > record.len() {
            return None;
        }
        let len = u16::from_be_bytes([record[i], record[i + 1]]) as usize;
        i += 2;
        if len == 0 || i + len > record.len() {
            return None;
        }
        out.extend_from_slice(START4);
        out.extend_from_slice(&record[i..i + len]);
        i += len;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn normalize_access_unit(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    if is_annex_b(data) {
        return data.to_vec();
    }
    if let Some(annex) = try_length_prefixed_to_annex_b(data, 4) {
        return annex;
    }
    if let Some(annex) = try_length_prefixed_to_annex_b(data, 3) {
        return annex;
    }
    if let Some(annex) = avcc_record_to_annex_b(data) {
        return annex;
    }
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(START4);
    out.extend_from_slice(data);
    out
}

#[derive(Debug, Default, Clone)]
pub struct ParameterSetCache {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParameterSetCache {
    pub fn observe(&mut self, annex_b: &[u8]) {
        for nal in iter_annex_b_nals(annex_b) {
            match nal.first().map(|b| nal_type(*b)) {
                Some(7) => self.sps = Some(nal.to_vec()),
                Some(8) => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
    }

    /// Prepend cached SPS/PPS when a keyframe access unit does not carry them.
    pub fn ensure(&self, annex_b: &[u8], is_keyframe: bool) -> Vec<u8> {
        if !is_keyframe || contains_parameter_sets(annex_b) {
            return annex_b.to_vec();
        }
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else {
            return annex_b.to_vec();
        };
        let mut out = Vec::with_capacity(8 + sps.len() + pps.len() + annex_b.len());
        out.extend_from_slice(START4);
        out.extend_from_slice(sps);
        out.extend_from_slice(START4);
        out.extend_from_slice(pps);
        out.extend_from_slice(annex_b);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_and_detects_idr() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]);
        let nals: Vec<_> = iter_annex_b_nals(&data).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0][0]), 7);
        assert!(contains_parameter_sets(&data));
        assert!(contains_idr(&data));
    }

    #[test]
    fn length_prefixed_and_avcc_become_annex_b() {
        let avcc_nals = {
            let mut v = Vec::new();
            v.extend_from_slice(&4u32.to_be_bytes());
            v.extend_from_slice(&[0x67, 1, 2, 3]);
            v.extend_from_slice(&2u32.to_be_bytes());
            v.extend_from_slice(&[0x68, 4]);
            v
        };
        let annex = try_length_prefixed_to_annex_b(&avcc_nals, 4).unwrap();
        assert!(is_annex_b(&annex));
        assert!(contains_parameter_sets(&annex));

        // configurationVersion, profile, compat, level, lengthSizeMinusOne = 3
        let mut record = vec![1, 0x42, 0, 0x1f, 0xff, 0xe1];
        record.extend_from_slice(&3u16.to_be_bytes());
        record.extend_from_slice(&[0x67, 0x42, 0x00]);
        record.push(1);
        record.extend_from_slice(&2u16.to_be_bytes());
        record.extend_from_slice(&[0x68, 0xce]);
        let from_record = avcc_record_to_annex_b(&record).unwrap();
        assert!(contains_parameter_sets(&from_record));
    }

    #[test]
    fn cache_prepends_missing_parameter_sets() {
        let mut cache = ParameterSetCache::default();
        let sets = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce];
        cache.observe(&sets);
        let idr = [0, 0, 0, 1, 0x65, 0x11];
        let fixed = cache.ensure(&idr, true);
        assert!(contains_parameter_sets(&fixed));
        assert!(contains_idr(&fixed));
        let p_frame = [0, 0, 0, 1, 0x41, 0x22];
        assert_eq!(cache.ensure(&p_frame, false), p_frame);
    }
}
