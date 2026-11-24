//! Damaged files: a compound file whose header, FAT or directory is broken, or a Word 97 stream
//! whose piece table is lost, still holds its text. The approach follows docboss
//! (crates/docboss-doc/src/document.rs, `find_fib` and `recover`, MIT OR Apache-2.0): the
//! WordDocument stream is taken to start at the first plausible FIB on a sector boundary and run
//! on from there; a Word 6.0/95 stream holds everything and is read as usual, and of a Word 97
//! stream, whose Table stream is lost, the text from fcMin to fcMac is read, as 16-bit text when
//! nearly every second byte is zero and as Windows-1252 otherwise.

use craft_wordmodel::para::Paragraph;
use craft_wordmodel::{Document, para_block};

use crate::DocbinError;

/// The offset of the first plausible FIB at a multiple of 64 bytes past the 512-byte header
/// (where regular and mini sectors start): wIdent 0xA5EC (or Word 6.0/95's 0xA5DC) and an nFib
/// from Word 6 to Word 2007.
pub(crate) fn find_fib(bytes: &[u8]) -> Option<usize> {
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    (512..bytes.len().saturating_sub(32))
        .step_by(64)
        .find(|&at| matches!(u16_at(at), Some(0xA5EC | 0xA5DC)) && u16_at(at + 2).is_some_and(|n| (0x0065..=0x0112).contains(&n)))
}

/// The text of a Word 97 WordDocument stream without its Table stream: fcMin to fcMac (FIB
/// offsets 0x18 and 0x1C), one paragraph per paragraph or cell mark, control characters other
/// than tabs left out.
pub(crate) fn text_only(word: &[u8], max_paragraphs: usize) -> Result<Document, DocbinError> {
    let u32_at = |at: usize| word.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    // FIB flags: fEncrypted (bit 8) and fObfuscated (bit 15) text can't be read without the
    // Table stream's key.
    let flags = word.get(0x0A..0x0C).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
    if flags & 0x0100 != 0 {
        return Err(DocbinError::PasswordRequired);
    }
    let (fc_min, fc_mac) = (u32_at(0x18).unwrap_or(0) as usize, u32_at(0x1C).unwrap_or(0) as usize);
    let bytes = word.get(fc_min.min(word.len())..fc_mac.min(word.len())).unwrap_or_default();
    if bytes.is_empty() {
        return Err(DocbinError::Malformed("no text between fcMin and fcMac".into()));
    }
    // 16-bit text when nearly every second byte is zero.
    let pairs = bytes.len() / 2;
    let zero_high = bytes.chunks_exact(2).filter(|p| p[1] == 0).count();
    let text: String = if pairs > 0 && zero_high * 10 >= pairs * 9 {
        char::decode_utf16(bytes.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]]))).map(|c| c.unwrap_or('\u{FFFD}')).collect()
    } else {
        bytes.iter().map(|&b| craft_codepage::cp1252(b)).collect()
    };
    let mut doc = Document::new();
    doc.settings.compat_mode = 11;
    doc.body.clear();
    for line in text.split(['\r', '\u{7}']) {
        if doc.body.len() >= max_paragraphs {
            return Err(DocbinError::Limit(format!("the document has more than {max_paragraphs} paragraphs")));
        }
        let s: String = line.chars().filter(|&c| c >= ' ' || c == '\t').collect();
        doc.body.push(para_block(Paragraph::with_text(&s, Default::default())));
    }
    if doc.body.len() > 1 && doc.body.last().and_then(|b| b.as_para()).is_some_and(|p| p.text.is_empty()) {
        doc.body.pop();
    }
    doc.ensure_nonempty();
    Ok(doc)
}
