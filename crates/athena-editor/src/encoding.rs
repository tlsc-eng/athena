use encoding_rs::{EncoderResult, Encoding};
use ropey::Rope;

/// The character encoding a file is read and written in, and whether it starts with a byte order mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileEncoding {
    codec: &'static Encoding,
    bom: bool,
}

/// Each encoding offered by Reopen and Save with Encoding: its status bar name and its picker name.
const PICKABLE: [(&str, bool, &str, &str); 14] = [
    ("UTF-8", false, "UTF-8", "UTF-8"),
    ("UTF-8", true, "UTF-8 with BOM", "UTF-8 with BOM"),
    ("UTF-16LE", true, "UTF-16 LE", "UTF-16 LE"),
    ("UTF-16BE", true, "UTF-16 BE", "UTF-16 BE"),
    (
        "windows-1252",
        false,
        "Windows 1252",
        "Western (Windows 1252)",
    ),
    (
        "windows-1250",
        false,
        "Windows 1250",
        "Central European (Windows 1250)",
    ),
    (
        "ISO-8859-2",
        false,
        "ISO 8859-2",
        "Central European (ISO 8859-2)",
    ),
    (
        "windows-1251",
        false,
        "Windows 1251",
        "Cyrillic (Windows 1251)",
    ),
    ("KOI8-R", false, "KOI8-R", "Cyrillic (KOI8-R)"),
    ("Shift_JIS", false, "Shift JIS", "Japanese (Shift JIS)"),
    ("EUC-JP", false, "EUC-JP", "Japanese (EUC-JP)"),
    ("GBK", false, "GBK", "Simplified Chinese (GBK)"),
    ("Big5", false, "Big5", "Traditional Chinese (Big5)"),
    ("EUC-KR", false, "EUC-KR", "Korean (EUC-KR)"),
];

impl FileEncoding {
    pub fn utf8() -> Self {
        Self {
            codec: encoding_rs::UTF_8,
            bom: false,
        }
    }

    /// The encodings a file can be reopened or saved in, in VS Code's picker order.
    pub fn all() -> Vec<Self> {
        PICKABLE
            .iter()
            .filter_map(|(label, bom, ..)| {
                Some(Self {
                    codec: Encoding::for_label(label.as_bytes())?,
                    bom: *bom,
                })
            })
            .collect()
    }

    fn entry(&self) -> Option<&'static (&'static str, bool, &'static str, &'static str)> {
        PICKABLE.iter().find(|(label, bom, ..)| {
            Encoding::for_label(label.as_bytes()) == Some(self.codec) && *bom == self.bom
        })
    }

    /// The short name the status bar shows, as VS Code words it.
    pub fn name(&self) -> &'static str {
        match self.entry() {
            Some((_, _, name, _)) => name,
            None => self.codec.name(),
        }
    }

    /// The longer name a picker lists.
    pub fn description(&self) -> &'static str {
        match self.entry() {
            Some((.., description)) => description,
            None => self.codec.name(),
        }
    }

    fn is_utf16(&self) -> bool {
        self.codec == encoding_rs::UTF_16LE || self.codec == encoding_rs::UTF_16BE
    }

    fn bom_bytes(&self) -> &'static [u8] {
        match self.codec {
            _ if !self.bom => b"",
            c if c == encoding_rs::UTF_16LE => b"\xFF\xFE",
            c if c == encoding_rs::UTF_16BE => b"\xFE\xFF",
            _ => b"\xEF\xBB\xBF",
        }
    }
}

/// A file's text and the encoding it was read in.
pub(crate) struct Decoded {
    pub text: String,
    pub encoding: FileEncoding,
    /// Whether writing the text back in `encoding` gives exactly the bytes it was read from.
    pub round_trips: bool,
}

/// Reads `bytes` as `forced`, or else as the encoding they look like; `None` for binary data.
pub(crate) fn decode(bytes: &[u8], forced: Option<FileEncoding>) -> Option<Decoded> {
    let bom = Encoding::for_bom(bytes);
    let encoding = match forced {
        Some(f) => FileEncoding {
            codec: f.codec,
            bom: bom.is_some_and(|(codec, _)| codec == f.codec),
        },
        None => match bom {
            Some((codec, _)) => FileEncoding { codec, bom: true },
            None if bytes.iter().take(8192).any(|b| *b == 0) => return None,
            None if std::str::from_utf8(bytes).is_ok() => FileEncoding::utf8(),
            None => FileEncoding {
                codec: guess(bytes),
                bom: false,
            },
        },
    };
    let body = &bytes[encoding.bom_bytes().len()..];
    let (text, malformed) = encoding.codec.decode_without_bom_handling(body);
    let text = text.into_owned();
    // UTF-16 text is full of zero bytes, so it is judged binary by its characters instead.
    if forced.is_none() && text.chars().take(8192).any(|c| c == '\0') {
        return None;
    }
    let round_trips = !malformed
        && (encoding.codec == encoding_rs::UTF_8
            || encode_chunks(std::iter::once(text.as_str()), encoding).as_deref() == Ok(bytes));
    Some(Decoded {
        text,
        encoding,
        round_trips,
    })
}

/// Shift_JIS or EUC-JP when the bytes read cleanly as Japanese text, else Windows 1252, which
/// reads (and writes back) any byte.
fn guess(bytes: &[u8]) -> &'static Encoding {
    [encoding_rs::SHIFT_JIS, encoding_rs::EUC_JP]
        .into_iter()
        .find(|codec| reads_as_japanese(codec, bytes))
        .unwrap_or(encoding_rs::WINDOWS_1252)
}

fn reads_as_japanese(codec: &'static Encoding, bytes: &[u8]) -> bool {
    let Some(text) = codec.decode_without_bom_handling_and_without_replacement(bytes) else {
        return false;
    };
    let (mut kana, mut fits, mut wide) = (0usize, 0usize, 0usize);
    for c in text.chars().filter(|c| !c.is_ascii()) {
        wide += 1;
        match c {
            '\u{3040}'..='\u{30FF}' => {
                kana += 1;
                fits += 1;
            }
            // Half-width katakana is left out: EUC-JP's byte pairs read as it in Shift_JIS.
            '\u{3000}'..='\u{303F}' | '\u{4E00}'..='\u{9FFF}' | '\u{FF01}'..='\u{FF60}' => {
                fits += 1
            }
            _ => {}
        }
    }
    kana > 0 && fits * 10 >= wide * 8
}

/// The text in `encoding`, or the first character it has no bytes for.
pub(crate) fn encode(rope: &Rope, encoding: FileEncoding) -> Result<Vec<u8>, char> {
    encode_chunks(rope.chunks(), encoding)
}

fn encode_chunks<'a>(
    chunks: impl Iterator<Item = &'a str>,
    encoding: FileEncoding,
) -> Result<Vec<u8>, char> {
    let mut out = encoding.bom_bytes().to_vec();
    let codec = encoding.codec;
    if codec == encoding_rs::UTF_8 {
        chunks.for_each(|c| out.extend_from_slice(c.as_bytes()));
        return Ok(out);
    }
    if encoding.is_utf16() {
        let le = codec == encoding_rs::UTF_16LE;
        for unit in chunks.flat_map(str::encode_utf16) {
            out.extend(if le {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        return Ok(out);
    }
    let mut encoder = codec.new_encoder();
    let mut chunks = chunks.peekable();
    while let Some(mut src) = chunks.next() {
        let last = chunks.peek().is_none();
        loop {
            let need = encoder
                .max_buffer_length_from_utf8_without_replacement(src.len())
                .unwrap_or(src.len() * 4 + 16);
            out.reserve(need);
            let (result, read) =
                encoder.encode_from_utf8_to_vec_without_replacement(src, &mut out, last);
            src = &src[read..];
            match result {
                EncoderResult::InputEmpty => break,
                EncoderResult::OutputFull => {}
                EncoderResult::Unmappable(c) => return Err(c),
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> FileEncoding {
        FileEncoding::all()
            .into_iter()
            .find(|e| e.name() == name)
            .unwrap()
    }

    /// Opens `bytes` as detected, checks the text, and checks writing it back gives the same bytes.
    fn round_trip(bytes: &[u8], text: &str, name: &str) {
        let d = decode(bytes, None).expect("not binary");
        assert_eq!(d.text, text);
        assert_eq!(d.encoding.name(), name);
        assert!(d.round_trips, "{name}");
        assert_eq!(encode(&Rope::from_str(&d.text), d.encoding).unwrap(), bytes);
    }

    #[test]
    fn every_pickable_encoding_is_known_to_encoding_rs() {
        assert_eq!(FileEncoding::all().len(), PICKABLE.len());
        for e in FileEncoding::all() {
            assert_eq!(e.entry().map(|p| p.2), Some(e.name()));
        }
    }

    #[test]
    fn utf16_with_a_bom_round_trips_in_both_byte_orders() {
        let text = "héllo\r\nwörld ✓ 𝄞\n";
        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        round_trip(&le, text, "UTF-16 LE");
        let mut be = vec![0xFE, 0xFF];
        be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        round_trip(&be, text, "UTF-16 BE");
    }

    #[test]
    fn utf8_with_a_bom_keeps_it_out_of_the_text_and_writes_it_back() {
        round_trip(
            b"\xEF\xBB\xBFfn main() {}\r\n",
            "fn main() {}\r\n",
            "UTF-8 with BOM",
        );
        round_trip(b"plain\n", "plain\n", "UTF-8");
    }

    #[test]
    fn shift_jis_is_detected_from_its_kana_and_round_trips() {
        let text = "日本語のテキスト、です。\r\nASCII too\n";
        let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode(text);
        round_trip(&bytes, text, "Shift JIS");
    }

    #[test]
    fn euc_jp_is_told_apart_from_shift_jis() {
        let text = "ひらがなとカタカナと漢字\n";
        let (bytes, _, _) = encoding_rs::EUC_JP.encode(text);
        round_trip(&bytes, text, "EUC-JP");
    }

    #[test]
    fn latin1_text_reads_as_windows_1252_and_every_byte_round_trips() {
        round_trip(b"caf\xE9 na\xEFve \xA9\n", "café naïve ©\n", "Windows 1252");
        let all: Vec<u8> = (1..=255).collect();
        let d = decode(&all, None).unwrap();
        assert_eq!(d.encoding.name(), "Windows 1252");
        assert!(d.round_trips);
        assert_eq!(encode(&Rope::from_str(&d.text), d.encoding).unwrap(), all);
    }

    #[test]
    fn binary_data_is_still_refused_but_utf16_zeros_are_not_binary() {
        assert!(decode(&[0, 1, 2, 3], None).is_none());
        assert!(decode(b"\xFF\xFEa\0b\0", None).is_some());
        assert!(decode(b"\xFF\xFE\0\0\0\0", None).is_none());
    }

    #[test]
    fn bytes_that_do_not_fit_the_encoding_do_not_round_trip() {
        let odd = decode(b"\xFF\xFEa\0b", None).unwrap();
        assert!(!odd.round_trips, "a trailing half code unit");
        let lone = decode(b"\xFF\xFE\x00\xD8a\0", None).unwrap();
        assert!(!lone.round_trips, "an unpaired surrogate");
        let forced = decode(b"caf\xE9\n", Some(FileEncoding::utf8())).unwrap();
        assert!(!forced.round_trips);
        assert!(forced.text.contains('\u{FFFD}'));
    }

    #[test]
    fn a_character_the_encoding_lacks_is_reported_instead_of_written() {
        let rope = Rope::from_str("abc ✓ def");
        assert_eq!(encode(&rope, named("Windows 1252")), Err('✓'));
        assert!(encode(&rope, named("UTF-16 LE")).is_ok());
    }

    #[test]
    fn a_forced_encoding_takes_its_own_bom_only() {
        let d = decode(b"\xEF\xBB\xBFabc", Some(named("Windows 1252"))).unwrap();
        assert_eq!(d.text, "ï»¿abc");
        assert!(d.round_trips);
        let d = decode(b"\xEF\xBB\xBFabc", Some(FileEncoding::utf8())).unwrap();
        assert_eq!(
            (d.text.as_str(), d.encoding.name()),
            ("abc", "UTF-8 with BOM")
        );
    }

    #[test]
    fn encoding_spans_rope_chunks() {
        let text = "テスト\n".repeat(5000);
        let rope = Rope::from_str(&text);
        assert!(rope.chunks().count() > 1);
        let (want, _, _) = encoding_rs::SHIFT_JIS.encode(&text);
        assert_eq!(
            encode(&rope, named("Shift JIS")).unwrap(),
            want.into_owned()
        );
    }
}
