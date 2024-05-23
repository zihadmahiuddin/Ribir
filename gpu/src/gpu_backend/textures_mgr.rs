use std::{
  cmp::Ordering,
  hash::{Hash, Hasher},
  ops::Range,
};

use guillotiere::euclid::SideOffsets2D;
use rayon::{prelude::ParallelIterator, slice::ParallelSlice};
use ribir_algo::Resource;
use ribir_geom::{DevicePoint, DeviceRect, DeviceSize, Point, Transform};
use ribir_painter::{
  image::ColorFormat, PaintPath, Path, PathSegment, PixelImage, Vertex, VertexBuffers,
};

use super::{
  atlas::{Atlas, AtlasConfig, AtlasHandle},
  Texture,
};
use crate::GPUBackendImpl;
const TOLERANCE: f32 = 0.1_f32;
const PAR_CHUNKS_SIZE: usize = 64;

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Copy)]
pub(super) enum TextureID {
  Alpha(usize),
  Rgba(usize),
}

pub(super) struct TexturesMgr<T: Texture> {
  alpha_atlas: Atlas<T, PathKey, f32>,
  rgba_atlas: Atlas<T, Resource<PixelImage>, ()>,
  fill_task: Vec<FillTask>,
  fill_task_buffers: VertexBuffers<()>,
  need_clear_areas: Vec<DeviceRect>,
}

struct FillTask {
  slice: TextureSlice,
  path: Path,
  // transform to construct vertex
  ts: Transform,
  clip_rect: Option<DeviceRect>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct TextureSlice {
  pub(super) tex_id: TextureID,
  pub(super) rect: DeviceRect,
}

macro_rules! id_to_texture_mut {
  ($mgr:ident, $id:expr) => {
    match $id {
      TextureID::Alpha(id) => $mgr.alpha_atlas.get_texture_mut(id),
      TextureID::Rgba(id) => $mgr.rgba_atlas.get_texture_mut(id),
    }
  };
}

macro_rules! id_to_texture {
  ($mgr:ident, $id:expr) => {
    match $id {
      TextureID::Alpha(id) => $mgr.alpha_atlas.get_texture(id),
      TextureID::Rgba(id) => $mgr.rgba_atlas.get_texture(id),
    }
  };
}

fn get_prefer_scale(transform: &Transform, size: DeviceSize, max_size: DeviceSize) -> f32 {
  let Transform { m11, m12, m21, m22, .. } = *transform;
  let scale = (m11.abs() + m12.abs()).max(m21.abs() + m22.abs());

  let dis = size.width.max(size.height) as f32;
  if dis * scale < 32. {
    // If the path is too small, set a minimum tessellation size of 32 pixels.
    32. / dis
  } else {
    // 2 * BLANK_EDGE is the blank edge for each side.
    let max_width = (max_size.width - 2 * BLANK_EDGE) as f32;
    let max_height = (max_size.height - 2 * BLANK_EDGE) as f32;
    let max_scale = (max_width / size.width as f32).min(max_height / size.height as f32);
    scale.min(max_scale)
  }
}

impl<T: Texture> TexturesMgr<T>
where
  T::Host: GPUBackendImpl<Texture = T>,
{
  pub(super) fn new(gpu_impl: &mut T::Host) -> Self {
    let limits = gpu_impl.limits();
    let max_size = limits.texture_size;

    Self {
      alpha_atlas: Atlas::new(
        AtlasConfig::new("Alpha atlas", max_size),
        ColorFormat::Alpha8,
        gpu_impl,
      ),
      rgba_atlas: Atlas::new(
        AtlasConfig::new("Rgba atlas", max_size),
        ColorFormat::Rgba8,
        gpu_impl,
      ),
      fill_task: <_>::default(),
      fill_task_buffers: <_>::default(),
      need_clear_areas: vec![],
    }
  }

  pub(super) fn is_good_for_cache(&self, size: DeviceSize) -> bool {
    self.alpha_atlas.is_good_size_to_alloc(size)
  }

  /// Store an alpha path in texture and return the texture and a transform that
  /// can transform the mask to viewport
  pub(super) fn store_alpha_path(
    &mut self, path: PaintPath, transform: &Transform, gpu_impl: &mut T::Host,
  ) -> (TextureSlice, Transform) {
    fn cache_to_view_matrix(
      path: &Path, path_ts: &Transform, slice_origin: DevicePoint, cache_scale: f32,
    ) -> Transform {
      // scale origin to the cached path
      let aligned_origin = path.bounds().origin * cache_scale;

      // back to slice origin
      Transform::translation(-slice_origin.x as f32, -slice_origin.y as f32)
        // move to cache path axis.
        .then_translate(aligned_origin.to_vector().cast_unit())
        // scale back to path axis.
        .then_scale(1. / cache_scale, 1. / cache_scale)
        // apply path transform matrix to view.
        .then(path_ts)
    }

    let prefer_scale: f32 = get_prefer_scale(
      transform,
      path.bounds().size.to_i32().cast_unit(),
      self.alpha_atlas.max_size(),
    );
    let key = PathKey::from_path(path);

    if let Some(h) = self
      .alpha_atlas
      .get(&key)
      .filter(|h| h.attr >= prefer_scale)
      .copied()
    {
      let mask_slice = alpha_tex_slice(&self.alpha_atlas, &h).cut_blank_edge();
      let matrix = cache_to_view_matrix(key.path(), transform, mask_slice.rect.origin, h.attr);
      (mask_slice.expand_for_paste(), matrix)
    } else {
      let path = key.path().clone();
      let scale_bounds = path.bounds().scale(prefer_scale, prefer_scale);
      let prefer_cache_size = add_blank_edges(scale_bounds.round_out().size.to_i32().cast_unit());

      let h = self
        .alpha_atlas
        .allocate(key, prefer_scale, prefer_cache_size, gpu_impl);
      let slice = alpha_tex_slice(&self.alpha_atlas, &h);
      let mask_slice = slice.cut_blank_edge();

      let matrix = cache_to_view_matrix(&path, transform, mask_slice.rect.origin, prefer_scale);

      let ts = Transform::scale(prefer_scale, prefer_scale)
        .then_translate(-scale_bounds.origin.to_vector().cast_unit())
        .then_translate(
          mask_slice
            .rect
            .origin
            .to_f32()
            .to_vector()
            .cast_unit(),
        );

      self
        .fill_task
        .push(FillTask { slice, path, ts, clip_rect: None });

      (mask_slice.expand_for_paste(), matrix)
    }
  }

  pub(super) fn store_clipped_path(
    &mut self, clip_view: DeviceRect, path: PaintPath, ts: &Transform, gpu_impl: &mut T::Host,
  ) -> (TextureSlice, Transform) {
    let alloc_size: DeviceSize = add_blank_edges(clip_view.size);
    let key = PathKey::from_path_with_clip(path, *ts, clip_view);

    let slice = if let Some(h) = self.alpha_atlas.get(&key).copied() {
      alpha_tex_slice(&self.alpha_atlas, &h).cut_blank_edge()
    } else {
      let path = key.path().clone();
      let h = self
        .alpha_atlas
        .allocate(key, 1., alloc_size, gpu_impl);
      let slice = alpha_tex_slice(&self.alpha_atlas, &h);
      let no_blank_slice = slice.cut_blank_edge();
      let clip_rect = Some(slice.rect);
      let offset = (no_blank_slice.rect.origin - clip_view.origin)
        .to_f32()
        .cast_unit();
      let ts = ts.then_translate(offset);
      let task = FillTask { slice, ts, path, clip_rect };
      self.fill_task.push(task);
      no_blank_slice
    };

    let offset = (clip_view.origin - slice.rect.origin).to_f32();
    (slice.expand_for_paste(), Transform::translation(offset.x, offset.y))
  }

  pub(super) fn store_image(
    &mut self, img: &Resource<PixelImage>, gpu_impl: &mut T::Host,
  ) -> TextureSlice {
    match img.color_format() {
      ColorFormat::Rgba8 => {
        if let Some(h) = self.rgba_atlas.get(img).copied() {
          rgba_tex_slice(&self.rgba_atlas, &h)
        } else {
          let size = DeviceSize::new(img.width() as i32, img.height() as i32);
          let h = self
            .rgba_atlas
            .allocate(img.clone(), (), size, gpu_impl);
          let slice = rgba_tex_slice(&self.rgba_atlas, &h);

          let texture = self.rgba_atlas.get_texture_mut(h.tex_id());
          texture.write_data(&slice.rect, img.pixel_bytes(), gpu_impl);
          slice
        }
      }
      ColorFormat::Alpha8 => todo!(),
    }
  }

  pub(super) fn texture(&self, tex_id: TextureID) -> &T { id_to_texture!(self, tex_id) }

  fn fill_tess(
    path: &Path, ts: &Transform, tex_size: DeviceSize, buffer: &mut VertexBuffers<()>,
    max_size: DeviceSize,
  ) -> Range<u32> {
    let start = buffer.indices.len() as u32;

    let scale = get_prefer_scale(ts, tex_size, max_size);

    path.tessellate(TOLERANCE / scale, buffer, |pos| {
      let pos = ts.transform_point(pos);
      Vertex::new([pos.x, pos.y], ())
    });
    start..buffer.indices.len() as u32
  }

  pub(crate) fn draw_alpha_textures<G: GPUBackendImpl<Texture = T>>(&mut self, gpu_impl: &mut G)
  where
    T: Texture<Host = G>,
  {
    if self.fill_task.is_empty() {
      return;
    }

    if !self.need_clear_areas.is_empty() {
      let tex = self.alpha_atlas.get_texture_mut(0);
      tex.clear_areas(&self.need_clear_areas, gpu_impl);
      self.need_clear_areas.clear();
    }

    self.fill_task.sort_by(|a, b| {
      let a_clip = a.clip_rect.is_some();
      let b_clip = b.clip_rect.is_some();
      if a_clip == b_clip {
        a.slice.tex_id.cmp(&b.slice.tex_id)
      } else if a_clip {
        Ordering::Less
      } else {
        Ordering::Greater
      }
    });

    let mut draw_indices = Vec::with_capacity(self.fill_task.len());
    if self.fill_task.len() < PAR_CHUNKS_SIZE {
      for f in self.fill_task.iter() {
        let FillTask { slice, path, clip_rect, ts } = f;
        let texture = id_to_texture!(self, slice.tex_id);

        let rg = Self::fill_tess(
          path,
          ts,
          texture.size(),
          &mut self.fill_task_buffers,
          self.alpha_atlas.max_size(),
        );
        draw_indices.push((slice.tex_id, rg, clip_rect));
      }
    } else {
      let mut tasks = Vec::with_capacity(self.fill_task.len());
      for f in self.fill_task.iter() {
        let FillTask { slice, path, clip_rect, ts } = f;
        let texture = id_to_texture!(self, slice.tex_id);
        tasks.push((slice, ts, texture.size(), path, clip_rect));
      }
      let max_size = self.alpha_atlas.max_size();
      let par_tess_res = tasks
        .par_chunks(PAR_CHUNKS_SIZE)
        .map(|tasks| {
          let mut buffer = VertexBuffers::default();
          let mut indices = Vec::with_capacity(tasks.len());
          for (slice, ts, tex_size, path, clip_rect) in tasks.iter() {
            let rg = Self::fill_tess(path, ts, *tex_size, &mut buffer, max_size);
            indices.push((slice.tex_id, rg, *clip_rect));
          }
          (indices, buffer)
        })
        .collect::<Vec<_>>();

      par_tess_res
        .into_iter()
        .for_each(|(indices, buffer)| {
          let offset = self.fill_task_buffers.indices.len() as u32;
          draw_indices.extend(indices.into_iter().map(|(id, mut rg, clip)| {
            rg.start += offset;
            rg.end += offset;
            (id, rg, clip)
          }));
          extend_buffer(&mut self.fill_task_buffers, buffer);
        })
    };

    gpu_impl.load_alpha_vertices(&self.fill_task_buffers);

    let mut idx = 0;
    loop {
      if idx >= draw_indices.len() {
        break;
      }

      let (tex_id, rg, Some(clip_rect)) = &draw_indices[idx] else {
        break;
      };
      let texture = id_to_texture_mut!(self, *tex_id);
      gpu_impl.draw_alpha_triangles_with_scissor(rg, texture, *clip_rect);
      idx += 1;
    }

    loop {
      if idx >= draw_indices.len() {
        break;
      }
      let (tex_id, rg, None) = &draw_indices[idx] else {
        unreachable!();
      };
      let next = draw_indices[idx..]
        .iter()
        .position(|(next, _, _)| tex_id != next);

      let indices = if let Some(mut next) = next {
        next += idx;
        idx = next;
        let (_, end, _) = &draw_indices[next];
        rg.start..end.start
      } else {
        idx = draw_indices.len();
        rg.start..self.fill_task_buffers.indices.len() as u32
      };

      let texture = id_to_texture_mut!(self, *tex_id);
      gpu_impl.draw_alpha_triangles(&indices, texture);
    }

    self.fill_task.clear();
    self.fill_task_buffers.vertices.clear();
    self.fill_task_buffers.indices.clear();
  }

  pub(crate) fn end_frame(&mut self) {
    self.alpha_atlas.end_frame_with(|rect| {
      self.need_clear_areas.push(rect);
    });
    self.rgba_atlas.end_frame();
  }
}

fn alpha_tex_slice<T, K>(atlas: &Atlas<T, K, f32>, h: &AtlasHandle<f32>) -> TextureSlice
where
  T: Texture,
{
  TextureSlice { tex_id: TextureID::Alpha(h.tex_id()), rect: h.tex_rect(atlas) }
}

fn rgba_tex_slice<T, K>(atlas: &Atlas<T, K, ()>, h: &AtlasHandle<()>) -> TextureSlice
where
  T: Texture,
{
  TextureSlice { tex_id: TextureID::Rgba(h.tex_id()), rect: h.tex_rect(atlas) }
}

fn extend_buffer<V>(dist: &mut VertexBuffers<V>, from: VertexBuffers<V>) {
  if dist.vertices.is_empty() {
    dist.vertices.extend(from.vertices);
    dist.indices.extend(from.indices);
  } else {
    let offset = dist.vertices.len() as u32;
    dist
      .indices
      .extend(from.indices.into_iter().map(|i| offset + i));
    dist.vertices.extend(from.vertices);
  }
}

const BLANK_EDGE: i32 = 2;

fn add_blank_edges(mut size: DeviceSize) -> DeviceSize {
  size.width += BLANK_EDGE * 2;
  size.height += BLANK_EDGE * 2;
  size
}

impl TextureSlice {
  pub fn cut_blank_edge(mut self) -> TextureSlice {
    let blank_side = SideOffsets2D::new_all_same(BLANK_EDGE);
    self.rect = self.rect.inner_rect(blank_side);
    self
  }

  pub fn expand_for_paste(mut self) -> TextureSlice {
    const EXPANDED_EDGE: i32 = 1;
    let blank_side = SideOffsets2D::new_all_same(EXPANDED_EDGE);
    self.rect = self.rect.outer_rect(blank_side);
    self
  }
}

#[derive(Debug, Clone)]
enum PathKey {
  Path { path: PaintPath, hash: u64 },
  PathWithClip { path: PaintPath, ts: Transform, hash: u64, clip_rect: DeviceRect },
}

fn pos_100_device(pos: Point) -> DevicePoint {
  Point::new(pos.x * 100., pos.y * 100.)
    .to_i32()
    .cast_unit()
}

fn path_inner_pos(pos: Point, path: &Path) -> DevicePoint {
  // Path pan to origin for comparison
  let pos = pos - path.bounds().origin;
  pos_100_device(pos.to_point())
}

fn path_hash(path: &Path, pos_adjust: impl Fn(Point) -> DevicePoint) -> u64 {
  let mut state = ahash::AHasher::default();

  for s in path.segments() {
    // core::mem::discriminant(&s).hash(&mut state);
    match s {
      PathSegment::MoveTo(to) | PathSegment::LineTo(to) => {
        pos_adjust(to).hash(&mut state);
      }
      PathSegment::QuadTo { ctrl, to } => {
        pos_adjust(ctrl).hash(&mut state);
        pos_adjust(to).hash(&mut state);
      }
      PathSegment::CubicTo { to, ctrl1, ctrl2 } => {
        pos_adjust(to).hash(&mut state);
        pos_adjust(ctrl1).hash(&mut state);
        pos_adjust(ctrl2).hash(&mut state);
      }
      PathSegment::Close(b) => b.hash(&mut state),
    };
  }

  state.finish()
}

fn path_eq(a: &Path, b: &Path, pos_adjust: impl Fn(Point, &Path) -> DevicePoint) -> bool {
  let a_adjust = |pos| pos_adjust(pos, a);
  let b_adjust = |pos| pos_adjust(pos, b);

  a.segments()
    .zip(b.segments())
    .all(|(a, b)| match (a, b) {
      (PathSegment::MoveTo(a), PathSegment::MoveTo(b))
      | (PathSegment::LineTo(a), PathSegment::LineTo(b)) => a_adjust(a) == b_adjust(b),
      (PathSegment::QuadTo { ctrl, to }, PathSegment::QuadTo { ctrl: ctrl_b, to: to_b }) => {
        a_adjust(ctrl) == b_adjust(ctrl_b) && a_adjust(to) == b_adjust(to_b)
      }
      (
        PathSegment::CubicTo { to, ctrl1, ctrl2 },
        PathSegment::CubicTo { to: to_b, ctrl1: ctrl1_b, ctrl2: ctrl2_b },
      ) => {
        a_adjust(to) == b_adjust(to_b)
          && a_adjust(ctrl1) == b_adjust(ctrl1_b)
          && a_adjust(ctrl2) == b_adjust(ctrl2_b)
      }
      (PathSegment::Close(a), PathSegment::Close(b)) => a == b,
      _ => false,
    })
}

impl PathKey {
  fn from_path(value: PaintPath) -> Self {
    let hash = path_hash(&value, |pos| path_inner_pos(pos, &value));
    PathKey::Path { path: value, hash }
  }

  fn from_path_with_clip(path: PaintPath, ts: Transform, clip_rect: DeviceRect) -> Self {
    let hash = path_hash(&path, pos_100_device);
    PathKey::PathWithClip { path, hash, ts, clip_rect }
  }

  fn path(&self) -> &Path {
    match self {
      PathKey::Path { path, .. } => path,
      PathKey::PathWithClip { path, .. } => path,
    }
  }
}

impl Hash for PathKey {
  fn hash<H: Hasher>(&self, state: &mut H) {
    match self {
      PathKey::Path { hash, .. } => hash.hash(state),
      PathKey::PathWithClip { hash, clip_rect, .. } => {
        clip_rect.hash(state);
        hash.hash(state)
      }
    }
  }
}

impl PartialEq for PathKey {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (PathKey::Path { path: a, .. }, PathKey::Path { path: b, .. }) => {
        path_eq(a, b, path_inner_pos)
      }
      (
        PathKey::PathWithClip { path: p_a, ts: t_a, hash: h_a, clip_rect: r_a },
        PathKey::PathWithClip { path: p_b, ts: t_b, hash: h_b, clip_rect: r_b },
      ) => h_a == h_b && r_a == r_b && t_a == t_b && path_eq(p_a, p_b, |p, _| pos_100_device(p)),
      _ => false,
    }
  }
}

impl Eq for PathKey {}

#[cfg(feature = "wgpu")]
#[cfg(test)]
pub mod tests {
  use std::borrow::Cow;

  use futures::executor::block_on;
  use ribir_geom::*;
  use ribir_painter::Color;

  use super::*;
  use crate::{WgpuImpl, WgpuTexture};

  pub fn color_image(color: Color, width: u32, height: u32) -> Resource<PixelImage> {
    let data = std::iter::repeat(color.into_components())
      .take(width as usize * height as usize)
      .flatten()
      .collect::<Vec<_>>();

    let img = PixelImage::new(Cow::Owned(data), width, height, ColorFormat::Rgba8);
    Resource::new(img)
  }

  #[test]
  fn smoke_store_image() {
    let mut wgpu = block_on(WgpuImpl::headless());
    let mut mgr = TexturesMgr::new(&mut wgpu);

    let red_img = color_image(Color::RED, 32, 32);
    let red_rect = mgr.store_image(&red_img, &mut wgpu);

    assert_eq!(red_rect.rect.min().to_array(), [0, 0]);

    // same image should have same position in atlas
    assert_eq!(red_rect, mgr.store_image(&red_img, &mut wgpu));
    color_img_check(&mgr, &red_rect, &mut wgpu, Color::RED);

    let yellow_img = color_image(Color::YELLOW, 64, 64);
    let yellow_rect = mgr.store_image(&yellow_img, &mut wgpu);

    // the color should keep after atlas rearrange
    color_img_check(&mgr, &red_rect, &mut wgpu, Color::RED);
    color_img_check(&mgr, &yellow_rect, &mut wgpu, Color::YELLOW);

    let extra_blue_img = color_image(Color::BLUE, 1024, 1024);
    let blue_rect = mgr.store_image(&extra_blue_img, &mut wgpu);

    color_img_check(&mgr, &blue_rect, &mut wgpu, Color::BLUE);
    color_img_check(&mgr, &red_rect, &mut wgpu, Color::RED);
    color_img_check(&mgr, &yellow_rect, &mut wgpu, Color::YELLOW);
  }

  fn color_img_check(
    mgr: &TexturesMgr<WgpuTexture>, rect: &TextureSlice, wgpu: &mut WgpuImpl, color: Color,
  ) {
    wgpu.begin_frame();
    let texture = mgr.texture(rect.tex_id);
    let img = texture.copy_as_image(&rect.rect, wgpu);
    wgpu.end_frame();

    let img = block_on(img).unwrap();
    assert!(
      img
        .pixel_bytes()
        .chunks(4)
        .all(|c| c == color.into_components())
    );
  }

  #[test]
  fn transform_path_share_cache() {
    let mut wgpu = block_on(WgpuImpl::headless());
    let mut mgr = TexturesMgr::<WgpuTexture>::new(&mut wgpu);

    let path1 = PaintPath::Own(Path::rect(&rect(0., 0., 300., 300.)));
    let path2 = PaintPath::Own(Path::rect(&rect(100., 100., 300., 300.)));
    let ts = Transform::scale(2., 2.);

    let (slice1, ts1) = mgr.store_alpha_path(path1, &ts, &mut wgpu);
    let (slice2, ts2) = mgr.store_alpha_path(path2, &Transform::identity(), &mut wgpu);
    assert_eq!(slice1, slice2);

    assert_eq!(ts1, Transform::new(1., 0., 0., 1., -2., -2.));
    assert_eq!(ts2, Transform::new(0.5, 0., 0., 0.5, 99., 99.));
  }

  #[test]
  fn store_clipped_path() {
    let mut wgpu = block_on(WgpuImpl::headless());
    let mut mgr = TexturesMgr::<WgpuTexture>::new(&mut wgpu);

    let path = PaintPath::Own(Path::rect(&rect(20., 20., 300., 300.)));
    let ts = Transform::new(2., 0., 0., 2., -10., -10.);
    let clip_view = ribir_geom::rect(10, 10, 100, 100);

    let (slice1, ts1) = mgr.store_clipped_path(clip_view, path.clone(), &ts, &mut wgpu);
    let (slice2, ts2) = mgr.store_clipped_path(clip_view, path, &ts, &mut wgpu);
    assert_eq!(slice1, slice2);
    assert_eq!(ts1, ts2);
    assert_eq!(slice1.rect, ribir_geom::rect(1, 1, 102, 102));
    assert_eq!(ts1, Transform::new(1., 0., 0., 1., 8., 8.));
  }

  #[test]
  fn fix_resource_address_conflict() {
    // because the next resource may allocate at same address of a deallocated
    // address.

    let mut wgpu = block_on(WgpuImpl::headless());
    let mut mgr = TexturesMgr::<WgpuTexture>::new(&mut wgpu);
    {
      let red_img = color_image(Color::RED, 32, 32);
      mgr.store_image(&red_img, &mut wgpu);
    }

    for _ in 0..10 {
      mgr.end_frame();
      let red_img = color_image(Color::RED, 32, 32);
      assert!(mgr.rgba_atlas.get(&red_img).is_none());
    }
  }
}
