//! A small QR code encoder for the pairing link `token new --link` prints:
//! byte mode, error correction level M, versions 1 to 10 (up to 213
//! bytes), every mask tried and the one with the lowest penalty (ISO/IEC
//! 18004 §7.8.3) kept. Written from the standard's algorithm, in the shape
//! of Project Nayuki's reference description; checked against an
//! independent encoder's output in this module's tests. Nothing here is
//! needed to pair: the link printed beside the code works on its own.

/// Largest version encoded: 57 by 57 modules, 213 bytes at level M.
const MAX_VERSION: usize = 10;
/// Error correction codewords per block at level M, by version.
const ECC_PER_BLOCK: [usize; MAX_VERSION + 1] = [0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26];
/// Error correction blocks at level M, by version.
const BLOCKS: [usize; MAX_VERSION + 1] = [0, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5];
/// Level M's two format bits.
const LEVEL_M: u32 = 0;

/// A QR code: `size` by `size` modules, `true` dark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrCode {
    pub size: usize,
    pub version: usize,
    pub mask: usize,
    modules: Vec<bool>,
    function: Vec<bool>,
}

impl QrCode {
    /// `data` in the smallest version that holds it, with the best mask.
    pub fn encode(data: &[u8]) -> Result<QrCode, String> {
        let version = (1..=MAX_VERSION)
            .find(|&v| data.len() <= capacity(v))
            .ok_or_else(|| {
                format!(
                    "{} bytes is too long for a QR code here (at most {})",
                    data.len(),
                    capacity(MAX_VERSION)
                )
            })?;
        let codewords = interleave(version, &data_codewords(version, data));
        // The lowest penalty; the first of equals, as the masks are tried.
        (0..8)
            .map(|mask| {
                let code = QrCode::with_mask(version, &codewords, mask);
                (code.penalty(), code)
            })
            .min_by_key(|(score, _)| *score)
            .map(|(_, code)| code)
            .ok_or_else(|| "no QR mask was tried".to_owned())
    }

    /// `data` in `version` with `mask`, for comparing with another encoder.
    pub fn encode_with(data: &[u8], version: usize, mask: usize) -> Result<QrCode, String> {
        if !(1..=MAX_VERSION).contains(&version) || mask > 7 || data.len() > capacity(version) {
            return Err("version, mask or length out of range".into());
        }
        let codewords = interleave(version, &data_codewords(version, data));
        Ok(QrCode::with_mask(version, &codewords, mask))
    }

    pub fn dark(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.size + x]
    }

    fn with_mask(version: usize, codewords: &[u8], mask: usize) -> QrCode {
        let size = version * 4 + 17;
        let mut code = QrCode {
            size,
            version,
            mask,
            modules: vec![false; size * size],
            function: vec![false; size * size],
        };
        code.function_patterns();
        code.place(codewords);
        code.apply_mask(mask);
        code.format_bits(mask);
        code
    }

    fn set(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
        self.function[y * self.size + x] = true;
    }

    fn function_patterns(&mut self) {
        let size = self.size;
        for i in 0..size {
            self.set(6, i, i % 2 == 0);
            self.set(i, 6, i % 2 == 0);
        }
        self.finder(3, 3);
        self.finder(size - 4, 3);
        self.finder(3, size - 4);
        let positions = alignment_positions(self.version);
        let last = positions.len().saturating_sub(1);
        for (i, &y) in positions.iter().enumerate() {
            for (j, &x) in positions.iter().enumerate() {
                let corner = (i == 0 && (j == 0 || j == last)) || (i == last && j == 0);
                if !corner {
                    self.alignment(x, y);
                }
            }
        }
        // Reserve the format areas (written for real after masking).
        self.format_bits(0);
        self.version_bits();
    }

    fn finder(&mut self, x: usize, y: usize) {
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                let (xx, yy) = (x as i32 + dx, y as i32 + dy);
                if (0..self.size as i32).contains(&xx) && (0..self.size as i32).contains(&yy) {
                    let distance = dx.abs().max(dy.abs());
                    self.set(xx as usize, yy as usize, distance != 2 && distance != 4);
                }
            }
        }
    }

    fn alignment(&mut self, x: usize, y: usize) {
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                let distance = dx.abs().max(dy.abs());
                self.set(
                    (x as i32 + dx) as usize,
                    (y as i32 + dy) as usize,
                    distance != 1,
                );
            }
        }
    }

    fn format_bits(&mut self, mask: usize) {
        let bits = format_word(mask);
        let bit = |i: u32| (bits >> i) & 1 != 0;
        let size = self.size;
        for i in 0..=5 {
            self.set(8, i as usize, bit(i));
        }
        self.set(8, 7, bit(6));
        self.set(8, 8, bit(7));
        self.set(7, 8, bit(8));
        for i in 9..15 {
            self.set(14 - i as usize, 8, bit(i));
        }
        for i in 0..8 {
            self.set(size - 1 - i as usize, 8, bit(i));
        }
        for i in 8..15 {
            self.set(8, size - 15 + i as usize, bit(i));
        }
        self.set(8, size - 8, true);
    }

    fn version_bits(&mut self) {
        if self.version < 7 {
            return;
        }
        let bits = version_word(self.version);
        for i in 0..18 {
            let dark = (bits >> i) & 1 != 0;
            let a = self.size - 11 + i % 3;
            let b = i / 3;
            self.set(a, b, dark);
            self.set(b, a, dark);
        }
    }

    fn place(&mut self, codewords: &[u8]) {
        let size = self.size;
        let total = codewords.len() * 8;
        let mut i = 0;
        let mut right = size as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vertical in 0..size {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let upward = (right + 1) & 2 == 0;
                    let y = if upward {
                        size - 1 - vertical
                    } else {
                        vertical
                    };
                    if !self.function[y * size + x] && i < total {
                        self.modules[y * size + x] = (codewords[i >> 3] >> (7 - (i & 7))) & 1 != 0;
                        i += 1;
                    }
                }
            }
            right -= 2;
        }
    }

    fn apply_mask(&mut self, mask: usize) {
        let size = self.size;
        for y in 0..size {
            for x in 0..size {
                let invert = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                if invert && !self.function[y * size + x] {
                    self.modules[y * size + x] ^= true;
                }
            }
        }
    }

    /// The standard's four penalty rules.
    fn penalty(&self) -> u32 {
        let size = self.size;
        let mut score = 0u32;
        let line = |i: usize, j: usize, rows: bool| -> bool {
            match rows {
                true => self.dark(j, i),
                false => self.dark(i, j),
            }
        };
        for rows in [true, false] {
            for i in 0..size {
                // Rule 1: runs of five or more of one colour.
                let mut run = 1;
                for j in 1..size {
                    if line(i, j, rows) == line(i, j - 1, rows) {
                        run += 1;
                    } else {
                        if run >= 5 {
                            score += 3 + (run - 5);
                        }
                        run = 1;
                    }
                }
                if run >= 5 {
                    score += 3 + (run - 5);
                }
                // Rule 3: 1:1:3:1:1 finder-like patterns with four light
                // modules on one side (outside the code counts as light).
                let at =
                    |j: i32| -> bool { (0..size as i32).contains(&j) && line(i, j as usize, rows) };
                const PATTERN: [bool; 7] = [true, false, true, true, true, false, true];
                for start in -4i32..size as i32 {
                    if (0..7).all(|k| at(start + k) == PATTERN[k as usize]) {
                        let before = (1..=4).all(|k| !at(start - k));
                        let after = (7..11).all(|k| !at(start + k));
                        if before || after {
                            score += 40;
                        }
                    }
                }
            }
        }
        // Rule 2: two-by-two blocks of one colour.
        for y in 0..size - 1 {
            for x in 0..size - 1 {
                let c = self.dark(x, y);
                if c == self.dark(x + 1, y)
                    && c == self.dark(x, y + 1)
                    && c == self.dark(x + 1, y + 1)
                {
                    score += 3;
                }
            }
        }
        // Rule 4: the balance of dark and light.
        let dark = self.modules.iter().filter(|&&d| d).count() as i64;
        let total = (size * size) as i64;
        let k = ((dark * 20 - total * 10).abs() + total - 1) / total - 1;
        score + (k.max(0) as u32) * 10
    }

    /// The code as terminal text: two modules per character cell with
    /// half blocks, light modules in bright white on a black background
    /// (so it scans on a dark or a light terminal), with a four-module
    /// light margin.
    pub fn to_ansi(&self) -> String {
        const MARGIN: i32 = 4;
        let size = self.size as i32;
        let light = |x: i32, y: i32| -> bool {
            !((0..size).contains(&x) && (0..size).contains(&y) && self.dark(x as usize, y as usize))
        };
        let mut out = String::new();
        let mut y = -MARGIN;
        while y < size + MARGIN {
            out.push_str("\x1b[97;40m");
            for x in -MARGIN..size + MARGIN {
                out.push(match (light(x, y), light(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
            out.push_str("\x1b[0m\n");
            y += 2;
        }
        out
    }

    /// One line per row, `#` dark and `.` light: for tests.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for y in 0..self.size {
            for x in 0..self.size {
                out.push(if self.dark(x, y) { '#' } else { '.' });
            }
            out.push('\n');
        }
        out
    }
}

/// Data bits in modules, after function patterns.
fn raw_modules(version: usize) -> usize {
    let mut result = (16 * version + 128) * version + 64;
    if version >= 2 {
        let aligns = version / 7 + 2;
        result -= (25 * aligns - 10) * aligns - 55;
        if version >= 7 {
            result -= 36;
        }
    }
    result
}

fn data_codeword_count(version: usize) -> usize {
    raw_modules(version) / 8 - ECC_PER_BLOCK[version] * BLOCKS[version]
}

/// Bytes `version` holds in byte mode at level M.
fn capacity(version: usize) -> usize {
    let count_bits = if version <= 9 { 8 } else { 16 };
    (data_codeword_count(version) * 8 - 4 - count_bits) / 8
}

fn alignment_positions(version: usize) -> Vec<usize> {
    if version == 1 {
        return Vec::new();
    }
    let aligns = version / 7 + 2;
    let step = (version * 4 + aligns * 2 + 1) / (aligns * 2 - 2) * 2;
    let mut result = vec![6];
    let mut tail: Vec<usize> = (0..aligns - 1)
        .map(|i| version * 4 + 10 - i * step)
        .collect();
    tail.reverse();
    result.extend(tail);
    result
}

/// The 15 format bits for level M and `mask`, BCH-coded and masked.
fn format_word(mask: usize) -> u32 {
    let data = (LEVEL_M << 3) | mask as u32;
    let mut rem = data;
    for _ in 0..10 {
        rem = (rem << 1) ^ ((rem >> 9) * 0x537);
    }
    ((data << 10) | rem) ^ 0x5412
}

/// The 18 version bits, BCH-coded.
fn version_word(version: usize) -> u32 {
    let mut rem = version as u32;
    for _ in 0..12 {
        rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
    }
    ((version as u32) << 12) | rem
}

/// Mode, count, bytes, terminator and padding, as codewords.
fn data_codewords(version: usize, data: &[u8]) -> Vec<u8> {
    let mut bits: Vec<bool> = Vec::new();
    let mut put = |value: u32, len: u32| {
        for i in (0..len).rev() {
            bits.push((value >> i) & 1 != 0);
        }
    };
    put(0b0100, 4);
    put(data.len() as u32, if version <= 9 { 8 } else { 16 });
    for &b in data {
        put(u32::from(b), 8);
    }
    let capacity_bits = data_codeword_count(version) * 8;
    let terminator = (capacity_bits - bits.len()).min(4);
    bits.extend(std::iter::repeat_n(false, terminator));
    let padding = (8 - bits.len() % 8) % 8;
    bits.extend(std::iter::repeat_n(false, padding));
    let mut out: Vec<u8> = bits
        .chunks(8)
        .map(|byte| byte.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b)))
        .collect();
    let pad = [0xEC, 0x11];
    let mut next = 0;
    while out.len() < data_codeword_count(version) {
        out.push(pad[next % 2]);
        next += 1;
    }
    out
}

/// Split into blocks, add each block's error correction, interleave.
fn interleave(version: usize, data: &[u8]) -> Vec<u8> {
    let blocks = BLOCKS[version];
    let ecc_len = ECC_PER_BLOCK[version];
    let raw = raw_modules(version) / 8;
    let short_blocks = blocks - raw % blocks;
    let short_len = raw / blocks;
    let divisor = rs_divisor(ecc_len);
    let mut all: Vec<Vec<u8>> = Vec::new();
    let mut k = 0;
    for i in 0..blocks {
        let len = short_len - ecc_len + usize::from(i >= short_blocks);
        let mut block = data[k..k + len].to_vec();
        k += len;
        let ecc = rs_remainder(&block, &divisor);
        if i < short_blocks {
            block.push(0);
        }
        block.extend(ecc);
        all.push(block);
    }
    let mut out = Vec::new();
    for i in 0..all[0].len() {
        for (j, block) in all.iter().enumerate() {
            if i != short_len - ecc_len || j >= short_blocks {
                out.push(block[i]);
            }
        }
    }
    out
}

fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((u32::from(y) >> i) & 1) * u32::from(x);
    }
    z as u8
}

fn rs_divisor(degree: usize) -> Vec<u8> {
    let mut result = vec![0u8; degree];
    result[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            result[j] = gf_mul(result[j], root);
            if j + 1 < degree {
                result[j] ^= result[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
    }
    result
}

fn rs_remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; divisor.len()];
    for &b in data {
        let factor = b ^ result.remove(0);
        result.push(0);
        for (r, &d) in result.iter_mut().zip(divisor) {
            *r ^= gf_mul(d, factor);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacities_match_the_standard_table_for_level_m() {
        let expected = [14, 26, 42, 62, 84, 106, 122, 152, 180, 213];
        for (v, want) in (1..=10).zip(expected) {
            assert_eq!(capacity(v), want, "version {v}");
        }
    }

    #[test]
    fn reed_solomon_matches_the_worked_example() {
        // "HELLO WORLD" as 1-M (alphanumeric), from the widely reproduced
        // worked example: its data codewords and their ten EC codewords.
        let data = [
            32, 91, 11, 120, 209, 114, 220, 77, 67, 64, 236, 17, 236, 17, 236, 17,
        ];
        assert_eq!(
            rs_remainder(&data, &rs_divisor(10)),
            vec![196, 35, 39, 119, 235, 215, 231, 226, 93, 23]
        );
    }

    #[test]
    fn format_and_version_words_match_the_standard() {
        // Level M, mask 0: 101010000010010; version 7: 000111110010010100.
        assert_eq!(format_word(0), 0b101010000010010);
        assert_eq!(version_word(7), 0b000111110010010100);
        assert_eq!(alignment_positions(2), vec![6, 18]);
        assert_eq!(alignment_positions(7), vec![6, 22, 38]);
    }

    /// Matrices from an independent encoder (Kazuhiko Arase's, as vendored
    /// by `qrcode-terminal` in npm), made with the same text, level M,
    /// version and mask; see `tools/qr-crosscheck.js`.
    #[test]
    fn matrices_match_an_independent_encoder() {
        for (text, version, mask, expected) in crate::companion::qr_fixtures::FIXTURES {
            let code = QrCode::encode_with(text.as_bytes(), *version, *mask).unwrap();
            assert_eq!(code.to_text(), *expected, "{text} v{version} mask {mask}");
        }
    }

    #[test]
    fn encodes_a_pairing_link_and_renders_it() {
        let link = "https://by.example.com:8421/app/#pair=0123456789abcdef0123456789abcdef";
        let code = QrCode::encode(link.as_bytes()).unwrap();
        assert_eq!(code.version, 5);
        let ansi = code.to_ansi();
        assert_eq!(ansi.lines().count(), (code.size + 8).div_ceil(2));
        assert!(QrCode::encode(&[b'x'; 214]).is_err());
    }
}
