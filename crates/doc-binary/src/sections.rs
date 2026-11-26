//! Sections and headers/footers: `PlcfSed` → `Sed` → `Sepx` (grpprls of section sprms) and
//! the `Plcfhdd` stories of the header subdocument ([MS-DOC] §2.8.25, §2.9.244).

use craft_wordmodel::props::Borders;
use craft_wordmodel::section::{Columns, LineNumberRestart, LineNumbering, SectionProps, SectionStart};

use crate::fib::{Fib, pair};
use crate::sprm::{self, Prl};

/// Most sections we read.
const MAX_SECTIONS: usize = 1024;

// Section sprms ([MS-DOC] §2.6.4).
const S_BKC: u16 = 0x3009;
const S_TITLE_PAGE: u16 = 0x300A; // sprmSFTitlePage
const S_CCOLUMNS: u16 = 0x500B;
const S_NFC_PGN: u16 = 0x300E; // sprmSNfcPgn: page number format
const S_PGN_RESTART: u16 = 0x3011; // sprmSFPgnRestart
const S_PGN_START: u16 = 0x501C; // sprmSPgnStart97
const S_FEVENLY_SPACED: u16 = 0x3005; // sprmSFEvenlySpaced
const S_DXA_COLUMNS: u16 = 0x900C; // sprmSDxaColumns: space between equal columns
const S_DXA_COL_WIDTH: u16 = 0xF203; // sprmSDxaColWidth: column index, width
const S_DXA_COL_SPACING: u16 = 0xF204; // sprmSDxaColSpacing: column index, space after it
const S_LBETWEEN: u16 = 0x3019; // sprmSLBetween: line between columns
const S_LNC: u16 = 0x3013; // sprmSLnc
const S_NLNN_MOD: u16 = 0x5015; // sprmSNLnnMod
const S_DXA_LNN: u16 = 0x9016; // sprmSDxaLnn
const S_LNN_MIN: u16 = 0x501B; // sprmSLnnMin
const S_PGB_PROP: u16 = 0x522F; // sprmSPgbProp: which pages, depth, measured from
const S_BRC_TOP80: u16 = 0x702B;
const S_BRC_LEFT80: u16 = 0x702C;
const S_BRC_BOTTOM80: u16 = 0x702D;
const S_BRC_RIGHT80: u16 = 0x702E;
const S_BRC_TOP: u16 = 0xD234;
const S_BRC_LEFT: u16 = 0xD235;
const S_BRC_BOTTOM: u16 = 0xD236;
const S_BRC_RIGHT: u16 = 0xD237;
const S_ORIENTATION: u16 = 0x301D; // sprmSBOrientation
const S_HDR_TOP: u16 = 0xB017;
const S_HDR_BOTTOM: u16 = 0xB018;
const S_XA_PAGE: u16 = 0xB01F;
const S_YA_PAGE: u16 = 0xB020;
const S_DXA_LEFT: u16 = 0xB021;
const S_DXA_RIGHT: u16 = 0xB022;
const S_DYA_TOP: u16 = 0x9023;
const S_DYA_BOTTOM: u16 = 0x9024;
const S_DXA_GUTTER: u16 = 0xB025;

/// One section: covers paragraphs up to (excluding) `end_cp`, with its properties.
pub(crate) struct Section {
    pub(crate) end_cp: u32,
    pub(crate) props: SectionProps,
}

/// Parse `PlcfSed` and each section's `Sepx` grpprl.
pub(crate) fn parse(word: &[u8], table: &[u8], fib: &Fib) -> Vec<Section> {
    let Some((fc, lcb)) = fib.pair(pair::PLCF_SED) else { return Vec::new() };
    let Some(plc) = table.get(fc as usize..(fc as usize).saturating_add(lcb as usize)) else { return Vec::new() };
    // PlcSed: aCp[n+1] u32s + aSed[n] (Sed = fn 2, fcSepx 4, fnMpr 2, fcMpr 4).
    if plc.len() < 4 || (plc.len() - 4) % 16 != 0 {
        return Vec::new();
    }
    let n = (plc.len() - 4) / 16;
    let cp_at = |i: usize| -> Option<u32> { plc.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) };
    let mut out = Vec::with_capacity(n.min(MAX_SECTIONS));
    for i in 0..n.min(MAX_SECTIONS) {
        let Some(end_cp) = cp_at(i + 1) else { break };
        let sed_at = (n + 1) * 4 + i * 12;
        let fc_sepx = plc.get(sed_at + 2..sed_at + 6).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0xFFFF_FFFF);
        // Sepx: cb u16, then a grpprl of section sprms.
        let props = from_grpprl(sepx(word, fc_sepx).unwrap_or_default());
        out.push(Section { end_cp, props });
    }
    out
}

/// The grpprl of the Sepx at `fc_sepx` in the WordDocument stream: cb u16, then cb bytes.
pub(crate) fn sepx(word: &[u8], fc_sepx: u32) -> Option<&[u8]> {
    let cb = word.get(fc_sepx as usize..)?.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)?;
    let at = (fc_sepx as usize).checked_add(2)?;
    word.get(at..at.checked_add(cb)?)
}

/// A section's properties from its (Word 97) section sprms.
pub(crate) fn from_grpprl(grpprl: &[u8]) -> SectionProps {
    let mut props = SectionProps::default();
    let mut explicit_orient = false;
    // Line numbering: sprmSNLnnMod turns it on (a count-by above 0), whatever order the
    // restart (sprmSLnc), distance (sprmSDxaLnn) and first number (sprmSLnnMin) come in.
    let mut lines = LineNumbering::default();
    let mut count_by = 0u16;
    // Columns: the count, the space between equal columns, the line between them, and the
    // width and space after of each unequal one, collected whatever their order.
    let mut evenly = true;
    let mut col_space = None;
    let mut separator = false;
    let mut cols: std::collections::BTreeMap<u8, (Option<f32>, Option<f32>)> = std::collections::BTreeMap::new();
    // Page borders measured from the text (pgbOffsetFrom 0, Word 97's default) rather than from
    // the page edge, and whether they are on every page (pgbApplyTo 0).
    let mut borders_from_text = true;
    let mut borders_every_page = true;
    // Page numbering restarts (sprmSFPgnRestart) at sprmSPgnStart97, 1 when it isn't given,
    // whatever order they come in.
    let mut pgn_restart = false;
    let mut pgn_start = None;
    for prl in sprm::iter(grpprl) {
        // Only a defined operand counts as explicit; anything else leaves the
        // orientation to the page shape.
        if prl.op == S_ORIENTATION && matches!(prl.operand.first(), Some(1 | 2)) {
            explicit_orient = true;
        }
        let u16_op = || prl.operand.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        match prl.op {
            S_LNC => {
                lines.restart = match prl.operand.first() {
                    Some(1) => LineNumberRestart::Section,
                    Some(2) => LineNumberRestart::Continuous,
                    _ => LineNumberRestart::Page,
                }
            }
            S_NLNN_MOD => count_by = u16_op().unwrap_or(0),
            // Twips; 0 is Word's automatic distance.
            S_DXA_LNN => lines.distance = u16_op().map_or(0.0, |v| (f32::from(v as i16) / 20.0).clamp(0.0, 1584.0)),
            // The number of the first line is lnnMin + 1.
            S_LNN_MIN => lines.start = u32::from(u16_op().unwrap_or(0).min(32_767)) + 1,
            S_PGB_PROP => {
                let v = u16_op().unwrap_or(0);
                borders_from_text = (v >> 5) & 7 == 0;
                borders_every_page = v & 7 == 0;
            }
            S_PGN_RESTART => pgn_restart = prl.operand.first().is_some_and(|&v| v != 0),
            S_PGN_START => pgn_start = u16_op().map(|v| u32::from(v.min(32_767))),
            S_FEVENLY_SPACED => evenly = prl.operand.first().is_none_or(|&v| v != 0),
            S_DXA_COLUMNS => col_space = u16_op().map(|v| (f32::from(v as i16) / 20.0).clamp(0.0, 1584.0)),
            S_LBETWEEN => separator = prl.operand.first().is_some_and(|&v| v != 0),
            S_DXA_COL_WIDTH | S_DXA_COL_SPACING => {
                if let [i, lo, hi, ..] = prl.operand
                    && cols.len() < 45
                {
                    let v = (f32::from(i16::from_le_bytes([*lo, *hi])) / 20.0).clamp(0.0, 1584.0);
                    let e = cols.entry(*i).or_default();
                    if prl.op == S_DXA_COL_WIDTH {
                        e.0 = Some(v);
                    } else {
                        e.1 = Some(v);
                    }
                }
            }
            _ => apply_section(&mut props, &prl),
        }
    }
    if pgn_restart {
        props.page_num_start = Some(pgn_start.unwrap_or(1));
    }
    // The model has page borders on every page of a section or none: borders on the first page
    // only, or on all pages but the first, are left out rather than drawn on every page.
    if !borders_every_page {
        props.page_borders = None;
    }
    // The model measures page borders from the page edge: one measured from the text is as far
    // from the edge as the margin leaves outside it.
    if borders_from_text && let Some(b) = props.page_borders.as_mut() {
        for (side, margin) in [
            (&mut b.top, props.margin_top),
            (&mut b.left, props.margin_left),
            (&mut b.bottom, props.margin_bottom),
            (&mut b.right, props.margin_right),
        ] {
            if let Some(side) = side.as_mut() {
                side.space = (margin - side.space - side.width).max(0.0);
            }
        }
    }
    if let Some(space) = col_space {
        props.columns.space = space;
    }
    props.columns.separator = separator;
    // Unequal columns need a width for every one of them.
    let count = props.columns.count;
    if !evenly && count > 1 && (0..count).all(|i| u8::try_from(i).ok().and_then(|i| cols.get(&i)).is_some_and(|c| c.0.is_some())) {
        props.columns.widths = (0..count)
            .filter_map(|i| u8::try_from(i).ok().and_then(|i| cols.get(&i)))
            .map(|&(w, sp)| (w.unwrap_or(0.0), sp.unwrap_or(0.0)))
            .collect();
    }
    if count_by > 0 {
        lines.count_by = u32::from(count_by.min(100));
        props.line_numbers = Some(lines);
    }
    // Without an explicit orientation sprm, infer it from the page shape.
    if !explicit_orient {
        if props.page_w < props.page_h {
            props.landscape = false;
        } else if props.page_w > props.page_h {
            props.landscape = true;
        }
    }
    props
}

/// Apply a section sprm to the model's section properties. All the measurement sprms here
/// carry 16-bit twip operands ([MS-DOC] §2.6.4).
fn apply_section(s: &mut SectionProps, prl: &Prl) {
    let tw = |v: i32| (v as f32 / 20.0).clamp(0.0, 1584.0);
    let s16 = |prl: &Prl| -> Option<i16> {
        match prl.operand {
            [b0, b1, ..] => Some(i16::from_le_bytes([*b0, *b1])),
            _ => None,
        }
    };
    match prl.op {
        S_BKC => {
            s.start = match prl.operand.first().copied().unwrap_or(2) {
                // SBkcOperand ([MS-DOC] sprmSBkc): bkcContinuous, bkcNewColumn, bkcNewPage,
                // bkcEvenPage, bkcOddPage; anything else falls back to the default (new page).
                0 => SectionStart::Continuous,
                1 => SectionStart::NextColumn,
                3 => SectionStart::EvenPage,
                4 => SectionStart::OddPage,
                _ => SectionStart::NextPage,
            };
        }
        S_TITLE_PAGE => s.title_page = prl.operand.first().is_some_and(|&v| v != 0),
        S_CCOLUMNS => {
            if let Some(c) = s16(prl) {
                s.columns = Columns { count: (c as u32).saturating_add(1).min(44), ..Default::default() };
            }
        }
        S_NFC_PGN => {
            if let Some(&nfc) = prl.operand.first() {
                s.page_num_format = crate::list::num_format(nfc);
            }
        }
        // SBOrientationOperand ([MS-DOC] §2.9.236): 1 portrait, 2 landscape.
        S_ORIENTATION => match prl.operand.first() {
            Some(1) => s.landscape = false,
            Some(2) => s.landscape = true,
            _ => {}
        },
        S_HDR_TOP => {
            if let Some(v) = s16(prl) {
                s.header = tw(v as i32);
            }
        }
        S_HDR_BOTTOM => {
            if let Some(v) = s16(prl) {
                s.footer = tw(v as i32);
            }
        }
        S_XA_PAGE => {
            if let Some(v) = s16(prl) {
                s.page_w = tw(v as i32).max(7.2);
            }
        }
        S_YA_PAGE => {
            if let Some(v) = s16(prl) {
                s.page_h = tw(v as i32).max(7.2);
            }
        }
        S_DXA_LEFT => {
            if let Some(v) = s16(prl) {
                s.margin_left = tw(v as i32);
            }
        }
        S_DXA_RIGHT => {
            if let Some(v) = s16(prl) {
                s.margin_right = tw(v as i32);
            }
        }
        // A negative top or bottom margin is that margin, kept even when a header or footer would
        // push the text away ([MS-DOC] sprmSDyaTop, sprmSDyaBottom); the layout uses its size, as
        // the DOCX reader does for w:pgMar.
        S_DYA_TOP => {
            if let Some(v) = s16(prl) {
                s.margin_top = tw((v as i32).abs());
            }
        }
        S_DYA_BOTTOM => {
            if let Some(v) = s16(prl) {
                s.margin_bottom = tw((v as i32).abs());
            }
        }
        S_DXA_GUTTER => {
            if let Some(v) = s16(prl) {
                s.gutter = tw(v as i32);
            }
        }
        S_BRC_TOP80 | S_BRC_LEFT80 | S_BRC_BOTTOM80 | S_BRC_RIGHT80 | S_BRC_TOP | S_BRC_LEFT | S_BRC_BOTTOM | S_BRC_RIGHT => {
            let border = if prl.op & 0xF000 == 0x7000 { crate::fmt::brc80(prl.operand) } else { crate::fmt::brc(prl.operand) };
            if let Some(b) = border {
                let borders = s.page_borders.get_or_insert_with(Borders::default);
                let slot = match prl.op {
                    S_BRC_TOP80 | S_BRC_TOP => &mut borders.top,
                    S_BRC_LEFT80 | S_BRC_LEFT => &mut borders.left,
                    S_BRC_BOTTOM80 | S_BRC_BOTTOM => &mut borders.bottom,
                    _ => &mut borders.right,
                };
                *slot = Some(b);
            }
        }
        _ => {}
    }
}

/// The header stories' CP ranges from `Plcfhdd` (a PLC of bare CPs): story i spans
/// `[cps[i], cps[i+1])`. Stories 0-5 are footnote/endnote separators; every section then owns
/// six stories in the order even header, odd (default) header, even footer, odd (default)
/// footer, first header, first footer. The CPs are relative to the header subdocument.
///
/// The CPs of a well-formed PLC ascend, so the stories are disjoint. A story starting before
/// the end of an earlier one (a hostile table pointing many sections at the same text, each
/// read into its own part) is left empty, like a slot linked to the previous section's:
/// every subdocument CP becomes at most one story.
pub(crate) fn header_stories(table: &[u8], fib: &Fib) -> Vec<(u32, u32)> {
    let Some((fc, lcb)) = fib.pair(pair::PLCF_HDD) else { return Vec::new() };
    let Some(plc) = table.get(fc as usize..(fc as usize).saturating_add(lcb as usize)) else { return Vec::new() };
    // Only the six separator stories and six per section (at most 1024) are ever used.
    let n = (lcb as usize / 4).min(6 + 6 * MAX_SECTIONS + 1);
    let cp_at = |i: usize| -> Option<u32> { plc.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) };
    let mut stories = Vec::with_capacity(n);
    let mut kept_to = 0u32;
    for i in 0..n.saturating_sub(1) {
        if let (Some(a), Some(b)) = (cp_at(i), cp_at(i + 1)) {
            let b = if a < kept_to { a } else { b.max(a) };
            kept_to = kept_to.max(b);
            stories.push((a, b));
        }
    }
    stories
}
