//! A compressed Chromaprint fingerprint (what `fpcalc` prints and an AcoustID lookup sends) read
//! back into its raw items, the form two prints are compared in.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The largest step between two set bits kept in the 3-bit stream; a step this size or larger
/// carries the rest in the 5-bit stream.
const MAX_NORMAL: u8 = 7;

/// The raw items of a compressed fingerprint (URL-safe base64, no padding, as `fpcalc` prints it).
/// None when the text is not one: bad base64, a header naming more items than its bits hold, or
/// bits past an item's 32.
pub fn decompress(print: &str) -> Option<Vec<u32>> {
    let bytes = URL_SAFE_NO_PAD.decode(print.trim()).ok()?;
    if bytes.len() < 4 {
        return None;
    }
    let count = (usize::from(bytes[1]) << 16) | (usize::from(bytes[2]) << 8) | usize::from(bytes[3]);
    let body = &bytes[4..];

    // The 3-bit stream holds each item's set bits as steps from the last one, and a 0 after each
    // item; it ends at the item count's last 0.
    let mut steps: Vec<u8> = Vec::new();
    let mut ends = 0;
    let mut exceptions = 0;
    if count > 0 {
        for at in 0..(body.len() * 8 / 3) {
            let step = bits_at(body, at * 3, 3);
            steps.push(step);
            if step == 0 {
                ends += 1;
                if ends == count {
                    break;
                }
            } else if step == MAX_NORMAL {
                exceptions += 1;
            }
        }
        if ends < count {
            return None;
        }
    }
    let rest = &body[(steps.len() * 3).div_ceil(8)..];
    if rest.len() * 8 < exceptions * 5 {
        return None;
    }
    for (exception, step) in steps.iter_mut().filter(|step| **step == MAX_NORMAL).enumerate() {
        *step += bits_at(rest, exception * 5, 5);
    }

    let mut items: Vec<u32> = Vec::with_capacity(count);
    let (mut value, mut bit) = (0u32, 0u32);
    for step in steps {
        if step == 0 {
            let item = items.last().map_or(value, |last| value ^ last);
            items.push(item);
            value = 0;
            bit = 0;
            continue;
        }
        bit += u32::from(step);
        if bit > 32 {
            return None;
        }
        value |= 1 << (bit - 1);
    }
    Some(items)
}

/// `width` bits starting at bit `from` of a little-endian bit stream.
fn bits_at(bytes: &[u8], from: usize, width: usize) -> u8 {
    let mut value = 0u8;
    for offset in 0..width {
        let at = from + offset;
        if (bytes[at / 8] >> (at % 8)) & 1 == 1 {
            value |= 1 << offset;
        }
    }
    value
}

#[cfg(test)]
#[path = "compressed_print_tests.rs"]
mod tests;
