//! Word for Windows 2.0 files: no compound file, the WordDocument stream's predecessor as the
//! whole file — a FIB (`wIdent` 0xA5DB) and the text. Only the text is read, as docboss reads
//! these files (crates/docboss-doc/src/document.rs, MIT OR Apache-2.0): the bytes from fcMin to
//! fcMac, in the code page of the document language, one paragraph per paragraph or cell mark.

use craft_wordmodel::para::Paragraph;
use craft_wordmodel::{Document, para_block};

use crate::DocbinError;

const W_IDENT: u16 = 0xA5DB;

/// Is `bytes` a Word for Windows 2.0 file (wIdent 0xA5DB, an nFib below Word 6's)?
pub fn is_word2(bytes: &[u8]) -> bool {
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    u16_at(0) == Some(W_IDENT) && u16_at(2).is_some_and(|n| n < 0x0065)
}

pub(crate) fn read(bytes: &[u8], max_paragraphs: usize) -> Result<Document, DocbinError> {
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    // FIB flags bit 8: encrypted, as in later versions.
    if u16_at(0x0A).unwrap_or(0) & 0x0100 != 0 {
        return Err(DocbinError::Encryption("password-protected Word 2.0 files can't be opened".into()));
    }
    let (fc_min, fc_mac) = (u32_at(0x18).unwrap_or(0) as usize, u32_at(0x1C).unwrap_or(0) as usize);
    let text = bytes.get(fc_min..fc_mac.min(bytes.len())).ok_or_else(|| DocbinError::Malformed("Word 2.0 text range outside the file".into()))?;
    let code_page = crate::word6::lid_code_page(u16_at(0x06).unwrap_or(0x0409));
    let mut doc = Document::new();
    doc.settings.compat_mode = 11;
    doc.body.clear();
    for line in text.split(|&b| b == b'\r' || b == 0x07) {
        if doc.body.len() >= max_paragraphs {
            return Err(DocbinError::Limit(format!("the document has more than {max_paragraphs} paragraphs")));
        }
        let s: String = line
            .iter()
            .filter(|&&b| b >= 0x20 || b == b'\t' || b == 0x0B)
            .map(|&b| if b == 0x0B { '\n' } else { craft_codepage::code_page_byte(code_page, b) })
            .collect();
        doc.body.push(para_block(Paragraph::with_text(&s, Default::default())));
    }
    // The text ends with a paragraph mark, which leaves an empty last line.
    if doc.body.len() > 1 && doc.body.last().and_then(|b| b.as_para()).is_some_and(|p| p.text.is_empty()) {
        doc.body.pop();
    }
    doc.ensure_nonempty();
    Ok(doc)
}
