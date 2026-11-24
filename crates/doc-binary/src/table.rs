//! Tables: the table sprms (sgc 5) carried by a row-end (TTP) mark's grpprl, and the
//! assembly of cell-ended paragraphs into [`Table`] blocks ([MS-DOC] §2.6.3, §2.4.3).

use std::sync::Arc;

use craft_wordmodel::Rgb;
use craft_wordmodel::para::Paragraph;
use craft_wordmodel::props::{Align, Borders, CellProps, HeightRule, RowProps, TableProps, VAlign, VMerge};
use craft_wordmodel::table::{Cell, Row, Table};
use craft_wordmodel::{Block, Blocks, para_block};

use crate::fmt;
use crate::sprm::{self, Prl};

// Paragraph sprms for table membership.
const P_IN_TABLE: u16 = 0x2416;
const P_TTP: u16 = 0x2417;
/// `sprmPItap`: the table depth of a paragraph (1 for the outer table, more for nested ones).
const P_ITAP: u16 = 0x6649;

// Table sprms ([MS-DOC] §2.6.3).
const T_JC: u16 = 0x5400;
const T_JC_90: u16 = 0x548A;
const T_DXA_LEFT: u16 = 0x9601;
const T_DXA_GAP_HALF: u16 = 0x9602;
const T_TABLE_HEADER: u16 = 0x3404;
const T_TABLE_BORDERS_80: u16 = 0xD605;
const T_ROW_HEIGHT: u16 = 0x9407;
const T_DEF_TABLE: u16 = 0xD608;
const T_DEF_TABLE_SHD_80: u16 = 0xD609;
/// `sprmTDefTableShd` / `Shd2nd` / `Shd3rd`: 10-byte `Shd`s for cells 0-21, 22-43, 44-62.
const T_DEF_TABLE_SHD: u16 = 0xD612;
const T_DEF_TABLE_SHD_2ND: u16 = 0xD616;
const T_DEF_TABLE_SHD_3RD: u16 = 0xD60C;
/// `sprmTDefTableShdRaw` / `Raw2nd` / `Raw3rd`: the same `Shd` arrays over the same cell
/// ranges, which Word 2007+ writes next to the above.
const T_DEF_TABLE_SHD_RAW: u16 = 0xD670;
const T_DEF_TABLE_SHD_RAW_2ND: u16 = 0xD671;
const T_DEF_TABLE_SHD_RAW_3RD: u16 = 0xD672;
const T_CANT_SPLIT_90: u16 = 0x3403;
const T_MERGE: u16 = 0x5624;
const T_VERT_MERGE: u16 = 0xD62B;
const T_VERT_ALIGN: u16 = 0xD62C;
const T_SET_SHD: u16 = 0xD62D;
const T_CANT_SPLIT: u16 = 0x3466;
const T_CELL_WIDTH: u16 = 0xD635;

/// Most grid columns we build from a `sprmTDefTable` (the model's own cap is 63; one more
/// edge is fine but merged rows can sum spans slightly over, so clamp when assembling).
const MAX_COLS: usize = 63;

// Nested tables ([MS-DOC] §2.4.3): inner cells end at a paragraph mark with
// `sprmPFInnerTableCell`, inner rows at one with `sprmPFInnerTtp`.
const P_INNER_TABLE_CELL: u16 = 0x244B;
const P_INNER_TTP: u16 = 0x244C;
/// `sprmTTableBorders`: six `Brc`s (8 bytes each) in the order of `sprmTTableBorders80`.
const T_TABLE_BORDERS: u16 = 0xD613;
/// `sprmTCellPaddingDefault` and `sprmTCellPadding`: a `CSSA` (itcFirst, itcLim, grfbrc,
/// ftsWidth, wWidth) after the cb byte.
const T_CELL_PADDING_DEFAULT: u16 = 0xD632;
const T_CELL_PADDING: u16 = 0xD634;
/// `sprmTTableWidth`: the table's preferred width, an `FtsWWidth_Table`.
const T_TABLE_WIDTH: u16 = 0xF614;
/// `sprmTFAutofit`: 0 = fixed column widths.
const T_FAUTOFIT: u16 = 0x3615;
/// Deepest table nesting we build; deeper marks are treated as this depth, so hostile depths
/// cannot recurse without bound. Word itself stops at 63.
const MAX_NEST: u32 = 16;

/// A cell's `TC80` from `sprmTDefTable` ([MS-DOC] §2.9.313): its `TCGRF` merge and alignment
/// bits and the borders it states.
#[derive(Default, Clone)]
struct Tc {
    /// `fFirstMerged` (bit 0) and `fMerged` (bit 1): a cell with only `fMerged` continues the
    /// horizontal merge of the cell before it.
    hmerge: u8,
    /// `fVertMerge` (bit 0 here) and `fVertRestart` (bit 1 here).
    vmerge: u8,
    /// `vertAlign`: 0 top, 1 center, 2 bottom.
    valign: u8,
    /// The borders the TC80 states; a zeroed `Brc80` states nothing and the cell keeps the
    /// table's border on that side.
    borders: Option<Borders>,
}

/// What a paragraph mark says about its table context, from its PAPX.
#[derive(Default)]
pub(crate) struct RowInfo {
    pub(crate) in_table: bool,
    pub(crate) ttp: bool,
    /// The mark itself was 0x07 (a cell end), set by the walker.
    pub(crate) cell_mark: bool,
    /// Table depth from `sprmPItap` (0 when the PAPX does not state it).
    pub(crate) depth: u32,
    /// `sprmPFInnerTableCell` / `sprmPFInnerTtp`: the mark ends a nested cell or row.
    pub(crate) inner_cell: bool,
    pub(crate) inner_ttp: bool,
    /// Cell-edge positions (points) from `sprmTDefTable` — [MS-DOC] defines them per row,
    /// so each row keeps its own; ascending in well-formed rows, the first edge usually 0
    /// (the row origin). Rows without a `sprmTDefTable` keep this empty.
    pub(crate) edges: Vec<f32>,
    tcs: Vec<Tc>,
    height: Option<f32>,
    height_rule: HeightRule,
    header: bool,
    cant_split: bool,
    align: Option<Align>,
    indent: Option<f32>,
    borders: Option<Borders>,
    /// Default cell margins (top, left, bottom, right), points.
    margins: Option<[f32; 4]>,
    width: Option<f32>,
    width_pct: Option<f32>,
    fixed: bool,
    /// Horizontal merges: (itcFirst, itcLim) cell-index ranges.
    merges: Vec<(u8, u8)>,
    /// Per-cell overrides keyed by (pre-merge) cell index.
    vmerge: Vec<(u8, VMerge)>,
    /// Later entries win: the `Shd` arrays override the `Shd80` fallback before them, and an
    /// automatic `Shd` (`None`) clears it.
    shading: Vec<(u8, Option<Rgb>)>,
    widths: Vec<(u8, f32)>,
    width_pcts: Vec<(u8, f32)>,
    valign: Vec<(u8, VAlign)>,
}

/// Read the table context out of a paragraph mark's PAPX grpprl.
impl RowInfo {
    /// Does the mark belong to a table: inside one, ending a row, or ending a cell?
    pub(crate) fn is_table(&self) -> bool {
        self.in_table || self.ttp || self.cell_mark || self.depth > 0 || self.inner_cell || self.inner_ttp
    }

    /// The table depth of the mark: `sprmPItap` when stated, else 1 for any table mark.
    fn level(&self) -> u32 {
        if self.depth > 0 {
            self.depth.min(MAX_NEST)
        } else if self.is_table() {
            1
        } else {
            0
        }
    }

    /// Does this mark end a row of a table at `level`? The outer table's rows end at a TTP
    /// mark, a nested table's at an inner TTP mark ([MS-DOC] §2.4.3).
    fn ends_row(&self, level: u32) -> bool {
        if level <= 1 { self.ttp || (self.cell_mark && self.inner_ttp && self.level() == 1) } else { self.inner_ttp }
    }

    /// Does this mark end a cell of a table at `level`: a cell mark (0x07) in the outer table,
    /// a paragraph mark with `sprmPFInnerTableCell` in a nested one?
    fn ends_cell(&self, level: u32) -> bool {
        if level <= 1 { self.cell_mark } else { self.inner_cell }
    }

    /// The row height from `sprmTDyaRowHeight`, for tests.
    #[cfg(test)]
    pub(crate) fn height(&self) -> Option<f32> {
        self.height
    }

    /// The fill cell `i` ends up with (the last shading entry for it wins), for tests.
    #[cfg(test)]
    pub(crate) fn shading_of(&self, i: u8) -> Option<Rgb> {
        self.shading.iter().rfind(|(k, _)| *k == i).and_then(|(_, s)| *s)
    }

    /// The width cell `i` ends up with, for tests.
    #[cfg(test)]
    pub(crate) fn width_of(&self, i: u8) -> Option<f32> {
        self.widths.iter().rfind(|(k, _)| *k == i).map(|(_, w)| *w)
    }

    /// How many per-cell overrides the row keeps, for tests.
    #[cfg(test)]
    pub(crate) fn overrides(&self) -> usize {
        self.vmerge.len() + self.shading.len() + self.widths.len() + self.width_pcts.len() + self.valign.len() + self.merges.len()
    }

    /// The width percentage cell `i` ends up with, for tests.
    #[cfg(test)]
    pub(crate) fn width_pct_of(&self, i: u8) -> Option<f32> {
        self.width_pcts.iter().rfind(|(k, _)| *k == i).map(|(_, w)| *w)
    }
}

/// Set cell `c`'s override to `v`, replacing an earlier one: one slot per cell, however often
/// a PAPX repeats the sprm, so a shared PAPX can't pile up copies on every paragraph mark.
fn set<T>(slots: &mut Vec<(u8, T)>, c: u8, v: T) {
    match slots.iter_mut().find(|(k, _)| *k == c) {
        Some(slot) => slot.1 = v,
        None => slots.push((c, v)),
    }
}

pub(crate) fn decode(papx: &[u8]) -> RowInfo {
    let mut r = RowInfo::default();
    let mut gap_half: Option<i16> = None;
    let mut dxa_left: Option<i16> = None;
    // Cell padding by side (top, left, bottom, right), points.
    let mut padding: [Option<f32>; 4] = [None; 4];
    for prl in sprm::iter(papx) {
        // The membership sprms are paragraph sprms (sgc 1); the rest here are table sprms
        // (sgc 5), so match opcodes directly.
        match prl.op {
            P_IN_TABLE => r.in_table = prl.operand.first().copied().unwrap_or(0) != 0,
            P_TTP => r.ttp = prl.operand.first().copied().unwrap_or(0) != 0,
            P_INNER_TABLE_CELL => r.inner_cell = prl.operand.first().copied().unwrap_or(0) != 0,
            P_INNER_TTP => r.inner_ttp = prl.operand.first().copied().unwrap_or(0) != 0,
            P_ITAP => r.depth = i32_of(&prl).unwrap_or(0).max(0) as u32,
            T_DEF_TABLE => {
                r.edges = def_table_edges(prl.operand);
                r.tcs = def_table_tcs(prl.operand);
            }
            T_TABLE_HEADER => r.header = prl.operand.first().copied().unwrap_or(0) != 0,
            T_CANT_SPLIT | T_CANT_SPLIT_90 => r.cant_split = prl.operand.first().copied().unwrap_or(0) != 0,
            T_ROW_HEIGHT => {
                if let Some(v) = s16(&prl) {
                    if v < 0 {
                        r.height_rule = HeightRule::Exact;
                        r.height = Some((-(v as f32) / 20.0).clamp(0.0, 1584.0));
                    } else {
                        r.height_rule = HeightRule::AtLeast;
                        r.height = Some((v as f32 / 20.0).clamp(0.0, 1584.0));
                    }
                }
            }
            T_JC | T_JC_90 => {
                r.align = match u16_of(&prl).unwrap_or(0) {
                    1 => Some(Align::Center),
                    2 => Some(Align::Right),
                    _ => Some(Align::Left),
                };
            }
            T_DXA_LEFT => dxa_left = s16(&prl),
            T_DXA_GAP_HALF => gap_half = s16(&prl),
            T_TABLE_BORDERS_80 => r.borders = table_borders(prl.operand),
            T_TABLE_BORDERS => r.borders = table_borders_brc(prl.operand),
            T_CELL_PADDING_DEFAULT | T_CELL_PADDING => {
                // cb, itcFirst, itcLim, grfbrc (1 top, 2 left, 4 bottom, 8 right), ftsWidth
                // (3 = twips), wWidth. Padding of the first cell stands for the table's default.
                if let [_, first, _, sides, fts, w0, w1, ..] = prl.operand
                    && *fts == 3
                    && (*first == 0 || prl.op == T_CELL_PADDING_DEFAULT)
                {
                    let w = (u16::from_le_bytes([*w0, *w1]) as f32 / 20.0).min(1584.0);
                    for (bit, slot) in [(1u8, 0usize), (2, 1), (4, 2), (8, 3)] {
                        if sides & bit != 0 {
                            padding[slot] = Some(w);
                        }
                    }
                }
            }
            T_TABLE_WIDTH => {
                if let [fts, w0, w1, ..] = prl.operand {
                    let w = u16::from_le_bytes([*w0, *w1]);
                    match fts {
                        2 => r.width_pct = Some((w as f32 / 50.0).clamp(0.0, 100.0)).filter(|p| *p > 0.0),
                        3 => r.width = Some((w as i16 as f32 / 20.0).clamp(0.0, 1584.0)).filter(|w| *w > 0.0),
                        _ => {}
                    }
                }
            }
            T_FAUTOFIT => r.fixed = prl.operand.first() == Some(&0),
            T_MERGE => {
                // Rows apply only the first MAX_COLS merges.
                if let [a, b, ..] = prl.operand
                    && b > a
                    && r.merges.len() < MAX_COLS
                {
                    r.merges.push((*a, *b));
                }
            }
            T_VERT_MERGE => {
                // cb, itc, flag: 1 = continues the merge from above, 3 = restarts it.
                if let [_, itc, flag, ..] = prl.operand {
                    let m = match flag {
                        3 => VMerge::Restart,
                        1 => VMerge::Continue,
                        _ => VMerge::None,
                    };
                    // The first override for a cell wins.
                    if m != VMerge::None && !r.vmerge.iter().any(|(k, _)| k == itc) {
                        r.vmerge.push((*itc, m));
                    }
                }
            }
            T_SET_SHD => {
                if let Some(rgb) = prl.operand.get(3..).and_then(shd_rgb)
                    && let [a, b, ..] = prl.operand.get(1..3).unwrap_or(&[])
                {
                    for c in *a..(*b).min(MAX_COLS as u8) {
                        set(&mut r.shading, c, Some(rgb));
                    }
                }
            }
            T_DEF_TABLE_SHD_80 => {
                let cells = prl.operand.get(1..).unwrap_or(&[]);
                for (i, b) in cells.as_chunks::<2>().0.iter().take(MAX_COLS).enumerate() {
                    if let Some(rgb) = shd80_rgb(b) {
                        set(&mut r.shading, i as u8, Some(rgb));
                    }
                }
            }
            T_DEF_TABLE_SHD | T_DEF_TABLE_SHD_RAW | T_DEF_TABLE_SHD_2ND | T_DEF_TABLE_SHD_RAW_2ND | T_DEF_TABLE_SHD_3RD | T_DEF_TABLE_SHD_RAW_3RD => {
                // Each array covers its own cell range (0-21, 22-43, 44-62); extra entries
                // are ignored so an over-long array can't override the next range.
                let (first, lim) = match prl.op {
                    T_DEF_TABLE_SHD_2ND | T_DEF_TABLE_SHD_RAW_2ND => (22, 44),
                    T_DEF_TABLE_SHD_3RD | T_DEF_TABLE_SHD_RAW_3RD => (44, MAX_COLS),
                    _ => (0, 22),
                };
                let cells = prl.operand.get(1..).unwrap_or(&[]);
                let raw = matches!(prl.op, T_DEF_TABLE_SHD_RAW | T_DEF_TABLE_SHD_RAW_2ND | T_DEF_TABLE_SHD_RAW_3RD);
                for (i, b) in cells.as_chunks::<10>().0.iter().take(lim - first).enumerate() {
                    // In the Raw arrays a nil Shd (ipat 0xFFFF) means "as the table style
                    // shades it", so it leaves any earlier fill alone.
                    if raw && b.get(8..10) == Some(&[0xFF, 0xFF]) {
                        continue;
                    }
                    set(&mut r.shading, (first + i) as u8, shd_rgb(b));
                }
            }
            T_CELL_WIDTH => {
                // Operand: cb (1 byte), itcFirst (1 byte), itcLim (1 byte), ftsWidth (1 byte), wWidth (2 bytes).
                // [MS-DOC] §2.9.290 sprmTCellWidth (0xD635).
                // ftsWidth: 3 = twips (dxa), 2 = percentage (50ths of a percent).
                if let [_, a, b, fts, w0, w1, ..] = prl.operand {
                    if *fts == 3 {
                        let w = (i16::from_le_bytes([*w0, *w1]) as f32 / 20.0).clamp(0.0, 1584.0);
                        for c in *a..(*b).min(MAX_COLS as u8) {
                            set(&mut r.widths, c, w);
                        }
                    } else if *fts == 2 {
                        let pct = (u16::from_le_bytes([*w0, *w1]) as f32 / 50.0).clamp(0.0, 100.0);
                        for c in *a..(*b).min(MAX_COLS as u8) {
                            set(&mut r.width_pcts, c, pct);
                        }
                    }
                }
            }
            T_VERT_ALIGN => {
                if let [_, a, b, va, ..] = prl.operand {
                    let v = match va {
                        1 => VAlign::Center,
                        2 => VAlign::Bottom,
                        _ => VAlign::Top,
                    };
                    for c in *a..(*b).min(MAX_COLS as u8) {
                        set(&mut r.valign, c, v);
                    }
                }
            }
            _ => {}
        }
    }
    // [MS-DOC] §2.6.3: `sprmTDxaLeft` is the row's left edge (the first cell's left boundary);
    // the table indent is that edge, as Word lays out a Word 97-2003 table (compatibility mode).
    if let Some(l) = dxa_left {
        r.indent = Some((l as f32 / 20.0).clamp(-1584.0, 1584.0));
    }
    // `sprmTDxaGapHalf` is each cell's left and right margin unless a padding sprm names the side.
    if gap_half.is_some() || padding.iter().any(Option::is_some) {
        let gap = gap_half.map_or(5.4, |g| (g.max(0) as f32 / 20.0).min(1584.0));
        r.margins = Some([padding[0].unwrap_or(0.0), padding[1].unwrap_or(gap), padding[2].unwrap_or(0.0), padding[3].unwrap_or(gap)]);
    }
    r
}

/// A finished paragraph plus the bookkeeping the block assembly needs.
pub(crate) struct ParaOut {
    pub(crate) para: Paragraph,
    /// CP just past the terminating mark.
    pub(crate) end_cp: u32,
    /// Table context from the mark's PAPX; `None` for a paragraph outside a table.
    pub(crate) row: Option<Box<RowInfo>>,
}

fn level_of(p: &ParaOut) -> u32 {
    p.row.as_ref().map_or(0, |r| r.level())
}

/// Group in-table paragraphs into [`Table`] blocks, nested tables inside their cells
/// ([MS-DOC] §2.4.3); the rest pass through. A row that never closes is kept as a row, so no
/// text is lost.
pub(crate) fn assemble(paras: Vec<ParaOut>) -> Blocks {
    let mut it = paras.into_iter().peekable();
    let mut out: Blocks = Vec::new();
    while let Some(next) = it.peek() {
        if level_of(next) > 0 {
            let (t, rest) = table(&mut it, 1);
            if let Some(t) = t {
                out.push(Arc::new(Block::Table(t)));
            }
            out.extend(rest);
            continue;
        }
        if let Some(mut p) = it.next() {
            // A drop cap is stored as a paragraph of its own (the letter, with sprmPDcs and its
            // frame) before the paragraph it starts; the model keeps it as that paragraph's
            // property, so the two are joined.
            if p.para.props.drop_cap.is_some()
                && !p.para.text.is_empty()
                && p.para.section.is_none()
                && it.peek().is_some_and(|n| level_of(n) == 0 && !n.para.text.is_empty())
                && let Some(next) = it.next()
            {
                let mut head = p.para;
                let mut props = next.para.props.clone();
                props.drop_cap = head.props.drop_cap;
                props.drop_cap_margin = head.props.drop_cap_margin;
                props.drop_cap_distance = head.props.frame.as_ref().map(|f| f.h_space).filter(|d| *d > 0.0);
                let mark = next.para.mark.clone();
                head.append(next.para);
                head.props = props;
                head.mark = mark;
                p.para = head;
            }
            out.push(para_block(p.para));
        }
    }
    out
}

/// One cell before merging: its blocks and the pre-merge index it had in its row.
fn flush_cell(cells: &mut Vec<Cell>, cell: &mut Blocks) {
    let mut blocks = std::mem::take(cell);
    if blocks.is_empty() {
        blocks.push(para_block(Paragraph::new()));
    }
    cells.push(Cell { props: CellProps { span: 1, ..Default::default() }, blocks, controls: Default::default() });
}

/// The table at `level`: cells end at that level's cell marks, rows at its row marks, and
/// deeper marks form tables nested in the current cell. In-table paragraphs after the last row
/// mark belong to no row ([MS-DOC] §2.4.3: a row ends at its TTP mark); they are returned as the
/// paragraphs they are, to follow the table, as Word and LibreOffice show them.
fn table(it: &mut std::iter::Peekable<std::vec::IntoIter<ParaOut>>, level: u32) -> (Option<Table>, Blocks) {
    let mut rows: Vec<(Vec<Cell>, Box<RowInfo>)> = Vec::new();
    let mut cells: Vec<Cell> = Vec::new();
    let mut cell: Blocks = Vec::new();
    while let Some(next) = it.peek() {
        let l = level_of(next);
        if l < level {
            break;
        }
        if l > level {
            let (t, rest) = table(it, level + 1);
            if let Some(t) = t {
                cell.push(Arc::new(Block::Table(t)));
            }
            cell.extend(rest);
            continue;
        }
        let Some(p) = it.next() else { break };
        let Some(info) = p.row else { break };
        if info.ends_row(level) {
            // Word writes rows as N content cells each ended by their own cell mark, followed by
            // an empty row-end mark that carries the row's TDefTable. When that mark ends no
            // content of its own and the TDefTable already has a column per flushed cell, it is
            // a pure row terminator, not the end of a phantom (N+1)th cell. A TTP mark with
            // content, or one short of the TDefTable's columns, still ends a real cell.
            let row_end_only = p.para.text.is_empty() && cell.is_empty() && !cells.is_empty() && (info.edges.is_empty() || cells.len() + 1 >= info.edges.len());
            if !p.para.text.is_empty() {
                cell.push(para_block(p.para));
            }
            if !row_end_only {
                flush_cell(&mut cells, &mut cell);
            }
            rows.push((std::mem::take(&mut cells), info));
            continue;
        }
        let ends_cell = info.ends_cell(level);
        cell.push(para_block(p.para));
        if ends_cell {
            flush_cell(&mut cells, &mut cell);
        }
    }
    let mut rest: Blocks = cells.into_iter().flat_map(|c| c.blocks).collect();
    rest.extend(cell);
    if rows.is_empty() {
        return (None, rest);
    }
    (Some(build_table(rows)), rest)
}

/// A table from its rows: the grid from every row's cell edges, each cell's span over it, the
/// table properties from the first row that states each.
fn build_table(rows: Vec<(Vec<Cell>, Box<RowInfo>)>) -> Table {
    let mut props = TableProps::default();
    // The grid: every distinct cell edge of every row ([MS-DOC] defines edges per row).
    let mut all: Vec<f32> = rows.iter().flat_map(|(_, i)| i.edges.iter().copied()).filter(|e| e.is_finite()).collect();
    all.sort_by(f32::total_cmp);
    all.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    if all.len() > MAX_COLS + 1 {
        all.truncate(MAX_COLS + 1);
    }
    let grid: Vec<f32> = all.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
    let col_of = |e: f32| all.iter().position(|x| (x - e).abs() < 0.05).or_else(|| all.iter().position(|x| *x >= e));
    let mut out_rows = Vec::with_capacity(rows.len());
    for (cells, info) in rows {
        if info.borders.is_some() && props.borders.is_none() {
            props.borders = info.borders;
        }
        if props.align.is_none() {
            props.align = info.align;
        }
        if props.indent.is_none() {
            props.indent = info.indent;
        }
        if props.cell_margins.is_none() {
            props.cell_margins = info.margins;
        }
        if props.width.is_none() && props.width_pct.is_none() {
            props.width = info.width;
            props.width_pct = info.width_pct;
        }
        props.fixed |= info.fixed;
        let (cells, bounds) = build_row(cells, &info);
        let mut row = Row {
            props: RowProps { height: info.height, height_rule: info.height_rule, header: info.header, cant_split: info.cant_split, ..Default::default() },
            cells,
            controls: Default::default(),
        };
        // Spans over the grid from the cells' edges, with the columns the row leaves out
        // before and after it.
        if !grid.is_empty() && info.edges.len() >= 2 {
            let mut first_col = None;
            let mut last_col = 0usize;
            for (c, (a, b)) in row.cells.iter_mut().zip(&bounds) {
                let (Some(&left), Some(&right)) = (info.edges.get(*a), info.edges.get(*b)) else { break };
                let (Some(ca), Some(cb)) = (col_of(left), col_of(right)) else { break };
                if first_col.is_none() {
                    first_col = Some(ca);
                }
                c.props.span = (cb.saturating_sub(ca)).clamp(1, MAX_COLS) as u32;
                last_col = cb.max(ca + 1);
            }
            if let Some(f) = first_col
                && f > 0
            {
                row.props.grid_before = f as u32;
                row.props.w_before = Some(grid.iter().take(f).sum());
            }
            if last_col > 0 && last_col < grid.len() {
                row.props.grid_after = (grid.len() - last_col) as u32;
                row.props.w_after = Some(grid.iter().skip(last_col).sum());
            }
        } else if !grid.is_empty() {
            // A row without edges of its own (a stray row): size its cells from the grid.
            for (i, c) in row.cells.iter_mut().enumerate() {
                if c.props.width.is_none() && c.props.span == 1 {
                    c.props.width = grid.get(i).copied();
                }
            }
        }
        out_rows.push(row);
    }
    if props.indent.is_none() {
        // Without `sprmTDxaLeft` the row origin (the first cell's left boundary) is the indent.
        props.indent = all.first().copied().filter(|e| *e != 0.0);
    }
    Table { props, grid, rows: out_rows, controls: Default::default() }
}

/// A row's cells with their own formatting, horizontally merged cells folded into the first
/// of their range, and for each resulting cell the range of pre-merge edges it covers.
fn build_row(mut cells: Vec<Cell>, info: &RowInfo) -> (Vec<Cell>, Vec<(usize, usize)>) {
    // Per-cell formatting first (sprm cell indices count pre-merge cells), then merges.
    let edge_width = |i: usize| match (info.edges.get(i), info.edges.get(i + 1)) {
        (Some(a), Some(b)) if b > a => Some((b - a).min(1584.0)),
        _ => None,
    };
    let mut merged_into_prev = vec![false; cells.len()];
    for (i, c) in cells.iter_mut().enumerate() {
        if let Some(tc) = info.tcs.get(i) {
            c.props.vmerge = match tc.vmerge {
                1 => VMerge::Continue,
                2 | 3 => VMerge::Restart,
                _ => VMerge::None,
            };
            c.props.valign = match tc.valign {
                1 => Some(VAlign::Center),
                2 => Some(VAlign::Bottom),
                _ => None,
            };
            c.props.borders = tc.borders;
            if tc.hmerge & 3 == 2 && i > 0 {
                merged_into_prev[i] = true;
            }
        }
        c.props.width = edge_width(i);
        let i = i as u8;
        if let Some((_, m)) = info.vmerge.iter().find(|(k, _)| *k == i) {
            c.props.vmerge = *m;
        }
        if let Some((_, w)) = info.widths.iter().rfind(|(k, _)| *k == i) {
            c.props.width = Some(*w);
        }
        if let Some((_, pct)) = info.width_pcts.iter().rfind(|(k, _)| *k == i) {
            c.props.width_pct = Some(*pct);
        }
        if let Some((_, s)) = info.shading.iter().rfind(|(k, _)| *k == i) {
            c.props.shading = *s;
        }
        if let Some((_, v)) = info.valign.iter().rfind(|(k, _)| *k == i) {
            c.props.valign = Some(*v);
        }
    }
    for &(a, b) in info.merges.iter().take(MAX_COLS) {
        for m in merged_into_prev.iter_mut().take((b as usize).min(cells.len())).skip(a as usize + 1) {
            *m = true;
        }
    }
    let mut out: Vec<Cell> = Vec::with_capacity(cells.len());
    let mut bounds: Vec<(usize, usize)> = Vec::with_capacity(cells.len());
    for (i, c) in cells.into_iter().enumerate() {
        if merged_into_prev.get(i).copied().unwrap_or(false)
            && let (Some(first), Some(bound)) = (out.last_mut(), bounds.last_mut())
        {
            first.props.span = (first.props.span + 1).min(MAX_COLS as u32);
            first.props.width = match (first.props.width, c.props.width) {
                (Some(x), Some(y)) => Some(x + y),
                (x, _) => x,
            };
            // A merged cell's text stays, after the first cell's.
            first.blocks.extend(c.blocks.into_iter().filter(|x| x.as_para().is_none_or(|p| !p.is_empty())));
            if first.blocks.len() > 1 && first.blocks.first().and_then(|x| x.as_para()).is_some_and(Paragraph::is_empty) {
                first.blocks.remove(0);
            }
            bound.1 = i + 1;
            continue;
        }
        out.push(c);
        bounds.push((i, i + 1));
    }
    (out, bounds)
}

/// The TC80s after the edges of a `sprmTDefTable` operand (cb u16, itcMac u8, itcMac+1 edges),
/// 20 bytes each: TCGRF, wUnused, then the top, left, bottom and right `Brc80`s.
fn def_table_tcs(operand: &[u8]) -> Vec<Tc> {
    let itc = (operand.get(2).copied().unwrap_or(0) as usize).min(MAX_COLS);
    let at = 3 + (itc + 1) * 2;
    let mut out = Vec::with_capacity(itc);
    for i in 0..itc {
        let Some(b) = operand.get(at + i * 20..at + i * 20 + 20) else { break };
        let grf = u16::from_le_bytes([b[0], b[1]]);
        let side = |k: usize| b.get(k..k + 4).filter(|x| *x != [0u8; 4]).and_then(fmt::brc80);
        let borders = Borders { top: side(4), left: side(8), bottom: side(12), right: side(16), ..Default::default() };
        let stated = borders.top.is_some() || borders.left.is_some() || borders.bottom.is_some() || borders.right.is_some();
        out.push(Tc { hmerge: (grf & 3) as u8, vmerge: ((grf >> 5) & 3) as u8, valign: ((grf >> 7) & 3) as u8, borders: stated.then_some(borders) });
    }
    out
}

/// Table borders from `sprmTTableBorders`: cb (0x30) then six `Brc`s of 8 bytes.
fn table_borders_brc(operand: &[u8]) -> Option<Borders> {
    let b = |i: usize| {
        let raw = operand.get(1 + i * 8..1 + i * 8 + 8)?;
        let mut op = [8u8; 9];
        op[1..].copy_from_slice(raw);
        fmt::brc(&op)
    };
    Some(Borders { top: b(0), left: b(1), bottom: b(2), right: b(3), between: b(4), inside_v: b(5) })
}

/// Cell-edge positions (points) from a `sprmTDefTable` operand: cb u16, itcMac u8, then
/// itcMac+1 twip boundaries followed by the TC80s (skipped). Edges are returned as stored:
/// per-row, in operand order; sorting and capping happen when the table grid is assembled.
fn def_table_edges(operand: &[u8]) -> Vec<f32> {
    let itc = operand.get(2).copied().unwrap_or(0) as usize;
    if itc == 0 {
        return Vec::new();
    }
    // itcMac is a u8, so this allocation stays tiny; the loop stops at the operand's end.
    let mut edges = Vec::with_capacity((itc + 1).min(256));
    for i in 0..=itc {
        match operand.get(3 + i * 2..3 + i * 2 + 2) {
            Some(b) => edges.push(i16::from_le_bytes([b[0], b[1]]) as f32 / 20.0),
            None => break,
        }
    }
    edges
}

/// Table borders from `sprmTTableBorders80`: cb (0x18) then six Brc80s in the order top,
/// left, bottom, right, inside-horizontal, inside-vertical, read as paragraph borders are.
fn table_borders(operand: &[u8]) -> Option<Borders> {
    let b = |i: usize| operand.get(1 + i * 4..1 + i * 4 + 4).and_then(fmt::brc80);
    Some(Borders { top: b(0), left: b(1), bottom: b(2), right: b(3), between: b(4), inside_v: b(5) })
}

/// Background colour from a 10-byte `Shd` ([`fmt::shd`]: patterns mixed as they look).
fn shd_rgb(b: &[u8]) -> Option<Rgb> {
    fmt::shd(b)
}

/// Background colour from a 2-byte `Shd80` ([`fmt::shd80`]).
fn shd80_rgb(b: &[u8]) -> Option<Rgb> {
    fmt::shd80(u16::from_le_bytes([*b.first()?, *b.get(1)?]))
}

fn u16_of(p: &Prl) -> Option<u16> {
    match p.operand {
        [b0, b1, ..] => Some(u16::from_le_bytes([*b0, *b1])),
        _ => None,
    }
}

fn s16(p: &Prl) -> Option<i16> {
    u16_of(p).map(|v| v as i16)
}

fn i32_of(p: &Prl) -> Option<i32> {
    match p.operand {
        [b0, b1, b2, b3, ..] => Some(i32::from_le_bytes([*b0, *b1, *b2, *b3])),
        _ => None,
    }
}
