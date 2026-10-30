use super::*;

fn print_of(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Chromaprint's own compressor, as its `FingerprintCompressor` writes a print: each item XORed
/// with the one before, its set bits as steps in a 3-bit stream (a 0 after each item, a step of 7
/// or more carrying the rest in a 5-bit stream), behind a header of the algorithm and the count.
fn compress(items: &[u32]) -> String {
    let (mut steps, mut exceptions): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
    for (at, item) in items.iter().enumerate() {
        let mut x = if at == 0 { *item } else { item ^ items[at - 1] };
        let (mut bit, mut last) = (1u8, 0u8);
        while x != 0 {
            if x & 1 == 1 {
                let step = bit - last;
                if step >= MAX_NORMAL {
                    steps.push(MAX_NORMAL);
                    exceptions.push(step - MAX_NORMAL);
                } else {
                    steps.push(step);
                }
                last = bit;
            }
            x >>= 1;
            bit += 1;
        }
        steps.push(0);
    }
    let pack = |values: &[u8], width: usize| -> Vec<u8> {
        let mut out = vec![0u8; (values.len() * width).div_ceil(8)];
        for (index, value) in values.iter().enumerate() {
            for offset in 0..width {
                if (value >> offset) & 1 == 1 {
                    let at = index * width + offset;
                    out[at / 8] |= 1 << (at % 8);
                }
            }
        }
        out
    };
    let count = items.len();
    let mut bytes = vec![1, (count >> 16) as u8, (count >> 8) as u8, count as u8];
    bytes.extend(pack(&steps, 3));
    bytes.extend(pack(&exceptions, 5));
    print_of(&bytes)
}

/// Chromaprint's own decompressor cases: one bit, three bits, a step carried in the 5-bit stream,
/// two items, and two items with nothing changed between them.
#[test]
fn chromaprints_own_cases_read_back() {
    for (bytes, items) in [
        (vec![0, 0, 0, 1, 1], vec![1u32]),
        (vec![0, 0, 0, 1, 73, 0], vec![7]),
        (vec![0, 0, 0, 1, 7, 0], vec![1 << 6]),
        (vec![0, 0, 0, 1, 7, 2], vec![1 << 8]),
        (vec![0, 0, 0, 2, 65, 0], vec![1, 0]),
        (vec![0, 0, 0, 2, 1, 0], vec![1, 1]),
    ] {
        assert_eq!(decompress(&print_of(&bytes)), Some(items.clone()), "{bytes:?}");
        let written = URL_SAFE_NO_PAD.decode(compress(&items)).expect("base64");
        assert_eq!(
            written[1..],
            bytes[1..],
            "the test's compressor writes it as Chromaprint does"
        );
    }
}

/// A print as long as a downloaded file's, every bit position used, reads back item for item.
#[test]
fn a_whole_print_reads_back_item_for_item() {
    let mut seed = 0x2545_f491_u32;
    let items: Vec<u32> = (0..1000)
        .map(|at| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            if at % 97 == 0 { u32::MAX } else { seed }
        })
        .collect();
    assert_eq!(decompress(&compress(&items)), Some(items));
    assert_eq!(decompress(&compress(&[])), Some(Vec::new()));
}

/// Text that is not a print is none: bad base64, a short header, a count its bits do not hold.
#[test]
fn text_that_is_not_a_print_reads_as_none() {
    assert_eq!(decompress("not a print!"), None);
    assert_eq!(decompress(&print_of(&[1, 0])), None);
    assert_eq!(decompress(&print_of(&[1, 0, 0, 5, 1])), None);
    assert_eq!(decompress("AQADtEqk"), None, "a print cut short");
}
