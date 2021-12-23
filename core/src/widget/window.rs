use super::{render_tree::*, PaintingContext};
use crate::{
  prelude::*,
  widget::{events::dispatcher::Dispatcher, widget_tree::*},
};
use canvas::{surface::TextureSurface, Canvas, CanvasRender, DeviceSize, WgpuRender};
use std::{cell::RefCell, pin::Pin, ptr::NonNull, rc::Rc};
pub use winit::window::CursorIcon;
use winit::{event::WindowEvent, event_loop::EventLoop, window::WindowBuilder, window::WindowId};

pub trait RawWindow {
  fn inner_size(&self) -> Size;
  fn outer_size(&self) -> Size;
  fn inner_position(&self) -> Point;
  fn outer_position(&self) -> Point;
  fn id(&self) -> WindowId;
  /// Modifies the cursor icon of the window. Not effective immediately.
  fn set_cursor(&mut self, cursor: CursorIcon);
  /// The cursor set to the window, but not submit to native window yet.
  fn updated_cursor(&self) -> Option<CursorIcon>;
  fn request_redraw(&self);
  /// Modify the native window if cursor modified.
  fn submit_cursor(&mut self);
  fn scale_factor(&self) -> f64;
}

pub struct NativeWindow {
  native: winit::window::Window,
  cursor: Option<CursorIcon>,
}

impl RawWindow for NativeWindow {
  fn inner_size(&self) -> Size {
    let wnd = &self.native;
    let size = wnd.inner_size().to_logical(wnd.scale_factor());
    Size::new(size.width, size.height)
  }

  fn outer_size(&self) -> Size {
    let wnd = &self.native;
    let size = wnd.outer_size().to_logical(wnd.scale_factor());
    Size::new(size.width, size.height)
  }

  fn inner_position(&self) -> Point {
    let wnd = &self.native;
    let pos = wnd
      .inner_position()
      .expect(" Can only be called on the main thread")
      .to_logical(wnd.scale_factor());

    Point::new(pos.x, pos.y)
  }
  #[inline]
  fn id(&self) -> WindowId { self.native.id() }

  fn outer_position(&self) -> Point {
    let wnd = &self.native;
    let pos = wnd
      .outer_position()
      .expect(" Can only be called on the main thread")
      .to_logical(wnd.scale_factor());
    Point::new(pos.x, pos.y)
  }

  #[inline]
  fn set_cursor(&mut self, cursor: CursorIcon) { self.cursor = Some(cursor) }

  #[inline]
  fn updated_cursor(&self) -> Option<CursorIcon> { self.cursor }

  #[inline]
  fn request_redraw(&self) { self.native.request_redraw() }

  fn submit_cursor(&mut self) {
    if let Some(cursor) = self.cursor.take() {
      self.native.set_cursor_icon(cursor)
    }
  }

  #[inline]
  fn scale_factor(&self) -> f64 { self.native.scale_factor() }
}

/// Window is the root to represent.
pub struct Window<R: CanvasRender = WgpuRender> {
  pub raw_window: Rc<RefCell<Box<dyn RawWindow>>>,
  render_tree: Pin<Box<RenderTree>>,
  widget_tree: Pin<Box<WidgetTree>>,
  canvas: Pin<Box<Canvas>>,
  render: R,
  pub(crate) dispatcher: Dispatcher,
}

impl<R: CanvasRender> Window<R> {
  /// processes native events from this native window
  #[inline]
  pub fn processes_native_event(&mut self, event: WindowEvent) {
    match event {
      WindowEvent::Resized(size) => {
        self.resize(DeviceSize::new(size.width, size.height));
      }
      WindowEvent::ScaleFactorChanged { new_inner_size, scale_factor } => {
        self.resize(DeviceSize::new(new_inner_size.width, new_inner_size.height));
        let factor = scale_factor as f32;
        self
          .canvas
          .set_default_transform(Transform::new(factor, 0., 0., factor, 0., 0.));
      }
      event => self.dispatcher.dispatch(event),
    };
    self.raw_window.borrow_mut().submit_cursor();
  }

  /// This method ensure render tree is ready to paint, three things it's have
  /// to do:
  /// 1. every need rebuild widgets has rebuild and correspond render tree
  /// construct.
  /// 2. every dirty widget has flush to render tree so render tree's data
  /// represent the latest application state.
  /// 3. every render objet need layout has done, so every render object is in
  /// the correct position.
  pub fn render_ready(&mut self) -> bool {
    unsafe { self.widget_tree.as_mut().get_unchecked_mut() }.notify_state_change_until_empty();
    let mut changed = self.tree_repair();
    changed = self.layout() || changed;
    if changed {
      self.dispatcher.focus_mgr.update(&self.dispatcher.common);
    }

    changed
  }

  /// Draw an image what current render tree represent.
  pub(crate) fn draw_frame(&mut self) {
    if let Some(layer) =
      PaintingContext::new(&self.render_tree, self.canvas.default_transform()).map(|ctx| ctx.draw())
    {
      let mut frame = self.canvas.next_frame(&mut self.render);
      frame.compose_2d_layer(layer);
    }
  }

  /// Repair the gaps between widget tree represent and current data state after
  /// some user or device inputs has been processed. The render tree will also
  /// react widget tree's change.
  fn tree_repair(&mut self) -> bool {
    unsafe {
      self
        .widget_tree
        .as_mut()
        .get_unchecked_mut()
        .repair(self.render_tree.as_mut().get_unchecked_mut())
    }
  }

  /// Layout the render tree as needed
  fn layout(&mut self) -> bool {
    unsafe {
      self
        .render_tree
        .as_mut()
        .get_unchecked_mut()
        .layout(self.raw_window.borrow().inner_size(), self.canvas.as_mut())
    }
  }

  fn new<W: RawWindow + 'static>(root: BoxedWidget, wnd: W, canvas: Canvas, render: R) -> Self {
    let render_tree = Box::pin(RenderTree::default());
    let widget_tree = Box::pin(WidgetTree::default());
    let raw_window: Rc<RefCell<Box<dyn RawWindow>>> = Rc::new(RefCell::new(Box::new(wnd)));
    let mut wnd = Self {
      dispatcher: Dispatcher::new(
        NonNull::from(&*render_tree),
        NonNull::from(&*widget_tree),
        raw_window.clone(),
      ),
      raw_window,
      render_tree,
      widget_tree,
      canvas: Box::pin(canvas),
      render,
    };

    unsafe {
      wnd
        .widget_tree
        .as_mut()
        .get_unchecked_mut()
        .set_root(root, wnd.render_tree.as_mut().get_unchecked_mut());
    }
    let focus_mgr = &mut wnd.dispatcher.focus_mgr;
    focus_mgr.update(&wnd.dispatcher.common);
    if let Some(auto_focusing) = focus_mgr.auto_focus(&wnd.widget_tree) {
      focus_mgr.focus(auto_focusing, &wnd.dispatcher.common)
    }

    wnd
  }

  fn resize(&mut self, size: DeviceSize) {
    let r_tree = unsafe { self.render_tree.as_mut().get_unchecked_mut() };
    if let Some(root) = r_tree.root() {
      root.mark_needs_layout(r_tree);
    }
    self.render.resize(size);
    self.raw_window.borrow().request_redraw();
  }

  pub fn render_tree(&mut self) -> Pin<&mut RenderTree> { self.render_tree.as_mut() }

  pub fn widget_tree(&mut self) -> Pin<&mut WidgetTree> { self.widget_tree.as_mut() }

  #[cfg(test)]
  pub fn canvas(&mut self) -> Pin<&mut Canvas> { self.canvas.as_mut() }

  #[cfg(test)]
  pub fn render(&mut self) -> &mut R { &mut self.render }
}

impl Window {
  pub(crate) fn from_event_loop(root: BoxedWidget, event_loop: &EventLoop<()>) -> Self {
    let native_window = WindowBuilder::new().build(event_loop).unwrap();
    let size = native_window.inner_size();
    let (mut canvas, render) =
      futures::executor::block_on(canvas::create_canvas_with_render_from_wnd(
        &native_window,
        DeviceSize::new(size.width, size.height),
      ));
    let factor = native_window.scale_factor() as f32;
    canvas.set_default_transform(Transform::new(factor, 0., 0., factor, 0., 0.));

    Self::new(
      root,
      NativeWindow { native: native_window, cursor: None },
      canvas,
      render,
    )
  }

  /// Emits a `WindowEvent::RedrawRequested` event in the associated event loop
  /// after all OS events have been processed by the event loop.
  #[inline]
  pub(crate) fn request_redraw(&self) { self.raw_window.borrow().request_redraw(); }
}

pub type HeadlessWindow = Window<WgpuRender<TextureSurface>>;
pub type NoRenderWindow = Window<MockRender>;

pub struct MockRender;

#[derive(Default)]
pub struct MockRawWindow {
  pub size: Size,
  pub cursor: Option<CursorIcon>,
}

impl CanvasRender for MockRender {
  fn draw(
    &mut self,
    _: &canvas::RenderData,
    _: &mut canvas::MemTexture<u8>,
    _: &mut canvas::MemTexture<u32>,
  ) {
  }

  fn resize(&mut self, _: DeviceSize) {}
}

impl RawWindow for MockRawWindow {
  fn inner_size(&self) -> Size { self.size }
  fn outer_size(&self) -> Size { self.size }
  fn inner_position(&self) -> Point { Point::zero() }
  fn outer_position(&self) -> Point { Point::zero() }
  fn id(&self) -> WindowId { unsafe { WindowId::dummy() } }
  fn set_cursor(&mut self, cursor: CursorIcon) { self.cursor = Some(cursor); }
  fn request_redraw(&self) {}
  fn updated_cursor(&self) -> Option<CursorIcon> { self.cursor }
  fn submit_cursor(&mut self) { self.cursor.take(); }
  fn scale_factor(&self) -> f64 { 1. }
}

impl HeadlessWindow {
  pub fn headless(root: BoxedWidget, size: DeviceSize) -> Self {
    let (canvas, render) =
      futures::executor::block_on(canvas::create_canvas_with_render_headless(size));
    Self::new(
      root,
      MockRawWindow {
        size: Size::from_untyped(size.to_f32().to_untyped()),
        ..Default::default()
      },
      canvas,
      render,
    )
  }
}

impl NoRenderWindow {
  pub fn without_render(root: BoxedWidget, size: Size) -> Self {
    let canvas = Canvas::new(None);
    let render = MockRender;
    Self::new(
      root,
      MockRawWindow { size, ..Default::default() },
      canvas,
      render,
    )
  }
}
