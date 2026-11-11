//! WAV ヘッダからの再生時間算出。

use std::time::Duration;

/// RIFF/WAVE のチャンクを探索し、data の長さ / fmt.byte_rate を返す。
/// 未知のチャンクと奇数サイズのパディングを許容する。不正・切り詰めは None。
pub fn wav_duration(bytes: &[u8]) -> Option<Duration> {
    fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
        ))
    }

    if bytes.get(..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
        return None;
    }
    let end = usize::try_from(u32_at(bytes, 4)?).ok()?.checked_add(8)?;
    if end < 12 || end > bytes.len() {
        return None;
    }
    let mut offset: usize = 12;
    let mut byte_rate = None;
    let mut data_size = None;
    while offset < end {
        let header_end = offset.checked_add(8)?;
        if header_end > end {
            return None;
        }
        let size = usize::try_from(u32_at(bytes, offset + 4)?).ok()?;
        let chunk_end = header_end.checked_add(size)?;
        if chunk_end > end {
            return None;
        }
        match &bytes[offset..offset + 4] {
            b"fmt " => {
                if size < 16 {
                    return None;
                }
                let rate = u32_at(bytes, header_end + 8)?;
                if rate == 0 {
                    return None;
                }
                byte_rate = Some(rate);
            }
            b"data" => {
                data_size = Some(data_size.unwrap_or(0_u64).checked_add(size as u64)?);
            }
            _ => {}
        }
        offset = chunk_end.checked_add(size & 1)?;
        if offset > end {
            return None;
        }
    }
    let rate = u64::from(byte_rate?);
    let size = data_size?;
    Some(Duration::new(
        size / rate,
        ((size % rate) * 1_000_000_000 / rate) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(wav: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
        wav.extend_from_slice(id);
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(data);
        if data.len() % 2 != 0 {
            wav.push(0);
        }
    }

    fn wav(extra_chunks: bool) -> Vec<u8> {
        let mut wav = b"RIFF\0\0\0\0WAVE".to_vec();
        if extra_chunks {
            chunk(&mut wav, b"JUNK", &[1, 2, 3]);
        }
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        fmt.extend_from_slice(&2_u16.to_le_bytes()); // stereo
        fmt.extend_from_slice(&24_000_u32.to_le_bytes());
        fmt.extend_from_slice(&96_000_u32.to_le_bytes()); // byte rate
        fmt.extend_from_slice(&4_u16.to_le_bytes()); // block align
        fmt.extend_from_slice(&16_u16.to_le_bytes()); // bits/sample
        chunk(&mut wav, b"fmt ", &fmt);
        if extra_chunks {
            chunk(&mut wav, b"LIST", &[0; 6]);
        }
        chunk(&mut wav, b"data", &vec![0; 120_000]);
        if extra_chunks {
            chunk(&mut wav, b"JUNK", &[7]);
        }
        let size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&size.to_le_bytes());
        wav
    }

    #[test]
    fn wav_duration_from_pcm_header() {
        assert_eq!(wav_duration(&wav(false)), Some(Duration::from_millis(1250)));
    }

    #[test]
    fn wav_duration_skips_extra_chunks() {
        assert_eq!(wav_duration(&wav(true)), Some(Duration::from_millis(1250)));
    }

    #[test]
    fn wav_duration_none_for_garbage() {
        assert_eq!(wav_duration(b"not a wav"), None);
        let valid = wav(false);
        for end in [0, 4, 8, 12, 20, 35, 43, valid.len() - 1] {
            assert_eq!(wav_duration(&valid[..end]), None, "truncated at {end}");
        }
        for (offset, value) in [
            (0, *b"RIFX"),
            (8, *b"AVI "),
            (12, *b"JUNK"),
            (24, 0_u32.to_le_bytes()),
            (28, 0_u32.to_le_bytes()),
            (16, 15_u32.to_le_bytes()),
            (40, u32::MAX.to_le_bytes()),
            (36, *b"JUNK"),
        ] {
            let mut invalid = valid.clone();
            invalid[offset..offset + 4].copy_from_slice(&value);
            // sample rate is not used for duration; byte rate is authoritative.
            if offset == 24 {
                assert_eq!(wav_duration(&invalid), Some(Duration::from_millis(1250)));
            } else {
                assert_eq!(wav_duration(&invalid), None, "invalid field at {offset}");
            }
        }
        let mut missing_pad = wav(true);
        missing_pad.pop();
        let size = (missing_pad.len() - 8) as u32;
        missing_pad[4..8].copy_from_slice(&size.to_le_bytes());
        assert_eq!(wav_duration(&missing_pad), None);
    }
}
