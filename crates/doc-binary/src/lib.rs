//! Reader and writer for the Word 97-2003 binary file format (`.doc`, [MS-DOC]), WordCraft's.
//!
//! [`read`] turns the bytes of a `.doc` file into a [`craft_wordmodel::Document`], and [`write()`]
//! turns a document into a Word 97-2003 `.doc` file (the `write` module lists what it keeps).
//! Both follow the public specification ([MS-DOC], Word (.doc) Binary File Format, Microsoft Open
//! Specifications). The reader, like the other Craft parsers, is lenient by design (unknown
//! structures are skipped, bad numbers fall back to defaults) and bounded (stream sizes, piece
//! counts and property counts are capped), so hostile input yields an error or a best-effort
//! document, never a panic.
//!
//! Word 6.0/95 files, which that specification does not cover, are read by `word6` from their
//! observed layout: their text, formatting and tables so far.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
// The code came from an app whose lints allow this.
#![allow(clippy::type_complexity)]

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;

mod crypt;
mod drawing;
mod fib;
mod fkp;
mod fmt;
mod list;
mod media;
mod metadata;
mod notes;
mod officeart;
mod piece;
mod rmark;
mod recover;
mod sections;
mod sprm;
mod stsh;
mod table;
mod word2;
mod word6;
mod write;

pub use write::write;

use std::sync::Arc;

use craft_wordmodel::link::hyperlink_target;
use craft_wordmodel::para::InlineObject;
use craft_wordmodel::para::NoteKind;
use craft_wordmodel::para::Paragraph;
use craft_wordmodel::para::Run;
use craft_wordmodel::props::CharProps;
use craft_wordmodel::props::NumRef;
use craft_wordmodel::section::HeaderSet;
use craft_wordmodel::styles::StyleSheet;
use craft_wordmodel::{Document, PartKind};

use fkp::Bins;
use piece::{PieceTable, PrmRef};
use sprm::Prl;
use table::ParaOut;

/// Largest stream we materialise (bytes); the engine already caps whole files at 2 GB.
const MAX_STREAM: u64 = 1 << 30;
/// Most header/footer parts we create (matches the docx reader's cap).
const MAX_PARTS: usize = 50_000;
/// Hard ceiling on characters materialised across every story walk of one document (body,
/// headers, notes, text boxes), whatever the file's size. The working budget is tied to the
/// WordDocument stream (see [`read`]); this caps even a maximal stream.
const MAX_TOTAL_CHARS: u64 = 64 << 20;
/// Most paragraph and cell marks across every story walk of one document. The character
/// budget does not see them: a mark is one byte of the file but about 1 KB of document, so a
/// file of nothing but marks would grow memory about a thousand times faster than its size.
/// The cap bounds a flood to about 2 GB while reading (about 4 GB at the very cap, when the
/// blocks are assembled). Past it the read fails with [`DocbinError::Limit`]. Real documents stay far below it:
/// 50 paragraphs on each of 10,000 pages is 500,000, and a 10,000-page one-column table (46
/// rows, each a cell mark plus a row mark) is about 920,000.
const MAX_PARAGRAPHS: usize = 2_000_000;
/// Bytes of repeated work per document beyond the text itself: piece Prm grpprls (Prcs, up
/// to 64 KB each) interpreted (#3692), hyperlink targets (#3708), picture alt text (#3709)
/// and floating objects (#3710) copied into every run or anchor. Past it Prms are skipped,
/// copied strings left empty and floating objects dropped, each kind logged once a document
/// ([`Work::log`]).
const MAX_WORK: u64 = 64 << 20;

/// What the [`MAX_WORK`] budget pays for, and leaves out once it is spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cost {
    /// A piece's Prm grpprl: past the budget the piece keeps its CHPX formatting only.
    Formatting,
    /// A hyperlink target copied into a run of its result: past it the text is kept, unlinked.
    LinkTarget,
    /// A picture's alt text copied into an anchor: past it the picture has none.
    AltText,
    /// A floating object copied into its anchor: past it the object is left out.
    FloatingObject,
}

impl Cost {
    const ALL: [Cost; 4] = [Cost::Formatting, Cost::LinkTarget, Cost::AltText, Cost::FloatingObject];

    /// What is left out, for the log.
    fn left_out(self) -> &'static str {
        match self {
            Cost::Formatting => "some text keeps only part of its formatting",
            Cost::LinkTarget => "some hyperlinks' text is kept without its link",
            Cost::AltText => "some pictures are left without their alt text",
            Cost::FloatingObject => "some floating shapes and pictures are left out",
        }
    }
}

/// One document's [`MAX_WORK`] budget, and what it has left out (each kind logged once when
/// the document has been read, [`Work::log`]).
struct Work {
    left: std::cell::Cell<u64>,
    dropped: std::cell::Cell<[bool; 4]>,
}

impl Work {
    fn new(budget: u64) -> Self {
        Work { left: std::cell::Cell::new(budget), dropped: std::cell::Cell::new([false; 4]) }
    }

    /// Spends `cost` bytes on `what`: false (and `what` noted as left out) when they aren't left.
    fn charge(&self, cost: u64, what: Cost) -> bool {
        match self.left.get().checked_sub(cost) {
            Some(left) => {
                self.left.set(left);
                true
            }
            None => {
                let mut dropped = self.dropped.get();
                if let Some(d) = Cost::ALL.iter().position(|c| *c == what).and_then(|i| dropped.get_mut(i)) {
                    *d = true;
                }
                self.dropped.set(dropped);
                false
            }
        }
    }

    /// The kinds of work left out so far, in [`Cost::ALL`] order.
    fn dropped(&self) -> Vec<Cost> {
        Cost::ALL.iter().zip(self.dropped.get()).filter(|(_, d)| *d).map(|(c, _)| *c).collect()
    }

    /// Logs one warning for each kind left out.
    fn log(&self) {
        let dropped = self.dropped();
        for c in &dropped {
            log::warn!(".doc: the document repeats more than WordCraft copies ({MAX_WORK} bytes): {}", c.left_out());
        }
        #[cfg(test)]
        LAST_DROPPED.with(|d| *d.borrow_mut() = dropped);
    }
}

#[cfg(test)]
thread_local! {
    /// What the last document read on this thread left out for the work budget ([`Work::log`]).
    static LAST_DROPPED: std::cell::RefCell<Vec<Cost>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// `sprmCPicLocation`: the fc of a picture in the Data stream.
const C_PIC_LOCATION: u16 = 0x6A03;
/// `sprmCSymbol`: the record font and character of a symbol character ([MS-DOC] §2.6.1); the
/// text stream holds the placeholder U+0028 at its position.
const C_SYMBOL: u16 = 0x6A09;
/// `sprmPIlvl` / `sprmPIlfo`: the paragraph's list level and list.
const P_ILVL: u16 = 0x260A;
const P_ILFO: u16 = 0x460B;
/// `sprmPIstd`: the paragraph's style.
const P_ISTD: u16 = 0x4600;

/// Errors from reading a Word 97-2003 binary file.
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum DocbinError {
    /// The bytes are not a readable compound file (OLE2/CFB container).
    #[error("not a Word document (compound file): {0}")]
    Container(String),
    /// The compound file is readable but is not a Word 97-2003 document.
    #[error("not a Word 97-2003 document: {0}")]
    NotWord(String),
    /// The document is password-protected and no password was given.
    #[error("this document is protected with a password")]
    PasswordRequired,
    /// The password does not open the document.
    #[error("the password is incorrect")]
    WrongPassword,
    /// The document is encrypted in a way WordCraft can't read (AES, an external provider, an
    /// unknown header).
    #[error("encrypted document: {0}")]
    Encryption(String),
    /// The input exceeds a safety limit (stream size, piece count…).
    #[error("limit exceeded: {0}")]
    Limit(String),
    /// A structure is malformed beyond recovery.
    #[error("malformed document: {0}")]
    Malformed(String),
}

/// Is this a Word for Windows 2.0 file (no compound file), which [`read`] reads as text?
pub use word2::is_word2;

/// Is this `.doc` password-protected (XOR obfuscation, RC4 or RC4 CryptoAPI)? False for
/// anything that is not a readable Word 97-2003 file.
pub fn is_encrypted(bytes: &[u8]) -> bool {
    let Ok(comp) = open_container(bytes) else { return false };
    stream(&comp, "WordDocument").ok().flatten().is_some_and(|w| fib::peek_encryption(&w).is_some())
}

/// Open the compound file around a `.doc` with `craft-cfb`, which bounds the directory and the
/// streams it reads.
fn open_container(bytes: &[u8]) -> Result<craft_cfb::CompoundFile<'_>, DocbinError> {
    craft_cfb::CompoundFile::open_with(bytes, craft_cfb::Limits { max_stream_size: MAX_STREAM, ..craft_cfb::Limits::default() })
        .map_err(|e| DocbinError::Container(e.to_string()))
}

/// Read a `.doc` (Word 97-2003 binary) file into a [`Document`]. A password-protected file
/// gives [`DocbinError::PasswordRequired`]; see [`read_with_password`].
pub fn read(bytes: &[u8]) -> Result<Document, DocbinError> {
    read_with_password(bytes, None)
}

/// [`read`] for a file that may be password-protected: it is checked against the file's stored
/// verifier ([`DocbinError::WrongPassword`] when it does not match) and then decrypted. The
/// password is ignored for an unprotected file.
pub fn read_with_password(bytes: &[u8], password: Option<&str>) -> Result<Document, DocbinError> {
    read_limited(bytes, password, MAX_PARAGRAPHS)
}

/// [`read_with_password`] with the paragraph cap as a parameter, so tests need not build a
/// file of [`MAX_PARAGRAPHS`] marks.
fn read_limited(bytes: &[u8], password: Option<&str>, max_paragraphs: usize) -> Result<Document, DocbinError> {
    if word2::is_word2(bytes) {
        return word2::read(bytes, max_paragraphs);
    }
    // A damaged compound file (header, FAT or directory) still holds its text: see [`recover`].
    let comp = match open_container(bytes) {
        Ok(c) => c,
        Err(e) => return recover_damaged(bytes, max_paragraphs).unwrap_or(Err(e)),
    };
    let mut word = match stream(&comp, "WordDocument")? {
        Some(w) => w,
        None => return recover_damaged(bytes, max_paragraphs).unwrap_or_else(|| Err(DocbinError::NotWord("no WordDocument stream".into()))),
    };
    if word.len() as u64 > MAX_STREAM {
        return Err(DocbinError::Limit(format!("WordDocument stream is {} bytes", word.len())));
    }
    if word6::is_word6(&word) {
        return read_word6(Some(&comp), &word, max_paragraphs);
    }
    let enc = fib::peek_encryption(&word);
    // The FIB names the Table stream (0Table or 1Table); fall back to whichever exists,
    // since files written by other writers sometimes carry the wrong bit.
    let which = if let Some(e) = enc { e.which_tbl_stm } else { fib::Fib::parse(&word)?.which_tbl_stm };
    let named = if which { "1Table" } else { "0Table" };
    let other = if which { "0Table" } else { "1Table" };
    let mut table = match stream(&comp, named)? {
        Some(t) => t,
        None => stream(&comp, other)?.unwrap_or_default(),
    };
    if table.len() as u64 > MAX_STREAM {
        return Err(DocbinError::Limit(format!("Table stream is {} bytes", table.len())));
    }
    let mut data = stream(&comp, "Data")?.unwrap_or_default();
    if data.len() as u64 > MAX_STREAM {
        return Err(DocbinError::Limit(format!("Data stream is {} bytes", data.len())));
    }
    if let Some(e) = enc {
        // Verify the password first; only then is anything decrypted.
        let password = password.ok_or(DocbinError::PasswordRequired)?;
        let cipher = crypt::Cipher::open(e.flags, e.lkey, &table, password)?;
        cipher.decrypt_word(&mut word);
        cipher.decrypt_table(&mut table);
        cipher.decrypt_data(&mut data);
    }
    let fib = fib::Fib::parse(&word)?;

    // The piece table locates every character in the WordDocument stream.
    // Without a readable piece table (a lost or damaged Table stream), the text between fcMin and
    // fcMac is all that is left ([`recover::text_only`]).
    let pieces = match fib.pair(fib::pair::CLX).filter(|(_, lcb)| *lcb > 0) {
        Some((fc_clx, lcb_clx)) => match piece::PieceTable::parse(&table, fc_clx, lcb_clx) {
            Ok(p) => p,
            Err(e) => return recover::text_only(&word, max_paragraphs).map_err(|_| e),
        },
        None => return recover::text_only(&word, max_paragraphs).map_err(|_| DocbinError::Malformed("the FIB has no piece table (Clx)".into())),
    };

    // Styles, fonts and the formatting bin tables; each is optional in broken files.
    let fonts = fib.pair(fib::pair::STTBF_FFN).map(|(f, l)| stsh::fonts(&table, f, l)).unwrap_or_default();
    let raw_styles = match fib.pair(fib::pair::STSHF) {
        Some((f, l)) => stsh::parse(&table, f, l)?,
        None => Vec::new(),
    };
    let standard_fonts = fib.pair(fib::pair::STSHF).map(|(f, l)| stsh::standard_fonts(&table, f, l)).unwrap_or_default();
    let chpx_bins = match fib.pair(fib::pair::PLCF_BTE_CHPX) {
        Some((f, l)) => Bins::parse(&table, f, l)?,
        None => Bins::parse(&[], 0, 0)?,
    };
    let papx_bins = match fib.pair(fib::pair::PLCF_BTE_PAPX) {
        Some((f, l)) => Bins::parse(&table, f, l)?,
        None => Bins::parse(&[], 0, 0)?,
    };

    let mut doc = Document::new();
    // Word opens Word 97-2003 files in compatibility mode 11 (Word 2003); the model's default
    // 15 would apply Word 2013+ justification and line-breaking rules to them.
    doc.settings.compat_mode = 11;
    // The default tab stop interval is the Dop's dxaTab, twips at byte 10 ([MS-DOC] Dop, DopBase);
    // a missing or zero one keeps the model's 720 twips.
    if let Some((fc, lcb)) = fib.pair(fib::pair::DOP)
        && lcb >= 12
        && let Some(b) = table.get(fc as usize + 10..fc as usize + 12)
        && let v = u16::from_le_bytes([b[0], b[1]])
        && v > 0
    {
        doc.settings.default_tab = f32::from(v) / 20.0;
    }
    // Different odd and even pages: the Dop's fFacingPages, bit 0 of its first byte ([MS-DOC]
    // DopBase). Word also decides by it whether a section starting on an odd or even page whose
    // numbering restarts gets a blank page before it.
    if let Some((fc, lcb)) = fib.pair(fib::pair::DOP)
        && lcb >= 1
        && let Some(&b) = table.get(fc as usize)
    {
        doc.settings.even_odd_headers = b & 1 != 0;
    }
    if let Some(core) = metadata::read(&comp) {
        doc.core = core;
    }
    doc.body.clear();
    doc.styles = Arc::new(fmt::stylesheet(&raw_styles, &fonts, standard_fonts));
    doc.numbering = Arc::new(list::parse(&table, &fib, &fonts));
    let sheet: StyleSheet = StyleSheet::clone(&doc.styles);
    let sects = sections::parse(&word, &table, &fib);
    let stories = sections::header_stories(&table, &fib);
    // Subdocument layout in the CP space: main document, footnotes, headers, comments,
    // endnotes, text boxes, header text boxes.
    // Story ranges from the Table stream are hostile: each is clamped to its subdocument's
    // length here and to the last piece's CP inside `walk`.
    let ftn_base = fib.ccp.text;
    let hdd_base = fib.ccp.text.saturating_add(fib.ccp.ftn);
    let edn_base = hdd_base.saturating_add(fib.ccp.hdd).saturating_add(fib.ccp.atn);
    let mut pics = media::Pictures::for_input(word.len().saturating_add(data.len()));
    // Every materialised character comes from at least one byte of the WordDocument stream, so a
    // legitimate document's total text (body plus every header, footnote and text box, each read
    // once) cannot exceed its stream. A hostile piece or story table that aliases the same bytes
    // many times can, so the walk is held to twice the stream's size — generous for real files,
    // tight against aliasing — and never past the absolute ceiling.
    let budget = std::cell::Cell::new((word.len() as u64).saturating_mul(2).min(MAX_TOTAL_CHARS));
    let work = Work::new(MAX_WORK);
    let revs = rmark::Revisions::new(&table, &fib);
    let paragraphs = ParaBudget { left: std::cell::Cell::new(max_paragraphs), exceeded: std::cell::Cell::new(false) };

    let ctx = WalkCtx {
        word: &word,
        data: &data,
        pieces: &pieces,
        chpx_bins: &chpx_bins,
        papx_bins: &papx_bins,
        fonts: &fonts,
        raw_styles: &raw_styles,
        sheet: &sheet,
        notes: &[],
        own_note: None,
        marks: &[],
        drawn: &[],
        sect_ends: &[],
        budget: &budget,
        paragraphs: &paragraphs,
        work: &work,
        revs: &revs,
    };

    // Drawings: text box stories become parts first, then the floating shapes and pictures
    // anchored in the main document and the headers, found by their anchor CP while walking.
    let txbx_base = edn_base.saturating_add(fib.ccp.edn);
    let hdr_txbx_base = txbx_base.saturating_add(fib.ccp.txbx);
    let art = fib
        .pair(fib::pair::DGG_INFO)
        .and_then(|(f, l)| table.get(f as usize..(f as usize).saturating_add(l as usize)))
        .map(officeart::content)
        .unwrap_or_default();
    let mut tb_stories = std::collections::HashMap::new();
    for (pair, base, len) in [(fib::pair::PLCF_TXBX_TXT, txbx_base, fib.ccp.txbx), (fib::pair::PLCF_HDR_TXBX_TXT, hdr_txbx_base, fib.ccp.hdr_txbx)] {
        for (spid, ab) in drawing::textbox_stories(&table, fib.pair(pair)) {
            let Some((a, b)) = story(base, len, ab) else { continue };
            if doc.parts.len() >= MAX_PARTS || tb_stories.contains_key(&spid) {
                continue;
            }
            let blocks = story_blocks(&ctx, a, b, &mut pics, max_paragraphs)?;
            tb_stories.insert(spid, doc.add_part(PartKind::TextBox, blocks));
        }
    }
    let build = drawing::Build { bstore: &art.bstore, delay: &word, stories: &tb_stories };
    let mut drawn = drawing::anchors(&table, fib.pair(fib::pair::PLC_SPA_MOM), 0, &art.main, &build, &mut pics);
    drawn.extend(drawing::anchors(&table, fib.pair(fib::pair::PLC_SPA_HDR), hdd_base, &art.header, &build, &mut pics));
    drawn.sort_by_key(|(cp, _)| *cp);
    let ctx = WalkCtx { drawn: &drawn, ..ctx };

    // Header and footer parts: skip the six separator stories, then six stories per section
    // in the order even header, odd (default) header, even footer, odd (default) footer,
    // first header, first footer. Empty stories inherit from the previous section.
    let mut hf_ids: Vec<(usize, usize, u32)> = Vec::new();
    for si in 0..sects.len() {
        for k in 0..6usize {
            let Some(&ab) = stories.get(6 + si * 6 + k) else { break };
            let Some((a, b)) = story(hdd_base, fib.ccp.hdd, ab) else { continue };
            if doc.parts.len() >= MAX_PARTS {
                continue;
            }
            let kind = match k {
                0 | 1 | 4 => PartKind::Header,
                _ => PartKind::Footer,
            };
            let blocks = header_story_blocks(&ctx, a, b, &mut pics, max_paragraphs)?;
            if !blocks.is_empty() {
                let id = doc.add_part(kind, blocks);
                hf_ids.push((si, k, id));
            }
        }
    }

    // Footnote and endnote stories: each text range becomes a part; the reference CPs then
    // link to the part ids during the main walk.
    let mut note_links: Vec<(u32, NoteKind, u32)> = Vec::new();
    for (kind, part_kind, refs, texts, base, len) in [
        (NoteKind::Footnote, PartKind::Footnote, fib::pair::PLCF_FND_REF, fib::pair::PLCF_FND_TXT, ftn_base, fib.ccp.ftn),
        (NoteKind::Endnote, PartKind::Endnote, fib::pair::PLCF_END_REF, fib::pair::PLCF_END_TXT, edn_base, fib.ccp.edn),
    ] {
        for (cp, ab) in notes::parse_notes(&table, &fib, refs, texts) {
            let Some((a, b)) = story(base, len, ab) else { continue };
            if doc.parts.len() >= MAX_PARTS {
                continue;
            }
            // The part is added first, so the walk knows the id its own number refers to.
            let id = doc.add_part(part_kind, craft_wordmodel::Blocks::new());
            let blocks = story_blocks(&WalkCtx { own_note: Some((kind, id)), ..ctx }, a, b, &mut pics, max_paragraphs)?;
            match doc.parts.get_mut(&id) {
                Some(part) if !blocks.is_empty() => {
                    part.blocks = blocks;
                    note_links.push((cp, kind, id));
                }
                _ => {
                    doc.parts.remove(&id);
                }
            }
        }
    }
    // Sorted so the main walk finds each reference by binary search.
    note_links.sort_by_key(|(cp, _, _)| *cp);
    let marks = notes::parse_bookmarks(&table, &fib);
    let mut sect_ends: Vec<u32> = sects.iter().map(|s| s.end_cp).collect();
    sect_ends.sort_unstable();
    let ctx = WalkCtx { notes: &note_links, marks: &marks, sect_ends: &sect_ends, ..ctx };

    // Sections: each section's properties attach to the paragraph that ends it (its story of
    // header/footer parts included); the section reaching the end of the text is the final one.
    let mut paras = walk(&ctx, 0, fib.ccp.text, &mut pics);
    // The first paragraph ending at each CP, found once rather than by a scan per section.
    let mut ending: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for (i, p) in paras.iter().enumerate() {
        ending.entry(p.end_cp).or_insert(i);
    }
    let last = sects.last().map(|s| s.props.clone());
    for (si, sec) in sects.iter().enumerate() {
        let mut props = sec.props.clone();
        let id_of = |k: usize| hf_ids.iter().find(|(s, kk, _)| *s == si && *kk == k).map(|(_, _, id)| *id);
        props.headers = HeaderSet { even: id_of(0), default: id_of(1), first: id_of(4) };
        props.footers = HeaderSet { even: id_of(2), default: id_of(3), first: id_of(5) };
        let is_final = si + 1 == sects.len() || sec.end_cp >= fib.ccp.text;
        if is_final {
            continue; // handled below through `last`
        }
        if let Some(p) = ending.get(&sec.end_cp).and_then(|&i| paras.get_mut(i)) {
            p.para.section = Some(Box::new(props));
        }
    }
    if let Some(mut l) = last {
        let si = sects.len().saturating_sub(1);
        let id_of = |k: usize| hf_ids.iter().find(|(s, kk, _)| *s == si && *kk == k).map(|(_, _, id)| *id);
        l.headers = HeaderSet { even: id_of(0), default: id_of(1), first: id_of(4) };
        l.footers = HeaderSet { even: id_of(2), default: id_of(3), first: id_of(5) };
        doc.last_section = l;
    }

    if paragraphs.exceeded.get() {
        return Err(paragraph_limit(max_paragraphs));
    }
    doc.media.extend(pics.media);
    doc.revisions = revs.take();
    doc.body = table::assemble(paras);
    doc.ensure_nonempty();
    work.log();
    Ok(doc)
}

/// Read a Word 6.0/95 file ([`word6`]): its main text, with its character and paragraph
/// formatting and table rows translated to Word 97 sprms, through the same walk as a Word 97 file.
/// Its paragraph styles are read too; character styles and the other stories are not yet, and a
/// section mark reads as a page break.
fn read_word6(comp: Option<&craft_cfb::CompoundFile<'_>>, word: &[u8], max_paragraphs: usize) -> Result<Document, DocbinError> {
    let fib = word6::Fib6::parse(word)?;
    let content = word6::content(word, &fib)?;
    let mut doc = Document::new();
    // Word opens these files in compatibility mode 11 too.
    doc.settings.compat_mode = 11;
    if let Some(core) = comp.and_then(metadata::read) {
        doc.core = core;
    }
    doc.body.clear();
    // The paragraph styles, built as for Word 97 files from their translated formatting.
    doc.styles = Arc::new(fmt::stylesheet(&content.styles, &content.fonts, stsh::StandardFonts::default()));
    let sheet: StyleSheet = StyleSheet::clone(&doc.styles);
    // As for Word 97 files: the text read can't exceed twice the stream, nor the absolute ceiling.
    let budget = std::cell::Cell::new((word.len() as u64).saturating_mul(2).min(MAX_TOTAL_CHARS));
    let work = Work::new(MAX_WORK);
    let paragraphs = ParaBudget { left: std::cell::Cell::new(max_paragraphs), exceeded: std::cell::Cell::new(false) };
    let mut pics = media::Pictures::for_input(word.len());
    let revs = rmark::Revisions::none();
    let ctx = WalkCtx {
        word,
        data: &[],
        pieces: &content.pieces,
        chpx_bins: &content.chpx,
        papx_bins: &content.papx,
        fonts: &content.fonts,
        raw_styles: &content.styles,
        sheet: &sheet,
        notes: &[],
        own_note: None,
        marks: &[],
        drawn: &[],
        sect_ends: &[],
        budget: &budget,
        paragraphs: &paragraphs,
        work: &work,
        revs: &revs,
    };
    let sects = word6::sections(word, &fib);
    // Text boxes: the text box stories of the main text and of the headers become parts, then
    // the drawn objects find them by their index.
    let (hdd_base, ccp_hdd) = fib.header_doc();
    let mut drawn: Vec<(u32, InlineObject)> = Vec::new();
    for (txbx_pair, (base, len), doa_pair, cp_base) in [
        (word6::PAIR2_TXBX, fib.textbox_doc(), word6::PAIR2_DOA_MOM, 0),
        (word6::PAIR2_HDR_TXBX, fib.header_textbox_doc(), word6::PAIR2_DOA_HDR, hdd_base),
    ] {
        let mut parts = Vec::new();
        for ab in word6::textbox_stories(word, fib.pair2(txbx_pair)) {
            let id = match story(base, len, ab) {
                Some((a, b)) if doc.parts.len() < MAX_PARTS => {
                    let blocks = story_blocks(&ctx, a, b, &mut pics, max_paragraphs)?;
                    Some(doc.add_part(PartKind::TextBox, blocks))
                }
                _ => None,
            };
            parts.push(id);
        }
        drawn.extend(word6::drawn_objects(word, fib.pair2(doa_pair), cp_base).iter().map(|d| (d.cp, drawing::word6_object(d, &parts))));
    }
    drawn.sort_by_key(|(cp, _)| *cp);
    let ctx = WalkCtx { drawn: &drawn, ..ctx };
    // Header and footer parts, by section and slot (see [`word6::header_stories`]).
    let mut hf_ids: Vec<(usize, usize, u32)> = Vec::new();
    for (si, slot, ab) in word6::header_stories(word, &fib, &sects) {
        let Some((a, b)) = story(hdd_base, ccp_hdd, ab) else { continue };
        if doc.parts.len() >= MAX_PARTS {
            break;
        }
        let kind = if matches!(slot, 0 | 1 | 4) { PartKind::Header } else { PartKind::Footer };
        let blocks = header_story_blocks(&ctx, a, b, &mut pics, max_paragraphs)?;
        if !blocks.is_empty() {
            hf_ids.push((si, slot, doc.add_part(kind, blocks)));
        }
    }
    let with_headers = |si: usize, props: &craft_wordmodel::section::SectionProps| {
        let mut props = props.clone();
        let id_of = |k: usize| hf_ids.iter().find(|(s, kk, _)| *s == si && *kk == k).map(|(_, _, id)| *id);
        props.headers = HeaderSet { even: id_of(0), default: id_of(1), first: id_of(4) };
        props.footers = HeaderSet { even: id_of(2), default: id_of(3), first: id_of(5) };
        props
    };
    // Sections: each one's properties go on the paragraph its section mark ends, the last
    // section's on the document, as for Word 97 files.
    let mut sect_ends: Vec<u32> = sects.iter().map(|s| s.end).collect();
    sect_ends.sort_unstable();
    let ctx = WalkCtx { sect_ends: &sect_ends, ..ctx };
    let mut paras = walk(&ctx, 0, fib.ccp_text, &mut pics);
    if paragraphs.exceeded.get() {
        return Err(paragraph_limit(max_paragraphs));
    }
    if let Some(last) = sects.last() {
        doc.last_section = with_headers(sects.len() - 1, &last.props);
    }
    for (si, sect) in sects.iter().enumerate() {
        if si + 1 == sects.len() || sect.end >= fib.ccp_text {
            continue;
        }
        let at = paras.partition_point(|p| p.end_cp < sect.end);
        if let Some(p) = paras.get_mut(at).filter(|p| p.end_cp == sect.end) {
            p.para.section = Some(Box::new(with_headers(si, &sect.props)));
        }
    }
    doc.media.extend(pics.media);
    doc.body = table::assemble(paras);
    doc.ensure_nonempty();
    work.log();
    Ok(doc)
}

/// A compound file that can't be opened or has lost its WordDocument entry: the WordDocument
/// stream read on from the first plausible FIB ([`recover::find_fib`]). `None` when there is no
/// compound file signature or no FIB to be found.
fn recover_damaged(bytes: &[u8], max_paragraphs: usize) -> Option<Result<Document, DocbinError>> {
    if !craft_cfb::sniff(bytes) {
        return None;
    }
    let word = bytes.get(recover::find_fib(bytes)?..)?;
    Some(if word6::is_word6(word) { read_word6(None, word, max_paragraphs) } else { recover::text_only(word, max_paragraphs) })
}

fn paragraph_limit(max_paragraphs: usize) -> DocbinError {
    DocbinError::Limit(format!("the document has more than {max_paragraphs} paragraphs and table cells"))
}

/// Walks one story into blocks. A walk that spent the shared paragraph budget is refused before
/// its partial list is assembled, so a flood does not build blocks it will throw away.
fn story_blocks(
    ctx: &WalkCtx<'_>,
    a: u32,
    b: u32,
    pics: &mut media::Pictures,
    max_paragraphs: usize,
) -> Result<craft_wordmodel::Blocks, DocbinError> {
    let paras = walk(ctx, a, b, pics);
    if ctx.paragraphs.exceeded.get() {
        return Err(paragraph_limit(max_paragraphs));
    }
    Ok(table::assemble(paras))
}

/// A header or footer story's blocks without its guard paragraph mark. [MS-DOC] 2.3.3: a
/// non-empty header story ends with a paragraph mark that only separates it from the next story
/// and is not part of its contents, so a one-line header is stored as `text\r\r`. Laid out, the
/// guard would add an empty paragraph to every header and footer and push the body down when
/// the header reaches past the top margin (#5922).
///
/// Only an empty last paragraph after an ordinary one is dropped: a story that is nothing but a
/// mark stays a part (an empty header, not one linked to the previous section), and a mark right
/// after a table row stays the paragraph that closes the table.
fn header_story_blocks(
    ctx: &WalkCtx<'_>,
    a: u32,
    b: u32,
    pics: &mut media::Pictures,
    max_paragraphs: usize,
) -> Result<craft_wordmodel::Blocks, DocbinError> {
    let mut paras = walk(ctx, a, b, pics);
    if ctx.paragraphs.exceeded.get() {
        return Err(paragraph_limit(max_paragraphs));
    }
    let outside_table = |p: &table::ParaOut| p.row.as_ref().is_none_or(|r| !r.in_table && !r.ttp && !r.cell_mark);
    if let [.., prev, last] = paras.as_slice() {
        let guard = outside_table(last) && last.para.text.is_empty() && last.para.objects.is_empty() && last.para.section.is_none();
        if guard && outside_table(prev) {
            paras.pop();
        }
    }
    Ok(table::assemble(paras))
}

/// A story's CP range `[a, b)`, relative to a subdocument starting at `base` with `len`
/// CPs, as absolute CPs clamped to that subdocument; `None` when empty.
fn story(base: u32, len: u32, (a, b): (u32, u32)) -> Option<(u32, u32)> {
    let (a, b) = (a.min(len), b.min(len));
    if b <= a {
        return None;
    }
    Some((base.checked_add(a)?, base.checked_add(b)?))
}

/// Everything the walker needs besides its CP range.
struct WalkCtx<'a> {
    word: &'a [u8],
    data: &'a [u8],
    pieces: &'a PieceTable,
    chpx_bins: &'a Bins,
    papx_bins: &'a Bins,
    fonts: &'a [String],
    raw_styles: &'a [stsh::RawStyle],
    sheet: &'a StyleSheet,
    /// (reference CP in the main document, kind, part id) for notes.
    notes: &'a [(u32, NoteKind, u32)],
    /// The note whose story is being walked (kind, part id): its own reference mark (0x02, the
    /// number at the start of its text) refers to it.
    own_note: Option<(NoteKind, u32)>,
    /// Bookmark boundaries (main-document CPs), sorted for a sequential walk.
    marks: &'a [(u32, notes::Mark)],
    /// Floating shapes and pictures by anchor CP (absolute), sorted.
    drawn: &'a [(u32, InlineObject)],
    /// Where sections end (main-document CPs, sorted): the section mark (0x0C) just before
    /// each ends the section's last paragraph, it isn't a page break inside it.
    sect_ends: &'a [u32],
    /// Characters still allowed across every story walk of this document. Story ranges (headers,
    /// footnotes, text boxes) are hostile and can each span a whole subdocument, so thousands of
    /// them could otherwise materialise the same text thousands of times; this bounds the total.
    budget: &'a std::cell::Cell<u64>,
    /// Paragraph and cell marks still allowed across every story walk ([`MAX_PARAGRAPHS`]).
    paragraphs: &'a ParaBudget,
    /// Repeated-work bytes left ([`MAX_WORK`]), and what they left out.
    work: &'a Work,
    /// Tracked insertions and deletions met so far.
    revs: &'a rmark::Revisions,
}

/// The paragraph and cell marks still allowed in this document, and whether a walk ran into
/// the end of them (the read then fails).
struct ParaBudget {
    left: std::cell::Cell<usize>,
    exceeded: std::cell::Cell<bool>,
}

impl ParaBudget {
    /// Spends one mark; false (and the overrun noted) when none are left.
    fn spend(&self) -> bool {
        match self.left.get().checked_sub(1) {
            Some(left) => {
                self.left.set(left);
                true
            }
            None => {
                self.exceeded.set(true);
                false
            }
        }
    }
}

/// State while assembling one paragraph from per-CP formatting.
#[derive(Default)]
struct ParaBuild {
    text: String,
    runs: Vec<Run>,
    props: craft_wordmodel::props::ParaProps,
    mark: CharProps,
    /// Which resolved formatting ([`FmtState::generation`]) the last character came from, while
    /// the last run is still the one it extended: the same generation means equal properties.
    run_gen: Option<u64>,
    /// Inline objects, each anchored at a U+FFFC pushed into `text`.
    objects: Vec<InlineObject>,
    /// The paragraph style the characters' toggles invert, looked up at the first character
    /// (its mark, which sets [`ParaBuild::props`], comes last).
    style_ahead: Option<Option<String>>,
}

impl ParaBuild {
    fn push(&mut self, c: char, props: CharProps) {
        self.run_gen = None;
        let len = c.len_utf8();
        match self.runs.last_mut() {
            Some(r) if r.props == props => r.len += len,
            _ => self.runs.push(Run { len, props }),
        }
        self.text.push(c);
    }

    /// [`ParaBuild::push`] for properties that are resolved once and shared by many characters:
    /// they are cloned only when a run starts, and not even compared while `generation` stays
    /// the same.
    fn push_shared(&mut self, c: char, props: &CharProps, generation: u64) {
        let len = c.len_utf8();
        match self.runs.last_mut() {
            Some(r) if self.run_gen == Some(generation) || r.props == *props => r.len += len,
            _ => self.runs.push(Run { len, props: props.clone() }),
        }
        self.run_gen = Some(generation);
        self.text.push(c);
    }

    /// Append `c` to the current run, whose properties the caller knows it matches.
    fn extend(&mut self, c: char) {
        if let Some(r) = self.runs.last_mut() {
            r.len += c.len_utf8();
        }
        self.text.push(c);
    }
}

/// Walk a CP range, splitting paragraphs at 0x0D/0x07 marks, applying the direct character
/// formatting of each CP and the paragraph formatting of each mark. Works for the main
/// document and for any subdocument range (headers, footers, notes…). Field characters,
/// note references, bookmarks and inline pictures become inline objects; pictures go into
/// `pics`, which shares one media entry per Data-stream location.
///
/// The range is clamped to the last piece's CP, and unreadable stretches (gaps, truncated
/// pieces) are skipped a whole piece at a time, so the work is bounded by the text that
/// really exists, not by the CP numbers a file claims.
fn walk<'a>(ctx: &WalkCtx<'a>, cp_start: u32, cp_end: u32, pics: &mut media::Pictures) -> Vec<ParaOut> {
    let cp_end = cp_end.min(ctx.pieces.cp_end());
    let mut out = Vec::new();
    let mut pb = ParaBuild::default();
    let mut cp = cp_start;
    let mut marks = ctx.marks.iter().peekable();
    // Field assembly state: instruction characters are swallowed, the result is kept until
    // the end character, then emitted as one object (hyperlinks keep their result text).
    let mut field: Option<FieldBuild> = None;
    // Fields whose result reaches over a paragraph mark or holds a field of its own (a table of
    // contents, an index) are written as range fields: a `FieldStart` was emitted and their
    // result is walked as ordinary text, up to the `FieldEnd` their end character adds. These
    // are the ones still open, outermost first; each keeps whether it is a hyperlink's target.
    let mut ranges: Vec<Option<String>> = Vec::new();
    let mut fmt = FmtState::default();
    while cp < cp_end {
        // Bookmark boundaries at this CP (main-document walks only; subdocument walks get
        // an empty table and `cp` never matches the main-document CPs).
        // `<=` so a boundary inside a skipped stretch is still emitted (late) rather than
        // blocking every later one.
        while marks.peek().is_some_and(|&(mcp, _)| *mcp <= cp) {
            if let Some((_, m)) = marks.next() {
                let obj = match m {
                    notes::Mark::Start(name) => InlineObject::BookmarkStart { name: name.clone() },
                    notes::Mark::End(name) => InlineObject::BookmarkEnd { name: name.clone() },
                };
                pb.push('\u{FFFC}', CharProps::default());
                pb.objects.push(obj);
            }
        }
        let (c, units) = match ctx.pieces.char_at(ctx.word, cp) {
            Ok(found) => found,
            Err(next) => {
                cp = next;
                continue;
            }
        };
        // Every real character counts against the document-wide budget; once it is spent, stop
        // (a hostile file with thousands of story ranges over the same text can't multiply on).
        if ctx.budget.get() == 0 {
            break;
        }
        ctx.budget.set(ctx.budget.get() - 1);
        // A section mark (0x0C) at a section's end is that section's last paragraph mark.
        let section_mark = c == '\u{C}' && ctx.sect_ends.binary_search(&cp.saturating_add(1)).is_ok();
        match c {
            '\r' | '\u{7}' | '\u{C}' if c != '\u{C}' || section_mark => {
                if !ctx.paragraphs.spend() {
                    break;
                }
                // A field result that goes on past this mark becomes a range field, so its
                // paragraphs stay paragraphs ([MS-DOC] §2.8.25: a field's result may hold any
                // content, paragraph marks included).
                if field.as_ref().is_some_and(|f| f.depth == 1 && f.sep.is_some())
                    && let Some(f) = field.take()
                {
                    ranges.push(open_range(&mut pb, f, ctx.work));
                }
                // Paragraph mark: its PAPX covers the FC range of the paragraph it ends, so
                // querying at the mark's own FC finds that paragraph's properties.
                let fc_mark = ctx.pieces.fc_of_cp(cp).unwrap_or(0);
                let (istd, papx) = ctx.papx_bins.papx(ctx.word, fc_mark);
                // The Prm of the piece holding the mark adds its paragraph sprms after the PAPX's
                // ([MS-DOC] §2.4.6.1, step 5), as Word 6/95's fast saves record a paragraph change.
                // A PAPX too large for its FKP holds only sprmPHugePapx, the Data-stream offset of
                // the real grpprl (a PrcData, [MS-DOC] sprmPHugePapx): that one is read in its place.
                let huge = huge_papx(papx, ctx.data, ctx.work);
                let papx: &[u8] = huge.as_deref().unwrap_or(papx);
                let extra = prm_paragraph_sprms(ctx.pieces.piece_prm(cp), ctx.work);
                let combined;
                let papx: &[u8] = if extra.is_empty() {
                    papx
                } else {
                    // Re-framed from its Prls, so that a pad byte at its end can't swallow the
                    // first appended opcode.
                    let mut v: Vec<u8> = sprm::iter(papx).flat_map(|p| p.op.to_le_bytes().into_iter().chain(p.operand.iter().copied())).collect();
                    v.extend_from_slice(&extra);
                    combined = v;
                    &combined
                };
                // sprmPIstd in the grpprl names the paragraph's style in place of the PAPX's istd
                // ([MS-DOC] sprmPIstd), as Word writes it after a tracked formatting change.
                let istd = sprm::iter(papx)
                    .filter(|p| p.op == P_ISTD)
                    .filter_map(|p| p.operand.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]])))
                    .last()
                    .unwrap_or(istd);
                if let Some(st) = ctx.raw_styles.get(istd as usize).filter(|s| !s.name.is_empty()) {
                    pb.props.style = Some(fmt::style_id(&st.name));
                }
                let mut row = table::decode(papx);
                row.cell_mark = c == '\u{7}';
                // Only table marks keep their table context; a plain paragraph would carry
                // a `RowInfo` (about 300 bytes) it never uses.
                let row = row.is_table().then(|| Box::new(row));
                let mut ilvl: Option<u8> = None;
                let mut ilfo: Option<i32> = None;
                for prl in sprm::iter(papx) {
                    if prl.op == P_ILVL {
                        ilvl = Some(prl.operand.first().copied().unwrap_or(0).min(8));
                        continue;
                    }
                    if prl.op == P_ILFO {
                        ilfo = Some(match prl.operand {
                            [b0, b1, ..] => i16::from_le_bytes([*b0, *b1]) as i32,
                            _ => 0,
                        });
                        continue;
                    }
                    if prl.sgc() == 1 {
                        fmt::apply_para(&mut pb.props, &prl);
                    }
                }
                // The paragraph's own list, else its style's list ([MS-DOC] §2.6.2): a level of its
                // own applies to the style's list, and an explicit "no list" (0 or 0xF801) ends
                // the style's.
                let style_num = pb.props.style.as_deref().and_then(|id| ctx.sheet.get(id)).and_then(|s| s.para.numbering);
                match ilfo {
                    Some(f) => match num_of(f) {
                        Some(num) => pb.props.numbering = Some(NumRef { num, level: ilvl.unwrap_or(0) }),
                        None if style_num.is_some() => pb.props.numbering = Some(NumRef { num: 0, level: 0 }),
                        None => {}
                    },
                    None => {
                        if let (Some(level), Some(s)) = (ilvl, style_num)
                            && level != s.level
                        {
                            pb.props.numbering = Some(NumRef { num: s.num, level });
                        }
                    }
                }
                fmt::settle_frame(&mut pb.props);
                let fc = ctx.pieces.fc_of_cp(cp).unwrap_or(0);
                let chpx = ctx.chpx_bins.chpx(ctx.word, fc);
                // The mark's toggles invert its paragraph style's values, as the characters' do.
                let toggle_base = pb.props.style.as_deref().and_then(|id| ctx.sheet.get(id)).map(|sty| sty.chr.clone()).unwrap_or_default();
                pb.mark = char_props(chpx, ctx.fonts, CharProps::default(), &toggle_base);
                // A tracked paragraph mark: an inserted or deleted paragraph break.
                ctx.revs.mark(&mut pb.mark, chpx, ctx.pieces.piece_prm(cp));
                let done = std::mem::take(&mut pb);
                out.push(ParaOut {
                    para: Paragraph {
                        text: done.text,
                        runs: done.runs,
                        props: done.props,
                        mark: done.mark,
                        objects: done.objects,
                        ..Default::default()
                    },
                    end_cp: cp.saturating_add(1),
                    row,
                });
            }
            // Field begin/separator/end ([MS-DOC] §2.9.84 Plcfld).
            '\u{13}' => {
                match field.as_mut() {
                    // A field inside another's result: the outer one becomes a range field and
                    // the inner one is read on its own, its instruction hidden as any other's.
                    Some(f) if f.depth == 1 && f.sep.is_some() => {
                        if let Some(f) = field.take() {
                            ranges.push(open_range(&mut pb, f, ctx.work));
                        }
                        field = Some(FieldBuild { depth: 1, instr: String::new(), sep: None, anchor: anchor_props(ctx, cp) });
                    }
                    // A field inside another's instruction is part of that instruction.
                    Some(f) => {
                        f.depth = f.depth.saturating_add(1);
                    }
                    None => field = Some(FieldBuild { depth: 1, instr: String::new(), sep: None, anchor: anchor_props(ctx, cp) }),
                }
                cp = cp.saturating_add(1);
                continue;
            }
            '\u{14}' => {
                if let Some(f) = field.as_mut()
                    && f.depth == 1
                    && f.sep.is_none()
                {
                    f.sep = Some(FieldResult::default());
                }
                cp = cp.saturating_add(1);
                continue;
            }
            '\u{15}' => {
                if let Some(mut f) = field.take() {
                    if f.depth > 1 {
                        f.depth -= 1;
                        field = Some(f);
                    } else {
                        finish_field(&mut pb, f, ctx.work);
                    }
                } else if ranges.pop().is_some() {
                    pb.push('\u{FFFC}', anchor_props(ctx, cp));
                    pb.objects.push(InlineObject::FieldEnd);
                }
                cp = cp.saturating_add(1);
                continue;
            }
            '\u{1}' => {
                // Inline picture: the CHPX of the anchor carries sprmCPicLocation.
                let fc = ctx.pieces.fc_of_cp(cp).unwrap_or(0);
                let chpx = ctx.chpx_bins.chpx(ctx.word, fc);
                let fc_pic = pic_location(chpx);
                match fc_pic.and_then(|at| pics.get(ctx.data, at)) {
                    Some(p) => {
                        // Every anchor of a picture gets its own copy of the alt text; the
                        // copies are charged to the work budget (#3709).
                        let alt = if ctx.work.charge(p.alt.len() as u64, Cost::AltText) { p.alt.clone() } else { String::new() };
                        let mut props = CharProps::default();
                        ctx.revs.mark(&mut props, chpx, ctx.pieces.piece_prm(cp));
                        pb.push('\u{FFFC}', props);
                        pb.objects.push(InlineObject::Image {
                            media: p.key.clone(),
                            w: p.w,
                            h: p.h,
                            alt,
                            float: Default::default(),
                            crop: p.crop,
                            fx: Default::default(),
                            ole: None,
                        });
                    }
                    None => pics.dropped(cp),
                }
            }
            '\u{8}' => {
                // Drawn object anchor: the floating shape or picture anchored at this CP.
                let at = ctx.drawn.partition_point(|(c, _)| *c < cp);
                // Each copy (a story can be walked more than once) is charged to the work budget
                // by what a clone copies: a picture's alt text, a group with all its members (#3710).
                for (_, obj) in ctx.drawn.get(at..).unwrap_or_default().iter().take_while(|(c, _)| *c == cp) {
                    if !ctx.work.charge(object_cost(obj), Cost::FloatingObject) {
                        continue;
                    }
                    pb.push('\u{FFFC}', anchor_props(ctx, cp));
                    pb.objects.push(obj.clone());
                }
            }
            '\u{2}' => {
                // Footnote or endnote reference; custom symbols share the same character.
                let at = ctx.notes.partition_point(|(c, _, _)| *c < cp);
                if let Some((_, kind, id)) = ctx.notes.get(at).filter(|(c, _, _)| *c == cp) {
                    pb.push('\u{FFFC}', anchor_props(ctx, cp));
                    pb.objects.push(InlineObject::NoteRef { kind: *kind, id: *id, custom: String::new() });
                } else if let Some((kind, id)) = ctx.own_note {
                    // In a note's own story: the note's number.
                    pb.push('\u{FFFC}', anchor_props(ctx, cp));
                    pb.objects.push(InlineObject::NoteRef { kind, id, custom: String::new() });
                }
            }
            _ if field.is_none() && ranges.last().is_some_and(Option::is_some) && !matches!(c, '\u{0}'..='\u{8}' | '\u{A}' | '\u{D}'..='\u{12}' | '\u{16}'..='\u{1D}') => {
                // The visible result of a hyperlink written as a range field keeps its target.
                let link = ranges.last().cloned().flatten();
                let c = match c {
                    '\u{B}' => '\n',
                    '\u{1E}' => '\u{2011}',
                    '\u{1F}' => '\u{AD}',
                    c => c,
                };
                let at = pb.text.len();
                push_formatted(&mut pb, cp, c, ctx, None, &mut fmt);
                // Re-pushed with the target: `push` joins it to the linked run before it.
                if let Some(link) = link
                    && pb.text.len() > at
                    && pb.runs.last().is_some_and(|r| r.props.link.is_none())
                    && let Some(mut props) = fmt.memo.as_ref().map(|m| m.props.clone())
                {
                    let pushed: String = pb.text.split_off(at);
                    if let Some(r) = pb.runs.last_mut() {
                        r.len = r.len.saturating_sub(pushed.len());
                        if r.len == 0 {
                            pb.runs.pop();
                        }
                    }
                    props.link = Some(link);
                    for ch in pushed.chars() {
                        pb.push(ch, props.clone());
                    }
                }
            }
            '\t' => push_formatted(&mut pb, cp, '\t', ctx, field.as_mut(), &mut fmt),
            '\u{B}' => push_formatted(&mut pb, cp, '\n', ctx, field.as_mut(), &mut fmt),
            '\u{C}' => push_formatted(&mut pb, cp, '\u{C}', ctx, field.as_mut(), &mut fmt),
            '\u{E}' => push_formatted(&mut pb, cp, '\u{E}', ctx, field.as_mut(), &mut fmt),
            '\u{1E}' => push_formatted(&mut pb, cp, '\u{2011}', ctx, field.as_mut(), &mut fmt),
            '\u{1F}' => push_formatted(&mut pb, cp, '\u{AD}', ctx, field.as_mut(), &mut fmt),
            // Object anchors, comment references and other special characters are dropped
            // (or handled by later passes).
            c if (c as u32) < 0x20 => {}
            c => push_formatted(&mut pb, cp, c, ctx, field.as_mut(), &mut fmt),
        }
        cp = cp.saturating_add(units);
    }
    if !pb.text.is_empty() {
        let done = pb;
        out.push(ParaOut {
            para: Paragraph { text: done.text, runs: done.runs, props: done.props, mark: done.mark, objects: done.objects, ..Default::default() },
            end_cp: cp_end,
            row: None,
        });
    }
    out
}

/// Field assembly: `instr` collects everything before the separator (nested fields
/// flattened), `sep` holds the cached result once the separator was seen.
struct FieldBuild {
    depth: u32,
    instr: String,
    sep: Option<FieldResult>,
    /// The begin character's revision marks, for the field object's anchor.
    anchor: CharProps,
}

/// A field's cached result: its text and, as runs, the formatting of that text. A run is added
/// only when the formatting changes, so a result of millions of equally formatted characters
/// costs one `CharProps`, not one per character (365 bytes each).
#[derive(Default)]
struct FieldResult {
    text: String,
    /// (byte length in `text`, formatting) in order; adjacent runs differ.
    runs: Vec<(usize, CharProps)>,
}

impl FieldResult {
    fn push(&mut self, c: char, props: CharProps) {
        let len = c.len_utf8();
        match self.runs.last_mut() {
            Some((n, last)) if *last == props => *n += len,
            _ => self.runs.push((len, props)),
        }
        self.text.push(c);
    }
}

/// Bytes a clone of `obj` copies: the object itself plus a picture's alt text, a group with all
/// its members (#3710).
fn object_cost(obj: &InlineObject) -> u64 {
    let shallow = std::mem::size_of::<InlineObject>() as u64;
    match obj {
        InlineObject::Image { alt, .. } => shallow.saturating_add(alt.len() as u64),
        InlineObject::Group { children, .. } => children.iter().fold(shallow, |n, c| n.saturating_add(object_cost(&c.obj))),
        _ => shallow,
    }
}

/// The properties of an object anchor at `cp`: only its revision marks, so a tracked
/// insertion or deletion of a picture, note reference or field survives.
fn anchor_props(ctx: &WalkCtx<'_>, cp: u32) -> CharProps {
    let fc = ctx.pieces.fc_of_cp(cp).unwrap_or(0);
    let mut props = CharProps::default();
    ctx.revs.mark(&mut props, ctx.chpx_bins.chpx(ctx.word, fc), ctx.pieces.piece_prm(cp));
    props
}

/// Emit a finished field: HYPERLINK keeps its result text as a linked run; every other
/// field becomes one inline object with its cached result.
///
/// The target is copied once per run of equally formatted result text, not per character,
/// and each copy is charged to `work` ([`MAX_WORK`]); once that is spent the rest of the
/// result is kept as plain text (#3708).
fn finish_field(pb: &mut ParaBuild, f: FieldBuild, work: &Work) {
    let instr = f.instr.trim().to_string();
    let result = f.sep.unwrap_or_default();
    if let Some(target) = hyperlink_target(&instr) {
        let cost = target.len() as u64;
        let mut chars = result.text.chars();
        for (len, props) in result.runs {
            let mut linked = props;
            if work.charge(cost, Cost::LinkTarget) {
                linked.link = Some(target.clone());
            }
            let mut left = len;
            let Some(first) = chars.next() else { break };
            pb.push(first, linked);
            left = left.saturating_sub(first.len_utf8());
            while left > 0 {
                let Some(c) = chars.next() else { break };
                pb.extend(c);
                left = left.saturating_sub(c.len_utf8());
            }
        }
        return;
    }
    // A result whose tracked insertions or deletions are not the field's own (an updated page
    // number keeps the old one as deleted text) is written as a range field, so each run keeps
    // its revision mark; one field object would hold the inserted and the deleted text as a
    // single result.
    // So is a result in more than one format: Word shows the result as its runs are formatted,
    // and one field object has one format.
    if result.runs.len() > 1 || result.runs.iter().any(|(_, p)| (p.ins, p.del) != (f.anchor.ins, f.anchor.del)) {
        let anchor = f.anchor.clone();
        let f = FieldBuild { sep: Some(result), ..f };
        // Not a hyperlink (handled above), so there is no link target to carry on.
        open_range(pb, f, work);
        pb.push('\u{FFFC}', anchor);
        pb.objects.push(InlineObject::FieldEnd);
        return;
    }
    // The object shows its result in the result's format (a FILENAME result in bold 22 pt), not
    // in the field begin character's.
    let props = result.runs.first().map_or(f.anchor, |(_, p)| p.clone());
    pb.push('\u{FFFC}', props);
    pb.objects.push(InlineObject::Field { instr, result: result.text, locked: false, dirty: false, code: false, form: String::new() });
}

/// Turn a field whose result goes on past a paragraph mark or holds another field into a range
/// field: a `FieldStart` with its instruction, then the result read so far as text. Returns the
/// hyperlink target its later result text keeps, if it is a hyperlink.
fn open_range(pb: &mut ParaBuild, f: FieldBuild, work: &Work) -> Option<String> {
    let instr = f.instr.trim().to_string();
    let target = hyperlink_target(&instr);
    pb.push('\u{FFFC}', f.anchor);
    pb.objects.push(InlineObject::FieldStart { instr, locked: false, dirty: false });
    let result = f.sep.unwrap_or_default();
    let mut chars = result.text.chars();
    for (len, mut props) in result.runs {
        if let Some(t) = &target
            && work.charge(t.len() as u64, Cost::LinkTarget)
        {
            props.link = Some(t.clone());
        }
        let mut left = len;
        let Some(first) = chars.next() else { break };
        pb.push(first, props);
        left = left.saturating_sub(first.len_utf8());
        while left > 0 {
            let Some(c) = chars.next() else { break };
            pb.extend(c);
            left = left.saturating_sub(c.len_utf8());
        }
    }
    target
}

/// `sprmCPicLocation` from a CHPX grpprl: the fc of the picture in the Data stream.
fn pic_location(chpx: &[u8]) -> Option<u32> {
    for prl in sprm::iter(chpx) {
        if prl.op == C_PIC_LOCATION {
            return match prl.operand {
                [b0, b1, b2, b3, ..] => Some(u32::from_le_bytes([*b0, *b1, *b2, *b3])),
                _ => None,
            };
        }
    }
    None
}

/// `sprmCSymbol` from a CHPX grpprl: the font table index and the character code of a symbol
/// character ([MS-DOC] §2.6.1, CSymbolOperand).
fn symbol_operand(chpx: &[u8]) -> Option<(u16, u16)> {
    for prl in sprm::iter(chpx) {
        if prl.op == C_SYMBOL
            && let [f0, f1, c0, c1, ..] = prl.operand
        {
            return Some((u16::from_le_bytes([*f0, *f1]), u16::from_le_bytes([*c0, *c1])));
        }
    }
    None
}

/// `sprmPIlfo` operand (a signed 16-bit value, [MS-DOC] §2.6.2) → the num id, if the
/// paragraph is in a list. 0x0001-0x07FE is a 1-based LFO index; 0xF802-0xFFFF (-2046..=-1)
/// is the negation of one and keeps the paragraph's own indents, which we do by leaving the
/// level's indents out of the paragraph (they never enter `ParaProps` anyway). 0 and 0xF801
/// mean "not in a list".
fn num_of(ilfo: i32) -> Option<u32> {
    match ilfo {
        0x0001..=0x07FE | -0x07FE..=-1 => Some(ilfo.unsigned_abs()),
        _ => None,
    }
}

/// Resolve the character properties for the character at `cp`: the CHPX grpprl of its FC,
/// plus the piece's Prm, over the paragraph style's base character props. Inside a field,
/// the character is recorded into the instruction or the cached result instead of (or as
/// well as) the visible text.
fn push_formatted<'a>(pb: &mut ParaBuild, cp: u32, c: char, ctx: &WalkCtx<'a>, field: Option<&mut FieldBuild>, st: &mut FmtState<'a>) {
    // The piece and the CHPX run of the previous character usually cover this one too.
    let span = match st.span.filter(|s| s.contains(cp)) {
        Some(s) => Some(s),
        None => {
            st.span = ctx.pieces.span(cp);
            st.span
        }
    };
    let fc = span.and_then(|s| s.fc_at(cp)).unwrap_or(0);
    let run = match st.chpx.filter(|r| r.contains(fc)) {
        Some(r) => r,
        None => {
            let r = ctx.chpx_bins.chpx_run(ctx.word, fc);
            st.chpx = Some(r);
            r
        }
    };
    let grpprl = run.grpprl;
    let prm = span.map_or(0, |s| s.prm());
    // The paragraph's style comes with its mark, after its characters: it is looked up ahead,
    // at the paragraph properties of the first character's FC (the PAPX run that holds it ends
    // at its paragraph's mark).
    if pb.style_ahead.is_none() {
        pb.style_ahead = Some(style_at(ctx, fc));
    }
    let style = pb.style_ahead.as_ref().and_then(Option::as_deref);
    let hit = st.memo.as_ref().is_some_and(|m| std::ptr::eq(m.chpx, grpprl) && m.prm == prm && m.style.as_deref() == style);
    if !hit {
        // The run keeps only its own formatting (and its character style's); the paragraph
        // style's properties are what its toggles invert ([MS-DOC] sprmCFBold: 0x81 is the
        // opposite of the style's value). A character style applies over the paragraph style's
        // properties, and the run's toggles then invert what the two give together ([MS-DOC]
        // sprmCIstd).
        let mut start = CharProps::default();
        let mut toggle_base = style.and_then(|id| ctx.sheet.get(id)).map(|sty| sty.chr.clone()).unwrap_or_default();
        if sprm::iter(grpprl).any(|p| p.op == 0x4A30) {
            fmt::apply_char_style(&mut start, grpprl, ctx.raw_styles, ctx.fonts);
            fmt::apply_char_style(&mut toggle_base, grpprl, ctx.raw_styles, ctx.fonts);
        }
        let mut props = char_props(grpprl, ctx.fonts, start, &toggle_base);
        let piece_prm = ctx.pieces.piece_prm(cp);
        apply_prm(&mut props, piece_prm, ctx.fonts, &toggle_base, ctx.work);
        ctx.revs.mark(&mut props, grpprl, piece_prm);
        st.memo = Some(FmtMemo { chpx: grpprl, prm, style: style.map(str::to_owned), props });
        st.generation += 1;
    }
    let Some(m) = st.memo.as_ref() else { return };
    // A symbol character (Insert › Symbol) is stored as the placeholder U+0028 with
    // sprmCSymbol carrying the font and the character code ([MS-DOC] §2.6.1).
    let symbol = if c == '(' { symbol_operand(grpprl) } else { None };
    if field.is_none() && symbol.is_none() {
        pb.push_shared(c, &m.props, st.generation);
        return;
    }
    let mut props = m.props.clone();
    let mut c = c;
    if let Some((ftc, xchar)) = symbol {
        if let Some(font) = ctx.fonts.get(usize::from(ftc)) {
            props.font = Some(font.clone());
        }
        if let Some(sym) = char::from_u32(u32::from(xchar)) {
            c = sym;
        }
    }
    match field {
        Some(f) => match f.sep.as_mut() {
            // Inside a field, characters are recorded for the field object instead of shown;
            // `finish_field` emits the visible text (hyperlink runs or one object anchor).
            Some(result) => result.push(c, props),
            None => f.instr.push(c),
        },
        None => pb.push(c, props),
    }
}

/// What the walk carries from one character to the next while it resolves formatting: the piece
/// and the CHPX run the last character was in (so most characters need no search), and the last
/// formatting resolved.
#[derive(Default)]
struct FmtState<'a> {
    span: Option<piece::PieceSpan<'a>>,
    chpx: Option<fkp::ChpxRun<'a>>,
    memo: Option<FmtMemo<'a>>,
    /// Counts the memo's replacements, so equal values mean the same properties.
    generation: u64,
}

/// The last character formatting resolved, reused while the CHPX, the piece's Prm and the
/// paragraph style stay the same, so a Prm grpprl (up to 64 KB) is interpreted once per
/// formatting change, not once per character (#3692).
struct FmtMemo<'a> {
    /// The CHPX grpprl, compared by address: a slice of the WordDocument stream, or of a Word 6.0/95
    /// file's translated runs, both of which stay put for the walk.
    chpx: &'a [u8],
    prm: u16,
    style: Option<String>,
    props: CharProps,
}

/// The paragraph and table sprms (sgc 1 and 5) of a piece's Prm, as Prls to append to a
/// paragraph's PAPX: a Prm0's single sprm, or those of the Prm1's grpprl. A Prm1 grpprl is
/// charged to `work`, as for [`apply_prm`].
fn prm_paragraph_sprms(prm: PrmRef<'_>, work: &Work) -> Vec<u8> {
    let mut out = Vec::new();
    match prm {
        PrmRef::Sprm0 { isprm, val } => {
            if let Some(op) = sprm::from_prm0(isprm)
                && matches!(Prl { op, operand: &[] }.sgc(), 1 | 5)
            {
                out.extend_from_slice(&op.to_le_bytes());
                out.push(val);
            }
        }
        PrmRef::Grpprl { grpprl: Some(g), .. } => {
            if !work.charge(g.len() as u64, Cost::Formatting) {
                return out;
            }
            for prl in sprm::iter(g).filter(|p| matches!(p.sgc(), 1 | 5)) {
                out.extend_from_slice(&prl.op.to_le_bytes());
                out.extend_from_slice(prl.operand);
            }
        }
        PrmRef::None | PrmRef::Grpprl { grpprl: None, .. } => {}
    }
    out
}

/// `sprmPHugePapx` (0x6646) in a PAPX: the paragraph's sprms read from the PrcData at its
/// operand's offset in the Data stream (`cbGrpprl`, at most 0x3FA2, then the grpprl), with the
/// PAPX's own other sprms before them ([MS-DOC] sprmPHugePapx, PrcData). None without the sprm, or
/// when the PrcData lies outside the stream. The copy is charged to `work` ([`MAX_WORK`]), as a
/// hostile file can point every paragraph at the same 16 KB.
fn huge_papx(papx: &[u8], data: &[u8], work: &Work) -> Option<Vec<u8>> {
    const P_HUGE_PAPX: u16 = 0x6646;
    let at = sprm::iter(papx).find(|p| p.op == P_HUGE_PAPX).and_then(|p| p.operand.get(..4))?;
    let at = u32::from_le_bytes([at[0], at[1], at[2], at[3]]) as usize;
    let cb = data.get(at..at.checked_add(2)?).map(|b| u16::from_le_bytes([b[0], b[1]]))?;
    let grpprl = data.get(at + 2..(at + 2).checked_add(usize::from(cb).min(0x3FA2))?)?;
    if !work.charge(grpprl.len() as u64, Cost::Formatting) {
        return None;
    }
    let mut out: Vec<u8> = sprm::iter(papx)
        .filter(|p| p.op != P_HUGE_PAPX)
        .flat_map(|p| p.op.to_le_bytes().into_iter().chain(p.operand.iter().copied()))
        .collect();
    out.extend_from_slice(grpprl);
    Some(out)
}

/// Apply a piece's Prm (Prm0 short sprm or Prm1 grpprl) to character properties. A Prm1
/// grpprl is charged to `work`; once that is spent the grpprl is skipped.
fn apply_prm(props: &mut CharProps, prm: PrmRef<'_>, fonts: &[String], base: &CharProps, work: &Work) {
    match prm {
        PrmRef::None => {}
        PrmRef::Sprm0 { isprm, val } => {
            if let Some(op) = sprm::from_prm0(isprm) {
                let operand = [val];
                let prl = Prl { op, operand: &operand };
                if prl.sgc() == 2 {
                    fmt::apply_char(props, &prl, base, fonts);
                }
            }
        }
        PrmRef::Grpprl { grpprl: Some(g), .. } => {
            if !work.charge(g.len() as u64, Cost::Formatting) {
                return;
            }
            for prl in sprm::iter(g) {
                if prl.sgc() == 2 {
                    fmt::apply_char(props, &prl, base, fonts);
                }
            }
        }
        PrmRef::Grpprl { grpprl: None, .. } => {}
    }
}

/// The style id of the paragraph whose properties hold `fc` (its PAPX's istd, or the sprmPIstd
/// in it).
fn style_at(ctx: &WalkCtx<'_>, fc: u32) -> Option<String> {
    let (istd, papx) = ctx.papx_bins.papx(ctx.word, fc);
    let istd = sprm::iter(papx)
        .filter(|p| p.op == P_ISTD)
        .filter_map(|p| p.operand.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]])))
        .last()
        .unwrap_or(istd);
    ctx.raw_styles.get(istd as usize).filter(|s| !s.name.is_empty()).map(|st| fmt::style_id(&st.name))
}

/// Character props from a CHPX grpprl applied over `start`, its toggles inverting `toggle_base`
/// (the paragraph style's values, with the run's character style over them).
fn char_props(grpprl: &[u8], fonts: &[String], start: CharProps, toggle_base: &CharProps) -> CharProps {
    let mut p = start;
    for prl in sprm::iter(grpprl) {
        if prl.sgc() == 2 {
            fmt::apply_char(&mut p, &prl, toggle_base, fonts);
        }
    }
    p
}

/// Read a root-level stream fully; `None` if it is absent or cannot be read. A stream larger than
/// [`MAX_STREAM`] is [`DocbinError::Limit`], and nothing is allocated for it.
fn stream(comp: &craft_cfb::CompoundFile<'_>, name: &str) -> Result<Option<Vec<u8>>, DocbinError> {
    match comp.read_path_capped(name, MAX_STREAM) {
        Ok(v) => Ok(Some(v)),
        Err(craft_cfb::Error::LimitExceeded(e)) => Err(DocbinError::Limit(format!("{name} stream: {e}"))),
        // A stream the directory tree no longer reaches (a damaged sibling link) is still an
        // entry of the directory: taken by its name.
        // A damaged entry or chain gives what it still holds ([`craft_cfb::CompoundFile::read_salvaged`]).
        Err(_) => {
            let id = comp.find(name).ok().or_else(|| {
                comp.entries.iter().position(|e| e.kind == craft_cfb::EntryKind::Stream && e.parent.is_none() && e.name.eq_ignore_ascii_case(name))
            });
            match id.map(|id| comp.read_salvaged(id, MAX_STREAM)) {
                Some(Ok(v)) if !v.is_empty() => Ok(Some(v)),
                Some(Err(craft_cfb::Error::LimitExceeded(e))) => Err(DocbinError::Limit(format!("{name} stream: {e}"))),
                _ => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests;
