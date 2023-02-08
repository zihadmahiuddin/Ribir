use crate::{
  builtin_widgets::key::AnyKey,
  impl_proxy_query, impl_query_self_only,
  prelude::*,
  widget::widget_id::{empty_node, split_arena},
};
use std::{cell::RefCell, collections::HashMap};

/// the information of a widget generated by `DynWidget`.
pub(crate) enum DynWidgetGenInfo {
  /// DynWidget generate single result, and have static children. The depth
  /// describe the distance from first dynamic widget (self) to the static
  /// child.
  DynDepth(usize),
  /// `DynWidget` without static children, and the whole subtree of generated
  /// widget are dynamic widgets. The value record how many dynamic siblings
  /// have.
  WholeSubtree { width: usize, directly_spread: bool },
}

/// Widget that as a container of dynamic widgets

#[derive(Declare)]
pub struct DynWidget<D> {
  #[declare(convert=custom)]
  pub(crate) dyns: Option<D>,
}

impl<D> DynWidgetDeclarer<D> {
  pub fn dyns(mut self, d: D) -> Self {
    self.dyns = Some(Some(d));
    self
  }
}

#[inline]
pub const fn identify<V>(v: V) -> V { v }

impl<D> DynWidget<D> {
  pub fn set_declare_dyns(&mut self, dyns: D) { self.dyns = Some(dyns); }

  pub(crate) fn into_inner(mut self) -> D {
    self
      .dyns
      .take()
      .unwrap_or_else(|| unreachable!("stateless `DynWidget` must be initialized."))
  }
}

/// Widget help to limit which `DynWidget` can be a parent widget and which can
/// be a child.
pub(crate) struct DynRender<D, M> {
  dyn_widgets: Stateful<DynWidget<D>>,
  self_render: RefCell<Box<dyn Render>>,
  gen_info: RefCell<Option<DynWidgetGenInfo>>,
  marker: PhantomData<fn(M)>,
}

pub(crate) trait DynsIntoWidget<M> {
  fn dyns_into_widget(self) -> Vec<Widget>;
}

// A dynamic widget must be stateful, depends others.
impl<D: DynsIntoWidget<M> + 'static, M: 'static> Render for DynRender<D, M> {
  fn perform_layout(&self, clamp: BoxClamp, ctx: &mut LayoutCtx) -> Size {
    if let Some(width) = self.take_spread_cnt() {
      let size = self.self_render.perform_layout(clamp, ctx);

      let arena = &ctx.arena;
      let mut sibling = Some(ctx.id);
      (0..width).for_each(|_| {
        let w = sibling.unwrap();
        inspect_key(&w, arena, |key: &dyn AnyKey| {
          key.mounted();
        });
        w.on_mounted(arena, ctx.store, ctx.wnd_ctx, ctx.dirty_set);
        sibling = w.next_sibling(arena);
      });

      size
    } else {
      self.regen_if_need(ctx);
      self.self_render.perform_layout(clamp, ctx)
    }
  }

  fn paint(&self, ctx: &mut PaintingCtx) { self.self_render.paint(ctx) }

  fn only_sized_by_parent(&self) -> bool {
    // Dyn widget effect the children of its parent. Even if its self render is only
    // sized by parent, but itself effect siblings, sibling effect parent, means
    // itself not only sized by parent but also its sibling.
    false
  }

  fn hit_test(&self, ctx: &HitTestCtx, pos: Point) -> HitTest {
    self.self_render.hit_test(ctx, pos)
  }

  fn can_overflow(&self) -> bool { self.self_render.can_overflow() }

  fn get_transform(&self) -> Option<Transform> { self.self_render.get_transform() }
}

impl<D: DynsIntoWidget<M>, M> DynRender<D, M> {
  pub(crate) fn new(dyns: Stateful<DynWidget<D>>) -> Self {
    Self {
      dyn_widgets: dyns,
      self_render: RefCell::new(Box::new(Void)),
      gen_info: <_>::default(),
      marker: PhantomData,
    }
  }

  pub(crate) fn spread(dyns: Stateful<DynWidget<D>>) -> Vec<Widget>
  where
    M: 'static,
    D: 'static,
  {
    let mut widgets = dyns.silent_ref().dyns.take().unwrap().dyns_into_widget();

    if widgets.is_empty() {
      widgets.push(Void.into_widget());
    }

    let first = std::mem::replace(&mut widgets[0], Void.into_widget());
    let first = DynRender::new(Stateful::new(DynWidget { dyns: Some(first) }));
    let first = Self {
      dyn_widgets: dyns,
      self_render: RefCell::new(Box::new(first)),
      gen_info: RefCell::new(Some(DynWidgetGenInfo::WholeSubtree {
        width: widgets.len(),
        directly_spread: true,
      })),
      marker: PhantomData,
    };

    widgets[0] = first.into_widget();

    widgets
  }

  fn regen_if_need(&self, ctx: &mut LayoutCtx) {
    let mut dyn_widget = self.dyn_widgets.silent_ref();
    let Some(new_widgets) = dyn_widget.dyns.take() else {
      return
    };

    let mut gen_info = self.gen_info.borrow_mut();
    let mut gen_info = gen_info.get_or_insert_with(|| {
      if ctx.has_child() {
        DynWidgetGenInfo::DynDepth(1)
      } else {
        DynWidgetGenInfo::WholeSubtree { width: 1, directly_spread: false }
      }
    });

    let LayoutCtx {
      id: sign,
      arena,
      store,
      wnd_ctx,
      dirty_set,
    } = ctx;

    let mut new_widgets = new_widgets
      .dyns_into_widget()
      .into_iter()
      .filter_map(|w| w.into_subtree(None, arena, wnd_ctx))
      .collect::<Vec<_>>();
    if new_widgets.is_empty() {
      new_widgets.push(empty_node(arena));
    }

    // Place the real old render in node, the dyn render in node keep.
    std::mem::swap(
      &mut *self.self_render.borrow_mut(),
      sign.assert_get_mut(arena),
    );

    // swap the new sign and old, so we can always keep the sign id not change.
    sign.swap_id(new_widgets[0], arena);
    let old_sign = new_widgets[0];
    new_widgets[0] = *sign;

    match &mut gen_info {
      DynWidgetGenInfo::DynDepth(depth) => {
        assert_eq!(new_widgets.len(), 1);

        let declare_child_parent = single_down(old_sign, arena, *depth as isize - 1);
        let (new_leaf, down_level) = down_to_leaf(*sign, arena);

        let new_depth = down_level + 1;
        if let Some(declare_child_parent) = declare_child_parent {
          // Safety: control two subtree not intersect.
          let (arena1, arena2) = unsafe { split_arena(arena) };
          declare_child_parent
            .children(arena1)
            .for_each(|c| new_leaf.append(c, arena2));
        }

        let mut old_key = None;
        inspect_key(&old_sign, arena, |key: &dyn AnyKey| {
          old_key = Some((key.key(), old_sign));
        });

        old_sign.insert_after(*sign, arena);
        old_sign.remove_subtree(arena, store, wnd_ctx);

        let w = *sign;

        if let Some(old) = &old_key {
          inspect_key(&w, arena, |key: &dyn AnyKey| {
            if old.0 == key.key() {
              inspect_key(&old.1, arena, |old_key_widget: &dyn AnyKey| {
                key.record_before_value(old_key_widget)
              });
            } else {
              inspect_key(&old.1, arena, |old_key_widget: &dyn AnyKey| {
                old_key_widget.disposed()
              });
              key.mounted();
            }
          });
        }

        loop {
          w.on_mounted(arena, store, wnd_ctx, dirty_set);
          if w == new_leaf {
            break;
          }
          w.single_child(arena).unwrap();
        }

        *depth = new_depth;
      }

      DynWidgetGenInfo::WholeSubtree { width: siblings, .. } => {
        let mut cursor = old_sign;
        new_widgets.iter().rev().for_each(|n| {
          cursor.insert_before(*n, arena);
          cursor = *n;
        });

        let mut old_key_list = HashMap::new();
        let mut remove = Some(old_sign);

        (0..*siblings).for_each(|_| {
          let o = remove.unwrap();

          inspect_key(&o, arena, |old_key_widget: &dyn AnyKey| {
            old_key_list.insert(old_key_widget.key(), o);
          });

          remove = o.next_sibling(arena);
        });

        new_widgets.iter().for_each(|n| {
          inspect_key(n, arena, |new_key_widget: &dyn AnyKey| {
            let key = &new_key_widget.key();
            if let Some(old_key_widget) = old_key_list.get(key) {
              inspect_key(old_key_widget, arena, |old_key_widget: &dyn AnyKey| {
                new_key_widget.record_before_value(old_key_widget);
              });
              old_key_list.remove(key);
            } else {
              new_key_widget.mounted();
            }
          });
        });

        if !old_key_list.is_empty() {
          old_key_list.iter().for_each(|old_key| {
            inspect_key(old_key.1, arena, |old_key_widget| old_key_widget.disposed())
          });
        }

        let mut remove = Some(old_sign);
        (0..*siblings).for_each(|_| {
          let o = remove.unwrap();
          remove = o.next_sibling(arena);
          o.remove_subtree(arena, store, wnd_ctx);
        });

        new_widgets
          .iter()
          .for_each(|n| n.on_mounted_subtree(arena, store, wnd_ctx, dirty_set));
        *siblings = new_widgets.len();
      }
    };
    // Place the dynRender back in node.
    std::mem::swap(
      &mut *self.self_render.borrow_mut(),
      sign.assert_get_mut(arena),
    );
  }

  fn take_spread_cnt(&self) -> Option<usize> {
    if let Some(DynWidgetGenInfo::WholeSubtree { directly_spread, width }) =
      &mut *self.gen_info.borrow_mut()
    {
      if *directly_spread {
        *directly_spread = false;
        return Some(*width);
      }
    }
    None
  }
}

pub(crate) struct SingleDyn<M>(M);

impl<D, M> DynsIntoWidget<SingleDyn<M>> for D
where
  M: ImplMarker,
  D: IntoWidget<M> + 'static,
{
  fn dyns_into_widget(self) -> Vec<Widget> { vec![self.into_widget()] }
}

impl<D, M> DynsIntoWidget<SingleDyn<Option<M>>> for Option<D>
where
  M: ImplMarker,
  D: IntoWidget<M> + 'static,
{
  fn dyns_into_widget(self) -> Vec<Widget> {
    if let Some(w) = self {
      vec![w.into_widget()]
    } else {
      vec![]
    }
  }
}

impl<D, M> DynsIntoWidget<&dyn Iterator<Item = M>> for D
where
  M: ImplMarker,
  D: IntoIterator,
  D::Item: IntoWidget<M> + 'static,
{
  fn dyns_into_widget(self) -> Vec<Widget> {
    self.into_iter().map(IntoWidget::into_widget).collect()
  }
}

impl<D: 'static, M: 'static> Query for DynRender<D, M> {
  impl_proxy_query!(self.self_render, self.dyn_widgets);
}

impl<D: 'static> Query for DynWidget<D> {
  impl_query_self_only!();
}

fn inspect_key(id: &WidgetId, tree: &TreeArena, mut cb: impl FnMut(&dyn AnyKey)) {
  #[allow(clippy::borrowed_box)]
  id.assert_get(tree).query_on_first_type(
    QueryOrder::OutsideFirst,
    |key_widget: &Box<dyn AnyKey>| {
      cb(&**key_widget);
    },
  );
}

fn single_down(id: WidgetId, arena: &TreeArena, mut down_level: isize) -> Option<WidgetId> {
  let mut res = Some(id);
  while down_level > 0 {
    down_level -= 1;
    res = res.unwrap().single_child(arena);
  }
  res
}

fn down_to_leaf(id: WidgetId, arena: &TreeArena) -> (WidgetId, usize) {
  let mut leaf = id;
  let mut depth = 0;
  while let Some(c) = leaf.single_child(arena) {
    leaf = c;
    depth += 1;
  }
  (leaf, depth)
}

// impl IntoWidget

// only `DynWidget` gen single widget can as a parent widget
impl<M, D> IntoWidget<NotSelf<M>> for Stateful<DynWidget<D>>
where
  M: ImplMarker + 'static,
  D: IntoWidget<M> + 'static,
{
  #[inline]
  fn into_widget(self) -> Widget { DynRender::new(self).into_widget() }
}

/// only use to avoid conflict implement for `IntoIterator`.
/// `Stateful<DynWidget<Option<W: IntoWidget>>>` both satisfied `IntoWidget` as
/// a single child and `Stateful<DynWidget<impl IntoIterator<Item= impl
/// IntoWidget>>>` as multi child.
pub struct OptionDyn<M>(PhantomData<fn(M)>);
impl<M> ImplMarker for OptionDyn<M> {}

impl<M, D> IntoWidget<OptionDyn<M>> for Stateful<DynWidget<Option<D>>>
where
  M: ImplMarker + 'static,
  D: IntoWidget<M> + 'static,
{
  #[inline]
  fn into_widget(self) -> Widget { DynRender::<_, SingleDyn<_>>::new(self).into_widget() }
}

impl<D, M> IntoWidget<M> for DynWidget<D>
where
  M: ImplMarker,
  D: IntoWidget<M> + 'static,
{
  #[inline]
  fn into_widget(self) -> Widget { self.into_inner().into_widget() }
}

impl<D: SingleChild> SingleChild for DynWidget<D> {}
impl<D: MultiChild> MultiChild for DynWidget<D> {}

impl<D> ComposeChild for Stateful<DynWidget<D>>
where
  D: ComposeChild + 'static,
  D::Child: Clone,
{
  type Child = D::Child;

  fn compose_child(this: State<Self>, child: Self::Child) -> Widget {
    let dyns = match this {
      State::Stateless(dyns) => dyns,
      State::Stateful(dyns) => dyns.silent_ref().clone(),
    };

    widget! {
      states { dyns }
      DynWidget {
        dyns: dyns.silent().dyns.take().map(|d| {
          ComposeChild::compose_child(d.into(), child.clone())
        }),
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use crate::{builtin_widgets::key::KeyChange, prelude::*, test::*, widget_tree::WidgetTree};

  #[test]
  fn expr_widget_as_root() {
    let size = Stateful::new(Size::zero());
    let w = widget! {
      states { size: size.clone() }
      DynWidget {
        dyns: MockBox { size: *size },
        Void {}
      }
    };
    let scheduler = FuturesLocalSchedulerPool::default().spawner();
    let mut tree = WidgetTree::new(w, WindowCtx::new(AppContext::default(), scheduler));
    tree.layout(Size::zero());
    let ids = tree.root().descendants(&tree.arena).collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    {
      *size.state_ref() = Size::new(1., 1.);
    }
    tree.layout(Size::zero());
    let new_ids = tree.root().descendants(&tree.arena).collect::<Vec<_>>();
    assert_eq!(new_ids.len(), 2);

    assert_eq!(ids[1], new_ids[1]);
  }

  #[test]
  fn expr_widget_with_declare_child() {
    let size = Stateful::new(Size::zero());
    let w = widget! {
      states { size: size.clone() }
      MockBox {
        size: Size::zero(),
        DynWidget {
          dyns: MockBox { size: *size },
          Void {}
        }
      }
    };
    let app_ctx = <_>::default();
    let scheduler = FuturesLocalSchedulerPool::default().spawner();
    let mut tree = WidgetTree::new(w, WindowCtx::new(app_ctx, scheduler));
    tree.layout(Size::zero());
    let ids = tree.root().descendants(&tree.arena).collect::<Vec<_>>();
    assert_eq!(ids.len(), 3);
    {
      *size.state_ref() = Size::new(1., 1.);
    }
    tree.layout(Size::zero());
    let new_ids = tree.root().descendants(&tree.arena).collect::<Vec<_>>();
    assert_eq!(new_ids.len(), 3);

    assert_eq!(ids[0], new_ids[0]);
    assert_eq!(ids[2], new_ids[2]);
  }

  #[test]
  fn expr_widget_mounted_new() {
    let v = Stateful::new(vec![1, 2, 3]);

    let new_cnt = Stateful::new(0);
    let drop_cnt = Stateful::new(0);
    let w = widget! {
      states {
        v: v.clone(),
        new_cnt: new_cnt.clone(),
        drop_cnt: drop_cnt.clone(),
      }

      MockMulti { DynWidget {
        dyns: {
          v.clone().into_iter().map(move |_| {
            widget! {
              MockBox{
                size: Size::zero(),
                on_mounted: move |_| *new_cnt += 1,
                on_disposed: move |_| *drop_cnt += 1
              }
            }
          })
        }
      }}
    };
    let scheduler = FuturesLocalSchedulerPool::default().spawner();
    let mut tree = WidgetTree::new(w, WindowCtx::new(AppContext::default(), scheduler));
    tree.layout(Size::zero());
    assert_eq!(*new_cnt.state_ref(), 3);
    assert_eq!(*drop_cnt.state_ref(), 0);

    v.state_ref().push(4);
    tree.layout(Size::zero());
    assert_eq!(*new_cnt.state_ref(), 7);
    assert_eq!(*drop_cnt.state_ref(), 3);

    v.state_ref().pop();
    tree.layout(Size::zero());
    assert_eq!(*new_cnt.state_ref(), 10);
    assert_eq!(*drop_cnt.state_ref(), 7);
  }

  #[test]
  fn dyn_widgets_with_key() {
    let v = Stateful::new(HashMap::from([(1, '1'), (2, '2'), (3, '3')]));
    let enter_list: Stateful<Vec<char>> = Stateful::new(vec![]);
    let update_list: Stateful<Vec<char>> = Stateful::new(vec![]);
    let leave_list: Stateful<Vec<char>> = Stateful::new(vec![]);
    let key_change: Stateful<KeyChange<char>> = Stateful::new(KeyChange::default());
    let w = widget! {
      states {
        v: v.clone(),
        enter_list: enter_list.clone(),
        update_list: update_list.clone(),
        leave_list: leave_list.clone(),
        key_change: key_change.clone(),
      }

      MockMulti {
        DynWidget {
          dyns: {
            v.clone().into_iter().map(move |(i, c)| {
              widget! {
                KeyWidget {
                  id: key,
                  key: Key::from(i),
                  value: Some(c),

                  MockBox {
                    size: Size::zero(),
                    on_mounted: move |_| {
                      if key.is_enter() {
                        (*enter_list).push(key.value.unwrap());
                      }

                      if key.is_changed() {
                        (*update_list).push(key.value.unwrap());
                        *key_change = key.get_change();
                      }
                    },
                    on_disposed: move |_| {
                      if key.is_disposed() {
                        (*leave_list).push(key.value.unwrap());
                      }
                    }
                  }
                }
              }
            })
          }
        }
      }
    };

    // 1. 3 item enter
    let app_ctx = <_>::default();
    let scheduler = FuturesLocalSchedulerPool::default().spawner();
    let mut tree = WidgetTree::new(w, WindowCtx::new(app_ctx, scheduler));
    tree.layout(Size::zero());
    let expect_vec = vec!['1', '2', '3'];
    assert_eq!((*enter_list.state_ref()).len(), 3);
    assert!(
      (*enter_list.state_ref())
        .iter()
        .all(|item| expect_vec.contains(item))
    );
    // clear enter list vec
    (*enter_list.state_ref()).clear();

    // 2. add 1 item
    v.state_ref().insert(4, '4');
    tree.layout(Size::zero());
    let expect_vec = vec!['4'];
    assert_eq!((*enter_list.state_ref()).len(), 1);
    assert!(
      (*enter_list.state_ref())
        .iter()
        .all(|item| expect_vec.contains(item))
    );
    // clear enter list vec
    (*enter_list.state_ref()).clear();

    // 3. update the second item
    v.state_ref().insert(2, 'b');
    tree.layout(Size::zero());

    let expect_vec = vec![];
    assert_eq!((*enter_list.state_ref()).len(), 0);
    assert!(
      (*enter_list.state_ref())
        .iter()
        .all(|item| expect_vec.contains(item))
    );

    let expect_vec = vec!['b'];
    assert_eq!((*update_list.state_ref()).len(), 1);
    assert!(
      (*update_list.state_ref())
        .iter()
        .all(|item| expect_vec.contains(item))
    );
    assert_eq!(*key_change.state_ref(), KeyChange(Some('2'), Some('b')));
    (*update_list.state_ref()).clear();

    // 4. remove the second item
    v.state_ref().remove(&3);
    tree.layout(Size::zero());
    let expect_vec = vec!['3'];
    assert_eq!((*leave_list.state_ref()), expect_vec);
    assert_eq!((*leave_list.state_ref()).len(), 1);
    assert!(
      (*leave_list.state_ref())
        .iter()
        .all(|item| expect_vec.contains(item))
    );
  }
}
