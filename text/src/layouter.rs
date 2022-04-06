use std::{
  collections::VecDeque,
  ops::Range,
  sync::{Arc, RwLock},
};

use fontdb::ID;
use lyon_path::geom::{euclid::num::Zero, Point, Rect, Size};
use ttf_parser::GlyphId;
use unic_bidi::Level;
use unicode_script::{Script, UnicodeScript};

use crate::{font_db::FontDB, shaper::Glyph, Align, Em, FontSize, HAlign, VAlign};

/// Describe the glyph information, and axis relative to the typography bounds.
#[derive(Debug, Clone)]
pub struct TGlyph {
  /// The font face id of the glyph.
  pub face_id: ID,
  /// Where the glyph drawing start.
  pub position: Point<Em>,
  /// How many pixels the line advances after drawing this glyph when setting
  /// text in horizontal direction, advance on x-axis, else y-axis.
  pub advance: Size<Em>,
  /// The id of the glyph.
  pub glyph_id: GlyphId,
  /// An cluster of origin text as byte index.
  pub cluster: u32,
}

#[derive(Clone)]
pub enum Overflow {
  Clip,
}

#[derive(Clone, Copy, PartialEq)]
pub enum PlaceLineDirection {
  /// place the line from left to right, the direction use to shape text must be
  /// vertical
  LeftToRight,
  /// place the line from right to let, the direction use to shape text must be
  /// vertical
  RightToLeft,
  /// place the line from top to bottom, the direction use to shape text must be
  /// vertical
  TopToBottom,
}
/// `mode` should match the direction use to shape text, not check if layout
/// inputs mix horizontal & vertical.
#[derive(Clone)]
pub struct TypographyCfg {
  pub line_height: Option<Em>,
  pub letter_space: Option<Em>,
  pub h_align: Option<HAlign>,
  pub v_align: Option<VAlign>,
  // The rect glyphs can place, and hint `TypographyMan` where to early return.
  // the result of typography may over bounds.
  pub bounds: Rect<Em>,
  pub line_dir: PlaceLineDirection,
  pub overflow: Overflow,
}

/// Trait control how to place glyph inline.
pub trait InlineCursor {
  /// advance the cursor by a glyph, the `glyph` position is relative to self
  /// before call this method,  and relative to the cursor coordinate after
  /// call.
  /// return if the glyph is over boundary.
  fn advance_glyph(&mut self, glyph: &mut TGlyph, origin_text: &str) -> bool;

  fn advance(&mut self, offset: Em) -> bool;

  /// cursor position relative of inline.
  fn position(&self) -> Em;

  fn cursor(&self) -> Point<Em>;
}

#[derive(Default)]
pub struct VisualLine {
  // todo: not set
  pub line_rect: Rect<Em>,
  pub glyphs: VecDeque<TGlyph>,
}

#[derive(Default)]
pub struct VisualInfos {
  visual_lines: Vec<VisualLine>,
  /// The bounds after typography all, it's may over the bounds
  rect: Option<Rect<Em>>,
}

/// pixel
pub struct TypographyMan<Inputs> {
  font_db: Arc<RwLock<FontDB>>,
  cfg: TypographyCfg,
  /// Not directly use text as inputs, but accept glyphs after text shape
  /// because both simple text and rich text can custom compose its glyph runs
  /// by text reorder result and its style .
  inputs: Inputs,
  cursor: Point<Em>,
  visual_info: VisualInfos,
}

impl<'a, Inputs, Runs> TypographyMan<Inputs>
where
  Inputs: DoubleEndedIterator<Item = InputParagraph<Runs>>,
  Runs: DoubleEndedIterator<Item = InputRun<'a>>,
{
  pub fn new(inputs: Inputs, cfg: TypographyCfg, font_db: Arc<RwLock<FontDB>>) -> Self {
    let x_cursor = match (cfg.h_align, cfg.line_dir) {
      (Some(HAlign::Right), _) | (_, PlaceLineDirection::RightToLeft) => cfg.bounds.max_x(),
      _ => Em(0.),
    };
    let y_cursor = cfg
      .v_align
      .and_then(|v| (matches!(v, VAlign::Bottom)).then(|| cfg.bounds.max_y()))
      .unwrap_or(Em(0.));

    Self {
      font_db,
      cfg,
      inputs,
      cursor: (x_cursor, y_cursor).into(),
      visual_info: <_>::default(),
    }
  }

  pub fn typography_all(&mut self) -> Rect<Em> {
    while let Some(p) = self.next_input_paragraph() {
      self.consume_paragraph(p);
      if !self.cfg.bounds.contains(self.cursor) {
        break;
      }
    }
    let lines = &mut self.visual_info.visual_lines;
    let mut rect = lines
      .iter()
      .fold(None, |rect: Option<Rect<Em>>, l| {
        let union_rect = if let Some(rect) = rect {
          rect.union(&l.line_rect)
        } else {
          l.line_rect
        };
        Some(union_rect)
      })
      .unwrap_or(Rect::zero());

    let bounds = self.cfg.bounds;
    if let Some(VAlign::Center) = self.cfg.v_align {
      fn center_y_align_to(rect: &mut Rect<Em>, align_to: &Rect<Em>) {
        let center1 = rect.min_y() + rect.height() / 2.;
        let center2 = align_to.min_y() + align_to.height() / 2.;
        rect.origin.y += center2 - center1;
      }
      center_y_align_to(&mut rect, &bounds);
      lines
        .iter_mut()
        .for_each(|l| center_y_align_to(&mut l.line_rect, &bounds));
    }
    if let Some(HAlign::Center) = self.cfg.h_align {
      fn center_x_align_to(rect: &mut Rect<Em>, align_to: &Rect<Em>) {
        let center1 = rect.min_x() + rect.width() / 2.;
        let center2 = align_to.min_x() + align_to.width() / 2.;
        rect.origin.x += center2 - center1;
      }
      center_x_align_to(&mut rect, &bounds);
      lines
        .iter_mut()
        .for_each(|l| center_x_align_to(&mut l.line_rect, &bounds));
    }
    self.visual_info.rect = Some(rect);
    rect
  }

  pub fn next_input_paragraph(&mut self) -> Option<InputParagraph<Runs>> {
    if self.cfg.is_rev_place_line() {
      self.inputs.next_back()
    } else {
      self.inputs.next()
    }
  }

  #[inline]
  pub fn visual_info(&self) -> &VisualInfos { &self.visual_info }

  fn consume_paragraph(&mut self, p: InputParagraph<Runs>) {
    let mut runs = p.runs.peekable();
    if !self.visual_info.visual_lines.is_empty() || self.cfg.is_rev_place_line() {
      if let Some(r) = runs.peek() {
        let line_height = self.line_height_with_glyph(r.glyphs.first());
        self.advance_to_new_line(line_height * r.font_size.into_em());
      }
    }
    if self.cfg.should_rev_place_glyph() {
      runs.rev().for_each(|r| {
        let cursor = RightToLeftCursor { pos: self.cursor };
        self.consume_run_with_letter_space_cursor(&r, cursor)
      });
    } else if self.cfg.line_dir.is_horizontal() {
      runs.for_each(|r| {
        let cursor = LeftToRightCursor { pos: self.cursor };
        self.consume_run_with_letter_space_cursor(&r, cursor)
      });
    } else {
      runs.for_each(|r| {
        let cursor = TopToBottomCursor { pos: self.cursor };
        self.consume_run_with_letter_space_cursor(&r, cursor)
      });
    }
  }

  fn consume_run_with_letter_space_cursor(
    &mut self,
    run: &InputRun,
    inner_cursor: impl InlineCursor,
  ) {
    let letter_space = run
      .letter_space
      .or(self.cfg.letter_space)
      .unwrap_or(Em::zero());
    if letter_space != Em::zero() {
      let cursor = LetterSpaceCursor::new(inner_cursor, letter_space);
      self.consume_run_with_bounds_cursor(run, cursor);
    } else {
      self.consume_run_with_bounds_cursor(run, inner_cursor);
    }
  }

  fn consume_run_with_bounds_cursor(&mut self, run: &InputRun, inner_cursor: impl InlineCursor) {
    if self.cfg.h_align != Some(HAlign::Center) && self.cfg.v_align != Some(VAlign::Center) {
      let bounds = if self.cfg.line_dir.is_horizontal() {
        self.cfg.bounds.x_range()
      } else {
        self.cfg.bounds.y_range()
      };
      let cursor = BoundsCursor { inner_cursor, bounds };
      self.consume_run(run, cursor);
    } else {
      self.consume_run(run, inner_cursor);
    }
  }

  fn consume_run(&mut self, run: &InputRun, cursor: impl InlineCursor) {
    let font_size = run.font_size;
    let text = run.text;
    if self.cfg.should_rev_place_glyph() {
      self.place_glyphs(cursor, font_size, text, run.glyphs.iter().rev());
    } else {
      self.place_glyphs(cursor, font_size, text, run.glyphs.iter());
    }
  }

  fn place_glyphs<'b>(
    &mut self,
    mut cursor: impl InlineCursor,
    font_size: FontSize,
    text: &str,
    runs: impl Iterator<Item = &'b Glyph>,
  ) {
    for g in runs {
      let mut at = TGlyph::new(font_size, g);
      let over_boundary = cursor.advance_glyph(&mut at, text);
      self.push_glyph(at);
      if over_boundary {
        break;
      }
    }
    self.cursor = cursor.cursor();
  }

  fn push_glyph(&mut self, g: TGlyph) {
    let line = self.visual_info.visual_lines.last_mut();
    if self.cfg.should_rev_place_glyph() {
      line.unwrap().glyphs.push_front(g)
    } else {
      line.unwrap().glyphs.push_back(g)
    }
  }

  fn line_height_with_glyph(&self, g: Option<&Glyph>) -> Em {
    self
      .cfg
      .line_height
      .or_else(|| {
        g.and_then(|g| {
          let face = self.font_db.read().unwrap();
          face.try_get_face_data(g.face_id).map(|face| {
            let p_gap = match self.cfg.line_dir {
              PlaceLineDirection::LeftToRight | PlaceLineDirection::RightToLeft => {
                face.vertical_line_gap().unwrap_or_else(|| face.line_gap())
              }
              PlaceLineDirection::TopToBottom => face.line_gap(),
            };
            Em(p_gap as f32 / face.units_per_em() as f32)
          })
        })
      })
      .unwrap_or(Em(1.))
  }

  fn advance_to_new_line(&mut self, offset: Em) {
    match self.cfg.line_dir {
      PlaceLineDirection::LeftToRight => self.cursor.x += offset,
      PlaceLineDirection::RightToLeft => self.cursor.x -= offset,
      PlaceLineDirection::TopToBottom => self.cursor.y += offset,
    }

    // reset inline cursor
    if self.cfg.line_dir.is_horizontal() {
      if let Some(VAlign::Bottom) = self.cfg.v_align {
        self.cursor.y = self.cfg.bounds.max_y();
      } else {
        self.cursor.y = Em::zero();
      }
    } else {
      if let Some(HAlign::Right) = self.cfg.h_align {
        self.cursor.x = self.cfg.bounds.max_x();
      } else {
        self.cursor.x = Em::zero();
      }
    }

    self.visual_info.visual_lines.push(VisualLine::default())
  }
}

pub struct InputParagraph<Runs> {
  /// Horizontal align if paragraph in a horizontal layouter, Vertical align if
  /// paragraph in vertical layouter.
  pub align: Option<Align>,
  pub runs: Runs,
  pub level: Level,
}

/// A text run with its glyphs and style
#[derive(Clone)]
pub struct InputRun<'a> {
  pub text: &'a str,
  pub glyphs: &'a [Glyph],
  pub font_size: FontSize,
  pub letter_space: Option<Em>,
}

impl<'a> InputRun<'a> {
  pub fn pixel_glyphs<'b, C>(&'b self, mut cursor: C) -> impl Iterator<Item = TGlyph> + 'b
  where
    C: InlineCursor + 'b,
  {
    self.glyphs.iter().map(move |g| {
      let font_size = self.font_size;
      let mut at = TGlyph::new(font_size, g);
      cursor.advance_glyph(&mut at, self.text);
      at
    })
  }
}

impl TGlyph {
  fn new(font_size: FontSize, g: &Glyph) -> Self {
    Self {
      face_id: g.face_id,
      advance: Size::new(g.x_advance, g.y_advance) * font_size.into_em(),
      position: Point::new(g.x_offset, g.y_offset) * font_size.into_em(),
      glyph_id: g.glyph_id,
      cluster: g.cluster,
    }
  }
}

pub struct LeftToRightCursor {
  pub pos: Point<Em>,
}

pub struct RightToLeftCursor {
  pos: Point<Em>,
}

pub struct TopToBottomCursor {
  pub pos: Point<Em>,
}

pub struct LetterSpaceCursor<I> {
  inner_cursor: I,
  letter_space: Em,
}

struct BoundsCursor<Inner> {
  inner_cursor: Inner,
  bounds: Range<Em>,
}

impl<I> LetterSpaceCursor<I> {
  pub fn new(inner_cursor: I, letter_space: Em) -> Self { Self { inner_cursor, letter_space } }
}

impl InlineCursor for LeftToRightCursor {
  fn advance_glyph(&mut self, g: &mut TGlyph, _: &str) -> bool {
    g.position += self.pos.to_vector();
    self.pos.x = g.position.x + g.advance.width;

    false
  }

  fn advance(&mut self, offset: Em) -> bool {
    self.pos.x += offset;
    false
  }

  fn position(&self) -> Em { self.pos.x }

  fn cursor(&self) -> Point<Em> { self.pos }
}

impl InlineCursor for RightToLeftCursor {
  fn advance_glyph(&mut self, g: &mut TGlyph, _: &str) -> bool {
    let cursor_offset = g.position.x + g.advance.width;
    g.position.x = self.pos.x - g.advance.width;
    g.position.y += self.pos.y;
    self.pos.x -= cursor_offset;

    false
  }

  fn advance(&mut self, offset: Em) -> bool {
    self.pos.x += offset;
    false
  }

  fn position(&self) -> Em { self.pos.x }

  fn cursor(&self) -> Point<Em> { self.pos }
}

impl InlineCursor for TopToBottomCursor {
  fn advance_glyph(&mut self, g: &mut TGlyph, _: &str) -> bool {
    g.position += self.pos.to_vector();
    self.pos.y = g.position.y + g.advance.height;

    false
  }

  fn advance(&mut self, offset: Em) -> bool {
    self.pos.y += offset;
    false
  }

  fn position(&self) -> Em { self.pos.y }

  fn cursor(&self) -> Point<Em> { self.pos }
}

impl<I: InlineCursor> InlineCursor for LetterSpaceCursor<I> {
  fn advance_glyph(&mut self, g: &mut TGlyph, origin_text: &str) -> bool {
    let cursor = &mut self.inner_cursor;
    let res = cursor.advance_glyph(g, origin_text);

    let c = origin_text[g.cluster as usize..].chars().next().unwrap();
    if letter_spacing_char(c) {
      return cursor.advance(self.letter_space);
    }

    res
  }

  fn advance(&mut self, offset: Em) -> bool { self.inner_cursor.advance(offset) }

  fn position(&self) -> Em { self.inner_cursor.position() }

  fn cursor(&self) -> Point<Em> { self.inner_cursor.cursor() }
}

impl<I: InlineCursor> InlineCursor for BoundsCursor<I> {
  fn advance_glyph(&mut self, glyph: &mut TGlyph, origin_text: &str) -> bool {
    self.inner_cursor.advance_glyph(glyph, origin_text);
    self.bounds.contains(&self.position())
  }

  fn advance(&mut self, offset: Em) -> bool {
    self.inner_cursor.advance(offset);
    self.bounds.contains(&self.position())
  }

  fn position(&self) -> Em { self.inner_cursor.position() }

  fn cursor(&self) -> Point<Em> { self.inner_cursor.cursor() }
}

impl PlaceLineDirection {
  fn is_horizontal(&self) -> bool {
    matches!(
      self,
      PlaceLineDirection::LeftToRight | PlaceLineDirection::RightToLeft
    )
  }
}

impl TypographyCfg {
  pub fn should_rev_place_glyph(&self) -> bool {
    self.h_align == Some(HAlign::Right) && self.line_dir.is_horizontal()
  }
  pub fn is_rev_place_line(&self) -> bool {
    self.line_dir == PlaceLineDirection::RightToLeft
      || (self.v_align == Some(VAlign::Bottom) && !self.line_dir.is_horizontal())
  }
}

/// Check if a char support apply letter spacing.
fn letter_spacing_char(c: char) -> bool {
  let script = c.script();
  // The list itself is from: https://github.com/harfbuzz/harfbuzz/issues/64
  !matches!(
    script,
    Script::Arabic
      | Script::Syriac
      | Script::Nko
      | Script::Manichaean
      | Script::Psalter_Pahlavi
      | Script::Mandaic
      | Script::Mongolian
      | Script::Phags_Pa
      | Script::Devanagari
      | Script::Bengali
      | Script::Gurmukhi
      | Script::Modi
      | Script::Sharada
      | Script::Syloti_Nagri
      | Script::Tirhuta
      | Script::Ogham
  )
}

// #[cfg(test)]
// mod tests {
//   use super::*;
//   use crate::{shaper::*, FontFace, FontFamily};

//   #[test]
//   fn simple_text_bounds() {
//     let shaper = TextShaper::default();
//     let path = env!("CARGO_MANIFEST_DIR").to_owned() +
// "/../fonts/DejaVuSans.ttf";     let _ =
// shaper.font_db_mut().load_font_file(path);

//     let ids = shaper.font_db().select_all_match(&FontFace {
//       families: Box::new([FontFamily::Name("DejaVu Sans".into())]),
//       ..<_>::default()
//     });

//     let text = "Hello

//     world!";
//     let glyphs = shaper.shape_text(text, &ids);
//     let size = glyphs_box(text, glyphs.as_ref(), 14., None, 1.);
//     assert_eq!(size, Size::new(70.96094, 81.484375));
//   }

//   #[test]
//   fn simple_layout_text() {
//     let shaper = TextShaper::default();
//     let path = env!("CARGO_MANIFEST_DIR").to_owned() +
// "/../fonts/DejaVuSans.ttf";     let _ =
// shaper.font_db_mut().load_font_file(path);

//     let ids = shaper.font_db().select_all_match(&FontFace {
//       families: Box::new([FontFamily::Name("DejaVu Sans".into())]),
//       ..<_>::default()
//     });
//     let text = "Hello--------\nworld!";
//     let glyphs = shaper.shape_text(text, &ids);
//     let mut cfg = LayoutConfig {
//       font_size: 10.,
//       letter_space: 2.,
//       h_align: None,
//       v_align: None,
//       line_height: None,
//     };

//     let layout = |cfg: &LayoutConfig, bounds: Option<Rect<f32>>| {
//       layout_text(text, &glyphs, cfg, bounds)
//         .map(|g| (g.x, g.y))
//         .collect::<Vec<_>>()
//     };

//     let not_bounds = layout(&cfg, None);
//     assert_eq!(
//       &not_bounds,
//       &[
//         (0.0, 0.0),
//         (9.519531, 0.0),
//         (17.671875, 0.0),
//         (22.450195, 0.0),
//         (27.228516, 0.0),
//         (35.532227, 0.0),
//         (41.140625, 0.0),
//         (46.749023, 0.0),
//         (52.35742, 0.0),
//         (57.96582, 0.0),
//         (63.57422, 0.0),
//         (69.18262, 0.0),
//         (74.791016, 0.0),
//         (80.399414, 0.0),
//         // second line
//         (0.0, 11.640625),
//         (10.178711, 11.640625),
//         (18.296875, 11.640625),
//         (24.408203, 11.640625),
//         (29.186523, 11.640625),
//         (37.53418, 11.640625)
//       ]
//     );

//     cfg.h_align = Some(HAlign::Right);
//     let r_align = layout(&cfg, None);
//     assert_eq!(
//       &r_align,
//       &[
//         (80.399414, 0.0),
//         (74.791016, 0.0),
//         (69.18262, 0.0),
//         (63.57422, 0.0),
//         (57.96582, 0.0),
//         (52.35742, 0.0),
//         (46.749023, 0.0),
//         (41.140625, 0.0),
//         (35.532227, 0.0),
//         (27.228516, 0.0),
//         (22.450195, 0.0),
//         (17.671875, 0.0),
//         (9.519531, 0.0),
//         (0.0, 0.0),
//         // second line.
//         (82.3916, 11.640625),
//         (74.043945, 11.640625),
//         (69.265625, 11.640625),
//         (63.154297, 11.640625),
//         (55.036133, 11.640625),
//         (44.85742, 11.640625)
//       ]
//     );

//     cfg.h_align = None;
//     cfg.v_align = Some(VAlign::Bottom);

//     let bottom = layout(&cfg, None);
//     assert_eq!(
//       &bottom,
//       &[
//         // second line
//         (0.0, 11.640625),
//         (10.178711, 11.640625),
//         (18.296875, 11.640625),
//         (24.408203, 11.640625),
//         (29.186523, 11.640625),
//         (37.53418, 11.640625),
//         (0.0, 0.0),
//         // first line
//         (9.519531, 0.0),
//         (17.671875, 0.0),
//         (22.450195, 0.0),
//         (27.228516, 0.0),
//         (35.532227, 0.0),
//         (41.140625, 0.0),
//         (46.749023, 0.0),
//         (52.35742, 0.0),
//         (57.96582, 0.0),
//         (63.57422, 0.0),
//         (69.18262, 0.0),
//         (74.791016, 0.0),
//         (80.399414, 0.0)
//       ]
//     );

//     cfg.h_align = Some(HAlign::Center);
//     cfg.v_align = Some(VAlign::Center);
//     let center_clip = layout(&cfg, Some(Rect::from_size(Size::new(40.,
// 15.))));     assert_eq!(
//       &center_clip,
//       &[
//         // first line
//         (-0.75, -4.140625)  if let Some(letter_space) = letter_space {
//         (17.52539, 7.5),
//         (23.636719, 7.5),
//         (28.41504, 7.5),
//         (36.762695, 7.5)
//       ]
//     );
//   }
// }
