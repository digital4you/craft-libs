//! What sets an Excel 5.0/95 Workbook stream (BIFF5, and BIFF7, which shares its version
//! number) apart from BIFF8, from Microsoft's Excel binary file format documentation and the
//! OpenOffice.org documentation of the Excel file format: every string is a byte string in the
//! workbook's code page (the CodePage record), the XF record has an older 16-byte layout, and
//! parsed formulas use shorter cell references and longer 3-D and name tokens.
//!
//! The reader keeps one model: an XF is rewritten into the BIFF8 layout and a formula into BIFF8
//! tokens, which the BIFF8 code then decodes as usual.

use crate::formula::{Mode, Unsupported, Xti};

/// Formula token bytes rewritten at most (the BIFF8 decoder's own cap).
const MAX_RGCE: usize = 16 * 1024;
/// Sheets up to which every span of sheets (`Sheet1:Sheet3!A1`) gets its own link; past it a
/// 3-D reference across sheets keeps the cell's saved result.
const MAX_SPAN_SHEETS: usize = 64;

/// A workbook's code page (the CodePage record), for its byte strings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CodePage(u16);

impl Default for CodePage {
    fn default() -> Self {
        CodePage(1252)
    }
}

impl CodePage {
    /// The code page a CodePage record names: 32768 is Mac OS Roman, 32769 Windows-1252; 367
    /// (ASCII), 1200 (UTF-16, which some writers put in a BIFF5 file whose strings are still
    /// 8-bit) and pages that are not read decode as Windows-1252.
    pub(crate) fn new(cv: u16) -> Self {
        match cv {
            32768 => CodePage(craft_codepage::MAC_ROMAN_CODE_PAGE),
            cv if craft_codepage::is_text_code_page(cv) => CodePage(cv),
            _ => CodePage::default(),
        }
    }

    pub(crate) fn decode(self, bytes: &[u8]) -> String {
        if let Some(enc) = craft_codepage::multibyte_encoding_name(self.0).and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
            return enc.decode_without_bom_handling(bytes).0.into_owned();
        }
        bytes.iter().map(|&b| craft_codepage::code_page_byte(self.0, b)).collect()
    }
}

/// A BIFF5 XF record ([MS-XLS] for the fields, the older layout from the format documentation)
/// in BIFF8's: font, format and protection words, then the 14 CellXF bytes. `None` when the
/// record is shorter than the 16 bytes it should have.
pub(crate) fn xf(rec: &[u8]) -> Option<[u8; 20]> {
    let b = |i: usize| rec.get(i).copied().unwrap_or(0);
    let w = |i: usize| u32::from_le_bytes([b(i), b(i + 1), b(i + 2), b(i + 3)]);
    if rec.len() < 16 {
        return None;
    }
    let mut out = [0u8; 20];
    out[..6].copy_from_slice(rec.get(..6)?);
    // Alignment: horizontal, wrap, vertical and justify-last share BIFF8's bit positions.
    out[6] = b(6);
    // Orientation: none, stacked letters, 90° counter-clockwise, 90° clockwise.
    out[7] = match b(7) & 0x03 {
        1 => 255,
        2 => 90,
        3 => 180,
        _ => 0,
    };
    let fill = w(8);
    let lines = w(12);
    let (fg, bg, fls) = (fill & 0x7F, (fill >> 7) & 0x7F, (fill >> 16) & 0x3F);
    let (bottom, bottom_c) = ((fill >> 22) & 0x07, (fill >> 25) & 0x7F);
    let (top, left, right) = (lines & 0x07, (lines >> 3) & 0x07, (lines >> 6) & 0x07);
    let (top_c, left_c, right_c) = ((lines >> 9) & 0x7F, (lines >> 16) & 0x7F, (lines >> 23) & 0x7F);
    let b1 = left | (right << 4) | (top << 8) | (bottom << 12) | (left_c << 16) | (right_c << 23);
    let b2 = top_c | (bottom_c << 7) | (fls << 26);
    out[10..14].copy_from_slice(&b1.to_le_bytes());
    out[14..18].copy_from_slice(&b2.to_le_bytes());
    out[18..20].copy_from_slice(&((fg | (bg << 7)) as u16).to_le_bytes());
    Some(out)
}

/// What a BIFF5 formula is rewritten against: the workbook's sheet count, its code page, and its
/// EXTERNSHEET records, whose EXTERNNAME records name add-in functions.
#[derive(Clone, Copy)]
pub(crate) struct Context {
    pub sheets: usize,
    pub cp: CodePage,
    /// EXTERNSHEET records; the link of the first is `links(sheets).len()`, workbook 1.
    pub externs: usize,
}

/// The link table a BIFF5 workbook's 3-D references are rewritten against: one internal
/// workbook, one entry per sheet, then one per span of sheets for workbooks of up to
/// [`MAX_SPAN_SHEETS`] sheets, then a deleted sheet. The caller appends one entry per
/// EXTERNSHEET record (workbooks 1 and up).
pub(crate) fn links(sheets: usize) -> Vec<Xti> {
    let mut xti = Vec::new();
    for i in 0..sheets {
        let i = i16::try_from(i).unwrap_or(i16::MAX);
        xti.push(Xti { book: 0, first: i, last: i });
    }
    if sheets <= MAX_SPAN_SHEETS {
        for a in 0..sheets {
            for b in 0..sheets {
                let (a, b) = (i16::try_from(a).unwrap_or(i16::MAX), i16::try_from(b).unwrap_or(i16::MAX));
                xti.push(Xti { book: 0, first: a, last: b });
            }
        }
    }
    xti.push(Xti { book: 0, first: -1, last: -1 });
    xti
}

/// The index into [`links`] of a reference to sheets `first..=last`.
fn link(first: u16, last: u16, sheets: usize) -> Result<u16, Unsupported> {
    let (f, l) = (usize::from(first), usize::from(last));
    let deleted = sheets + if sheets <= MAX_SPAN_SHEETS { sheets * sheets } else { 0 };
    let i = if f >= sheets || l >= sheets {
        deleted
    } else if f == l {
        f
    } else if sheets <= MAX_SPAN_SHEETS {
        sheets + f * sheets + l
    } else {
        return Err(Unsupported("3-D references across sheets in a workbook with many sheets"));
    };
    u16::try_from(i).map_err(|_| Unsupported("3-D references across sheets in a workbook with many sheets"))
}

const TRUNC: Unsupported = Unsupported("truncated formula");

struct In<'a> {
    b: &'a [u8],
    pos: usize,
}

impl In<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Unsupported> {
        let s = self.b.get(self.pos..self.pos.checked_add(n).ok_or(TRUNC)?).ok_or(TRUNC)?;
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Unsupported> {
        self.take(1)?.first().copied().ok_or(TRUNC)
    }
    fn u16(&mut self) -> Result<u16, Unsupported> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([*s.first().ok_or(TRUNC)?, *s.get(1).ok_or(TRUNC)?]))
    }
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A BIFF5 row field (14-bit row, column- and row-relative flags in the top bits) and 8-bit
/// column as BIFF8's 16-bit row and column field. A relative row is a signed 14-bit offset.
fn cell(row: u16, col: u8, relative: bool) -> (u16, u16) {
    let (col_rel, row_rel) = (row & 0x4000 != 0, row & 0x8000 != 0);
    let r = row & 0x3FFF;
    let r = if relative && row_rel && r & 0x2000 != 0 { r | 0xC000 } else { r };
    let c = u16::from(col) | if col_rel { 0x4000 } else { 0 } | if row_rel { 0x8000 } else { 0 };
    (r, c)
}

/// A string in BIFF8's form: character count (one byte, or two with `wide_count`), UTF-16 flag,
/// characters, cut at what the count field holds.
fn text(out: &mut Vec<u8>, s: &str, wide_count: bool) {
    let units: Vec<u16> = s.encode_utf16().take(if wide_count { usize::from(u16::MAX) } else { 255 }).collect();
    if wide_count {
        put16(out, u16::try_from(units.len()).unwrap_or(u16::MAX));
    } else {
        out.push(u8::try_from(units.len()).unwrap_or(u8::MAX));
    }
    out.push(1);
    for u in units {
        put16(out, u);
    }
}

/// Rewrites a BIFF5 parsed formula (tokens and extra data) as BIFF8 tokens and extra data, for
/// the BIFF8 decoder, which reads them in `mode`: the relative parts of shared-formula
/// references, and of a name's 3-D references, are signed offsets. Tokens that refer outside the
/// workbook are refused as the BIFF8 decoder refuses them.
pub(crate) fn formula(rgce: &[u8], rgcb: &[u8], ctx: Context, mode: Mode) -> Result<(Vec<u8>, Vec<u8>), Unsupported> {
    let Context { sheets, cp, externs } = ctx;
    if rgce.len() > MAX_RGCE {
        return Err(Unsupported("formula too long"));
    }
    let mut t = In { b: rgce, pos: 0 };
    let mut x = In { b: rgcb, pos: 0 };
    let mut out = Vec::with_capacity(rgce.len() * 2);
    let mut extra = Vec::with_capacity(rgcb.len() * 2);
    while t.pos < rgce.len() {
        let ptg = t.u8()?;
        out.push(ptg);
        let id = if ptg & 0x60 != 0 { 0x20 | (ptg & 0x1F) } else { ptg };
        match id {
            // tExp and tTbl: row and column words, as in BIFF8.
            0x01 | 0x02 => out.extend_from_slice(t.take(4)?),
            0x03..=0x16 => {}
            0x17 => {
                let n = usize::from(t.u8()?);
                let s = cp.decode(t.take(n)?);
                text(&mut out, &s, false);
            }
            0x19 => {
                let kind = t.u8()?;
                out.push(kind);
                let data = t.u16()?;
                put16(&mut out, data);
                if kind & 0x04 != 0 {
                    out.extend_from_slice(t.take((usize::from(data) + 1) * 2)?);
                }
            }
            0x1C | 0x1D => out.push(t.u8()?),
            0x1E => out.extend_from_slice(t.take(2)?),
            0x1F => out.extend_from_slice(t.take(8)?),
            0x20 => {
                out.extend_from_slice(t.take(7)?);
                array(&mut x, &mut extra, cp)?;
            }
            0x21 => out.extend_from_slice(t.take(2)?),
            0x22 => out.extend_from_slice(t.take(3)?),
            // tName: the name's index, then 12 unused bytes (BIFF8: 2).
            0x23 => {
                let i = t.u16()?;
                t.take(12)?;
                put16(&mut out, i);
                put16(&mut out, 0);
            }
            0x24 | 0x2C | 0x2A => {
                let (r, c) = (t.u16()?, t.u8()?);
                let (r, c) = cell(r, c, id == 0x2C);
                put16(&mut out, r);
                put16(&mut out, c);
            }
            0x25 | 0x2D | 0x2B => {
                let (r0, r1, c0, c1) = (t.u16()?, t.u16()?, t.u8()?, t.u8()?);
                let rel = id == 0x2D;
                let ((r0, c0), (r1, c1)) = (cell(r0, c0, rel), cell(r1, c1, rel));
                for v in [r0, r1, c0, c1] {
                    put16(&mut out, v);
                }
            }
            // tMemArea: reserved and size, then its ranges in the extra data.
            0x26 => {
                out.extend_from_slice(t.take(6)?);
                let n = x.u16()?;
                put16(&mut extra, n);
                for _ in 0..n {
                    let (r0, r1, c0, c1) = (x.u16()?, x.u16()?, x.u8()?, x.u8()?);
                    for v in [r0, r1, u16::from(c0), u16::from(c1)] {
                        put16(&mut extra, v);
                    }
                }
            }
            0x27 | 0x28 => out.extend_from_slice(t.take(6)?),
            0x29 | 0x2E | 0x2F => out.extend_from_slice(t.take(2)?),
            // tNameX: the EXTERNSHEET record (one-based), 8 unused bytes, the EXTERNNAME (one-based),
            // 12 unused bytes; BIFF8: the link and the name.
            0x39 => {
                let ixals = usize::from((t.u16()? as i16).unsigned_abs());
                t.take(8)?;
                let name = t.u16()?;
                t.take(12)?;
                if ixals == 0 || ixals > externs {
                    return Err(Unsupported("reference to a missing name link"));
                }
                let base = links(sheets).len();
                put16(&mut out, u16::try_from(base + ixals - 1).map_err(|_| Unsupported("reference to a missing name link"))?);
                put16(&mut out, name);
                put16(&mut out, 0);
            }
            // 3-D references: the EXTERNSHEET index (negative for this workbook), 8 unused bytes,
            // the first and last sheet, then the cell or area.
            0x3A..=0x3D => {
                let ixals = t.u16()? as i16;
                t.take(8)?;
                let (first, last) = (t.u16()?, t.u16()?);
                if ixals >= 0 {
                    return Err(Unsupported("references to other workbooks (external links)"));
                }
                put16(&mut out, link(first, last, sheets)?);
                if id == 0x3A || id == 0x3C {
                    let (r, c) = (t.u16()?, t.u8()?);
                    let (r, c) = cell(r, c, mode == Mode::Name);
                    put16(&mut out, r);
                    put16(&mut out, c);
                } else {
                    let (r0, r1, c0, c1) = (t.u16()?, t.u16()?, t.u8()?, t.u8()?);
                    let rel = mode == Mode::Name;
                    let ((r0, c0), (r1, c1)) = (cell(r0, c0, rel), cell(r1, c1, rel));
                    for v in [r0, r1, c0, c1] {
                        put16(&mut out, v);
                    }
                }
            }
            _ => return Err(Unsupported("unknown formula token")),
        }
    }
    Ok((out, extra))
}

/// An array constant's values: BIFF5 counts columns (0 for 256) and rows, BIFF8 stores both
/// less one; strings are byte strings.
fn array(x: &mut In<'_>, extra: &mut Vec<u8>, cp: CodePage) -> Result<(), Unsupported> {
    let cols = x.u8()?.wrapping_sub(1);
    let rows = x.u16()?.checked_sub(1).ok_or(Unsupported("empty array constant"))?;
    extra.push(cols);
    put16(extra, rows);
    let n = (usize::from(cols) + 1).saturating_mul(usize::from(rows) + 1);
    if n > 65_536 {
        return Err(Unsupported("array constant too large"));
    }
    for _ in 0..n {
        let kind = x.u8()?;
        extra.push(kind);
        match kind {
            0x02 => {
                let len = usize::from(x.u8()?);
                let s = cp.decode(x.take(len)?);
                text(extra, &s, true);
            }
            0x00 | 0x01 | 0x04 | 0x10 => extra.extend_from_slice(x.take(8)?),
            _ => return Err(Unsupported("unknown array constant value")),
        }
    }
    Ok(())
}
