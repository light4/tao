// Copyright 2014-2021 The winit contributors
// Copyright 2021-2022 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0

use std::{
  collections::VecDeque,
  convert::TryInto,
  f64,
  os::raw::c_void,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
  },
};

use raw_window_handle::{
  AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle,
};

use crate::{
  dpi::{
    LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Position, Size, Size::Logical,
  },
  error::{ExternalError, NotSupportedError, OsError as RootOsError},
  icon::Icon,
  monitor::{MonitorHandle as RootMonitorHandle, VideoMode as RootVideoMode},
  platform::macos::WindowExtMacOS,
  platform_impl::platform::{
    app_state::AppState,
    ffi, menu,
    monitor::{self, MonitorHandle, VideoMode},
    util::{self, IdRef},
    view::{self, new_view, CursorState},
    window_delegate::new_delegate,
    OsError,
  },
  window::{
    CursorIcon, Fullscreen, Theme, UserAttentionType, WindowAttributes, WindowId as RootWindowId,
  },
};
use cocoa::{
  appkit::{
    self, CGFloat, NSApp, NSApplication, NSApplicationPresentationOptions, NSColor, NSEvent,
    NSRequestUserAttentionType, NSScreen, NSView, NSWindow, NSWindowButton, NSWindowOrderingMode,
    NSWindowStyleMask,
  },
  base::{id, nil},
  foundation::{
    NSArray, NSAutoreleasePool, NSDictionary, NSPoint, NSRect, NSSize, NSString, NSUInteger,
  },
};
use core_graphics::display::{CGDisplay, CGDisplayMode};
use objc::{
  declare::ClassDecl,
  runtime::{Class, Object, Sel, BOOL, NO, YES},
};

use super::{util::ns_string_to_rust, Menu};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub usize);

impl Id {
  pub unsafe fn dummy() -> Self {
    Id(0)
  }
}

// Convert the `cocoa::base::id` associated with a window to a usize to use as a unique identifier
// for the window.
pub fn get_window_id(window_cocoa_id: id) -> Id {
  Id(window_cocoa_id as *const Object as _)
}

#[non_exhaustive]
#[derive(Clone)]
pub enum Parent {
  None,
  ChildOf(*mut c_void),
}

#[derive(Clone)]
pub struct PlatformSpecificWindowBuilderAttributes {
  pub parent: Parent,
  pub movable_by_window_background: bool,
  pub titlebar_transparent: bool,
  pub title_hidden: bool,
  pub titlebar_hidden: bool,
  pub titlebar_buttons_hidden: bool,
  pub fullsize_content_view: bool,
  pub resize_increments: Option<LogicalSize<f64>>,
  pub disallow_hidpi: bool,
  pub has_shadow: bool,
  pub automatic_tabbing: bool,
  pub tabbing_identifier: Option<String>,
}

impl Default for PlatformSpecificWindowBuilderAttributes {
  #[inline]
  fn default() -> Self {
    Self {
      parent: Parent::None,
      movable_by_window_background: false,
      titlebar_transparent: false,
      title_hidden: false,
      titlebar_hidden: false,
      titlebar_buttons_hidden: false,
      fullsize_content_view: false,
      resize_increments: None,
      disallow_hidpi: false,
      has_shadow: true,
      automatic_tabbing: true,
      tabbing_identifier: None,
    }
  }
}

unsafe fn create_view(
  ns_window: id,
  pl_attribs: &PlatformSpecificWindowBuilderAttributes,
) -> Option<(IdRef, Weak<Mutex<CursorState>>)> {
  let (ns_view, cursor_state) = new_view(ns_window);
  ns_view.non_nil().map(|ns_view| {
    if !pl_attribs.disallow_hidpi {
      ns_view.setWantsBestResolutionOpenGLSurface_(YES);
    }

    // On Mojave, views automatically become layer-backed shortly after being added to
    // a window. Changing the layer-backedness of a view breaks the association between
    // the view and its associated OpenGL context. To work around this, on Mojave we
    // explicitly make the view layer-backed up front so that AppKit doesn't do it
    // itself and break the association with its context.
    if f64::floor(appkit::NSAppKitVersionNumber) > appkit::NSAppKitVersionNumber10_12 {
      ns_view.setWantsLayer(YES);
    }

    (ns_view, cursor_state)
  })
}

fn create_window(
  attrs: &WindowAttributes,
  pl_attrs: &PlatformSpecificWindowBuilderAttributes,
) -> Option<IdRef> {
  unsafe {
    let pool = NSAutoreleasePool::new(nil);
    let screen = match attrs.fullscreen {
      Some(Fullscreen::Borderless(Some(RootMonitorHandle { inner: ref monitor })))
      | Some(Fullscreen::Exclusive(RootVideoMode {
        video_mode: VideoMode { ref monitor, .. },
      })) => {
        let monitor_screen = monitor.ns_screen();
        Some(monitor_screen.unwrap_or_else(|| appkit::NSScreen::mainScreen(nil)))
      }
      Some(Fullscreen::Borderless(None)) => Some(appkit::NSScreen::mainScreen(nil)),
      None => None,
    };
    let frame = match screen {
      Some(screen) => NSScreen::frame(screen),
      None => {
        let screen = NSScreen::mainScreen(nil);
        let scale_factor = NSScreen::backingScaleFactor(screen) as f64;
        let (width, height) = match attrs.inner_size {
          Some(size) => {
            let logical = size.to_logical(scale_factor);
            (logical.width, logical.height)
          }
          None => (800.0, 600.0),
        };
        let (left, bottom) = match attrs.position {
          Some(position) => {
            let logical = util::window_position(position.to_logical(scale_factor));
            // macOS wants the position of the bottom left corner,
            // but caller is setting the position of top left corner
            (logical.x, logical.y - height)
          }
          // This value is ignored by calling win.center() below
          None => (0.0, 0.0),
        };
        NSRect::new(NSPoint::new(left, bottom), NSSize::new(width, height))
      }
    };

    let mut masks = if !attrs.decorations && screen.is_none() || pl_attrs.titlebar_hidden {
      // Resizable UnownedWindow without a titlebar or borders
      // if decorations is set to false, ignore pl_attrs
      NSWindowStyleMask::NSBorderlessWindowMask
        | NSWindowStyleMask::NSResizableWindowMask
        | NSWindowStyleMask::NSMiniaturizableWindowMask
    } else {
      // default case, resizable window with titlebar and titlebar buttons
      NSWindowStyleMask::NSClosableWindowMask
        | NSWindowStyleMask::NSMiniaturizableWindowMask
        | NSWindowStyleMask::NSResizableWindowMask
        | NSWindowStyleMask::NSTitledWindowMask
    };

    if !attrs.resizable {
      masks &= !NSWindowStyleMask::NSResizableWindowMask;
    }

    if !attrs.minimizable {
      masks &= !NSWindowStyleMask::NSMiniaturizableWindowMask;
    }

    if !attrs.closable {
      masks &= !NSWindowStyleMask::NSClosableWindowMask;
    }

    if pl_attrs.fullsize_content_view {
      masks |= NSWindowStyleMask::NSFullSizeContentViewWindowMask;
    }

    let ns_window: id = msg_send![WINDOW_CLASS.0, alloc];
    let ns_window = IdRef::new(ns_window.initWithContentRect_styleMask_backing_defer_(
      frame,
      masks,
      appkit::NSBackingStoreBuffered,
      NO,
    ));
    let res = ns_window.non_nil().map(|ns_window| {
      let title = util::ns_string_id_ref(&attrs.title);
      ns_window.setReleasedWhenClosed_(NO);
      ns_window.setTitle_(*title);
      ns_window.setAcceptsMouseMovedEvents_(YES);

      if pl_attrs.titlebar_transparent {
        ns_window.setTitlebarAppearsTransparent_(YES);
      }
      if pl_attrs.title_hidden {
        ns_window.setTitleVisibility_(appkit::NSWindowTitleVisibility::NSWindowTitleHidden);
      }
      if pl_attrs.titlebar_buttons_hidden {
        for titlebar_button in &[
          NSWindowButton::NSWindowFullScreenButton,
          NSWindowButton::NSWindowMiniaturizeButton,
          NSWindowButton::NSWindowCloseButton,
          NSWindowButton::NSWindowZoomButton,
        ] {
          let button = ns_window.standardWindowButton_(*titlebar_button);
          let _: () = msg_send![button, setHidden: YES];
        }
      }
      if pl_attrs.movable_by_window_background {
        ns_window.setMovableByWindowBackground_(YES);
      }

      if attrs.always_on_top {
        let _: () = msg_send![
          *ns_window,
          setLevel: ffi::NSWindowLevel::NSFloatingWindowLevel
        ];
      }

      if attrs.always_on_bottom {
        let _: () = msg_send![
          *ns_window,
          setLevel: ffi::NSWindowLevel::BelowNormalWindowLevel
        ];
      }

      if attrs.content_protection {
        let _: () = msg_send![*ns_window, setSharingType: 0];
      }

      if !attrs.maximizable {
        let button = ns_window.standardWindowButton_(NSWindowButton::NSWindowZoomButton);
        let _: () = msg_send![button, setEnabled: NO];
      }

      if let Some(increments) = pl_attrs.resize_increments {
        let (x, y) = (increments.width, increments.height);
        if x >= 1.0 && y >= 1.0 {
          let size = NSSize::new(x as CGFloat, y as CGFloat);
          ns_window.setResizeIncrements_(size);
        }
      }

      if let Parent::ChildOf(parent) = pl_attrs.parent {
        let _: () = msg_send![parent as id, addChildWindow: *ns_window ordered: NSWindowOrderingMode::NSWindowAbove];
      }

      if !pl_attrs.automatic_tabbing {
        NSWindow::setAllowsAutomaticWindowTabbing_(*ns_window, NO);
      }

      if let Some(tabbing_identifier) = &pl_attrs.tabbing_identifier {
        let _: () = msg_send![*ns_window, setTabbingIdentifier: NSString::alloc(nil).init_str(tabbing_identifier)];
      }

      if !pl_attrs.has_shadow {
        ns_window.setHasShadow_(NO);
      }
      if attrs.position.is_none() {
        ns_window.center();
      }
      if let Some(window_menu) = attrs.window_menu.clone() {
        menu::initialize(window_menu);
      }

      ns_window
    });
    pool.drain();
    res
  }
}

pub(super) fn get_ns_theme() -> Theme {
  unsafe {
    let mut appearances: Vec<id> = Vec::new();
    appearances.push(NSString::alloc(nil).init_str("NSAppearanceNameAqua"));
    appearances.push(NSString::alloc(nil).init_str("NSAppearanceNameDarkAqua"));
    let app_class = class!(NSApplication);
    let app: id = msg_send![app_class, sharedApplication];
    let has_theme: BOOL = msg_send![app, respondsToSelector: sel!(effectiveAppearance)];
    if has_theme == NO {
      return Theme::Light;
    }
    let appearance: id = msg_send![app, effectiveAppearance];
    let name: id = msg_send![
      appearance,
      bestMatchFromAppearancesWithNames: NSArray::arrayWithObjects(nil, &appearances)
    ];
    let name = ns_string_to_rust(name);
    match &name[..] {
      "NSAppearanceNameDarkAqua" => Theme::Dark,
      _ => Theme::Light,
    }
  }
}

pub(super) fn set_ns_theme(theme: Theme) {
  let name = match theme {
    Theme::Dark => "NSAppearanceNameDarkAqua",
    Theme::Light => "NSAppearanceNameAqua",
  };
  unsafe {
    let app_class = class!(NSApplication);
    let app: id = msg_send![app_class, sharedApplication];
    let has_theme: BOOL = msg_send![app, respondsToSelector: sel!(effectiveAppearance)];
    if has_theme == YES {
      let name = NSString::alloc(nil).init_str(name);
      let appearance: id = msg_send![class!(NSAppearance), appearanceNamed: name];
      let _: () = msg_send![app, setAppearance: appearance];
    }
  }
}

struct WindowClass(*const Class);
unsafe impl Send for WindowClass {}
unsafe impl Sync for WindowClass {}

lazy_static! {
  static ref WINDOW_CLASS: WindowClass = unsafe {
    let window_superclass = class!(NSWindow);
    let mut decl = ClassDecl::new("TaoWindow", window_superclass).unwrap();
    decl.add_method(
      sel!(canBecomeMainWindow),
      util::yes as extern "C" fn(&Object, Sel) -> BOOL,
    );
    decl.add_method(
      sel!(canBecomeKeyWindow),
      util::yes as extern "C" fn(&Object, Sel) -> BOOL,
    );
    decl.add_method(
      sel!(sendEvent:),
      send_event as extern "C" fn(&Object, Sel, id),
    );
    WindowClass(decl.register())
  };
}

extern "C" fn send_event(this: &Object, _sel: Sel, event: id) {
  unsafe {
    let event_type = event.eventType();
    match event_type {
      appkit::NSLeftMouseDown => {
        // When wkwebview is set on NSWindow, `WindowBuilder::with_movable_by_window_background` is not working.
        // Because of this, we need to invoke `[NSWindow performWindowDragWithEvent]` in NSLeftMouseDown event.
        let is_movable_window: BOOL = msg_send![this, isMovableByWindowBackground];
        if is_movable_window == YES {
          let _: () = msg_send![this, performWindowDragWithEvent: event];
        }
      }
      _ => (),
    }
    let superclass = util::superclass(this);
    let _: () = msg_send![super(this, superclass), sendEvent: event];
  }
}

#[derive(Default)]
pub struct SharedState {
  pub resizable: bool,
  pub fullscreen: Option<Fullscreen>,
  // This is true between windowWillEnterFullScreen and windowDidEnterFullScreen
  // or windowWillExitFullScreen and windowDidExitFullScreen.
  // We must not toggle fullscreen when this is true.
  pub in_fullscreen_transition: bool,
  // If it is attempted to toggle fullscreen when in_fullscreen_transition is true,
  // Set target_fullscreen and do after fullscreen transition is end.
  pub target_fullscreen: Option<Option<Fullscreen>>,
  pub maximized: bool,
  pub standard_frame: Option<NSRect>,
  is_simple_fullscreen: bool,
  pub saved_style: Option<NSWindowStyleMask>,
  /// Presentation options saved before entering `set_simple_fullscreen`, and
  /// restored upon exiting it. Also used when transitioning from Borderless to
  /// Exclusive fullscreen in `set_fullscreen` because we need to disable the menu
  /// bar in exclusive fullscreen but want to restore the original options when
  /// transitioning back to borderless fullscreen.
  save_presentation_opts: Option<NSApplicationPresentationOptions>,
  pub saved_desktop_display_mode: Option<(CGDisplay, CGDisplayMode)>,
  pub current_theme: Theme,
}

impl SharedState {
  pub fn saved_standard_frame(&self) -> NSRect {
    self
      .standard_frame
      .unwrap_or_else(|| NSRect::new(NSPoint::new(50.0, 50.0), NSSize::new(800.0, 600.0)))
  }
}

impl From<WindowAttributes> for SharedState {
  fn from(attribs: WindowAttributes) -> Self {
    SharedState {
      resizable: attribs.resizable,
      // This fullscreen field tracks the current state of the window
      // (as seen by `WindowDelegate`), and since the window hasn't
      // actually been fullscreened yet, we can't set it yet. This is
      // necessary for state transitions to work right, since otherwise
      // the initial value and the first `set_fullscreen` call would be
      // identical, resulting in a no-op.
      fullscreen: None,
      maximized: attribs.maximized,
      ..Default::default()
    }
  }
}

pub struct UnownedWindow {
  pub ns_window: IdRef, // never changes
  pub ns_view: IdRef,   // never changes
  input_context: IdRef, // never changes
  pub shared_state: Arc<Mutex<SharedState>>,
  decorations: AtomicBool,
  cursor_state: Weak<Mutex<CursorState>>,
  pub inner_rect: Option<PhysicalSize<u32>>,
}

unsafe impl Send for UnownedWindow {}
unsafe impl Sync for UnownedWindow {}

impl UnownedWindow {
  pub fn new(
    mut win_attribs: WindowAttributes,
    pl_attribs: PlatformSpecificWindowBuilderAttributes,
  ) -> Result<(Arc<Self>, IdRef), RootOsError> {
    unsafe {
      let is_main_thread: BOOL = msg_send!(class!(NSThread), isMainThread);
      if is_main_thread == NO {
        panic!("Windows can only be created on the main thread on macOS");
      }
    }
    trace!("Creating new window");

    let pool = unsafe { NSAutoreleasePool::new(nil) };
    let ns_window = create_window(&win_attribs, &pl_attribs).ok_or_else(|| {
      unsafe { pool.drain() };
      os_error!(OsError::CreationError("Couldn't create `NSWindow`"))
    })?;

    let (ns_view, cursor_state) =
      unsafe { create_view(*ns_window, &pl_attribs) }.ok_or_else(|| {
        unsafe { pool.drain() };
        os_error!(OsError::CreationError("Couldn't create `NSView`"))
      })?;

    // Configure the new view as the "key view" for the window
    unsafe {
      ns_window.setContentView_(*ns_view);
      ns_window.setInitialFirstResponder_(*ns_view);
    }

    let input_context = unsafe { util::create_input_context(*ns_view) };

    let scale_factor = unsafe { NSWindow::backingScaleFactor(*ns_window) as f64 };

    unsafe {
      if win_attribs.transparent {
        ns_window.setOpaque_(NO);
        ns_window.setBackgroundColor_(NSColor::clearColor(nil));
      }

      win_attribs.min_inner_size.map(|dim| {
        let logical_dim = dim.to_logical(scale_factor);
        set_min_inner_size(*ns_window, logical_dim)
      });
      win_attribs.max_inner_size.map(|dim| {
        let logical_dim = dim.to_logical(scale_factor);
        set_max_inner_size(*ns_window, logical_dim)
      });

      // register for drag and drop operations.
      let () = msg_send![
        *ns_window,
        registerForDraggedTypes: NSArray::arrayWithObject(nil, appkit::NSFilenamesPboardType)
      ];
    }

    // Since `win_attribs` is put into a mutex below, we'll just copy these
    // attributes now instead of bothering to lock it later.
    // Also, `SharedState` doesn't carry `fullscreen` over; it's set
    // indirectly by us calling `set_fullscreen` below, causing handlers in
    // `WindowDelegate` to update the state.
    let fullscreen = win_attribs.fullscreen.take();
    let maximized = win_attribs.maximized;
    let visible = win_attribs.visible;
    let focused = win_attribs.focused;
    let decorations = win_attribs.decorations;
    let inner_rect = win_attribs
      .inner_size
      .map(|size| size.to_physical(scale_factor));

    let cloned_preferred_theme = win_attribs.preferred_theme.clone();

    let window = Arc::new(UnownedWindow {
      ns_view,
      ns_window,
      input_context,
      shared_state: Arc::new(Mutex::new(win_attribs.into())),
      decorations: AtomicBool::new(decorations),
      cursor_state,
      inner_rect,
    });

    match cloned_preferred_theme {
      Some(theme) => {
        set_ns_theme(theme);
        let mut state = window.shared_state.lock().unwrap();
        state.current_theme = theme.clone();
      }
      None => {
        let mut state = window.shared_state.lock().unwrap();
        state.current_theme = get_ns_theme();
      }
    }

    let delegate = new_delegate(&window, fullscreen.is_some());

    // Set fullscreen mode after we setup everything
    window.set_fullscreen(fullscreen);

    // Setting the window as key has to happen *after* we set the fullscreen
    // state, since otherwise we'll briefly see the window at normal size
    // before it transitions.
    if visible {
      if focused {
        // Tightly linked with `app_state::window_activation_hack`
        unsafe { window.ns_window.makeKeyAndOrderFront_(nil) };
      } else {
        unsafe { window.ns_window.orderFront_(nil) };
      }
    }

    if maximized {
      window.set_maximized(maximized);
    }

    unsafe { pool.drain() };

    Ok((window, delegate))
  }

  fn set_style_mask_async(&self, mask: NSWindowStyleMask) {
    unsafe { util::set_style_mask_async(*self.ns_window, *self.ns_view, mask) };
  }

  fn set_style_mask_sync(&self, mask: NSWindowStyleMask) {
    unsafe { util::set_style_mask_sync(*self.ns_window, *self.ns_view, mask) };
  }

  pub fn id(&self) -> Id {
    get_window_id(*self.ns_window)
  }

  pub fn set_title(&self, title: &str) {
    unsafe {
      util::set_title_async(*self.ns_window, title.to_string());
    }
  }

  pub fn title(&self) -> String {
    unsafe {
      let title = self.ns_window.title();
      ns_string_to_rust(title)
    }
  }

  pub fn set_menu(&self, menu: Option<Menu>) {
    // TODO if None we should set an empty menu
    // On windows we can remove it, in macOS we can't
    if let Some(menu) = menu {
      menu::initialize(menu);
    }
  }

  pub fn set_visible(&self, visible: bool) {
    match visible {
      true => unsafe { util::make_key_and_order_front_async(*self.ns_window) },
      false => unsafe { util::order_out_async(*self.ns_window) },
    }
  }

  #[inline]
  // Shortener for set_visible(true)
  pub fn set_focus(&self) {
    unsafe {
      let is_minimized: BOOL = msg_send![*self.ns_window, isMiniaturized];
      if is_minimized == NO {
        util::set_focus(*self.ns_window);
      }
    }
  }

  #[inline]
  pub fn is_focused(&self) -> bool {
    unsafe {
      let is_key_window: BOOL = msg_send![*self.ns_window, isKeyWindow];
      is_key_window == YES
    }
  }

  pub fn request_redraw(&self) {
    AppState::queue_redraw(RootWindowId(self.id()));
  }

  pub fn outer_position(&self) -> Result<PhysicalPosition<i32>, NotSupportedError> {
    let frame_rect = unsafe { NSWindow::frame(*self.ns_window) };
    let position = LogicalPosition::new(
      frame_rect.origin.x as f64,
      util::bottom_left_to_top_left(frame_rect),
    );
    let scale_factor = self.scale_factor();
    Ok(position.to_physical(scale_factor))
  }

  pub fn inner_position(&self) -> Result<PhysicalPosition<i32>, NotSupportedError> {
    let content_rect = unsafe {
      NSWindow::contentRectForFrameRect_(*self.ns_window, NSWindow::frame(*self.ns_window))
    };
    let position = LogicalPosition::new(
      content_rect.origin.x as f64,
      util::bottom_left_to_top_left(content_rect),
    );
    let scale_factor = self.scale_factor();
    Ok(position.to_physical(scale_factor))
  }

  pub fn set_outer_position(&self, position: Position) {
    let scale_factor = self.scale_factor();
    let position = position.to_logical(scale_factor);
    unsafe {
      util::set_frame_top_left_point_async(*self.ns_window, util::window_position(position));
    }
  }

  #[inline]
  pub fn inner_size(&self) -> PhysicalSize<u32> {
    let view_frame = unsafe { NSView::frame(*self.ns_view) };
    let logical: LogicalSize<f64> =
      (view_frame.size.width as f64, view_frame.size.height as f64).into();
    let scale_factor = self.scale_factor();
    logical.to_physical(scale_factor)
  }

  #[inline]
  pub fn outer_size(&self) -> PhysicalSize<u32> {
    let view_frame = unsafe { NSWindow::frame(*self.ns_window) };
    let logical: LogicalSize<f64> =
      (view_frame.size.width as f64, view_frame.size.height as f64).into();
    let scale_factor = self.scale_factor();
    logical.to_physical(scale_factor)
  }

  #[inline]
  pub fn set_inner_size(&self, size: Size) {
    unsafe {
      let scale_factor = self.scale_factor();
      util::set_content_size_async(*self.ns_window, size.to_logical(scale_factor));
    }
  }

  pub fn set_min_inner_size(&self, dimensions: Option<Size>) {
    unsafe {
      let dimensions = dimensions.unwrap_or(Logical(LogicalSize {
        width: 0.0,
        height: 0.0,
      }));
      let scale_factor = self.scale_factor();
      set_min_inner_size(*self.ns_window, dimensions.to_logical(scale_factor));
    }
  }

  pub fn set_max_inner_size(&self, dimensions: Option<Size>) {
    unsafe {
      let dimensions = dimensions.unwrap_or(Logical(LogicalSize {
        width: std::f32::MAX as f64,
        height: std::f32::MAX as f64,
      }));
      let scale_factor = self.scale_factor();
      set_max_inner_size(*self.ns_window, dimensions.to_logical(scale_factor));
    }
  }

  #[inline]
  pub fn set_resizable(&self, resizable: bool) {
    let fullscreen = {
      trace!("Locked shared state in `set_resizable`");
      let mut shared_state_lock = self.shared_state.lock().unwrap();
      shared_state_lock.resizable = resizable;
      trace!("Unlocked shared state in `set_resizable`");
      shared_state_lock.fullscreen.is_some()
    };
    if !fullscreen {
      let mut mask = unsafe { self.ns_window.styleMask() };
      if resizable {
        mask |= NSWindowStyleMask::NSResizableWindowMask;
      } else {
        mask &= !NSWindowStyleMask::NSResizableWindowMask;
      }
      self.set_style_mask_async(mask);
    } // Otherwise, we don't change the mask until we exit fullscreen.
  }

  #[inline]
  pub fn set_minimizable(&self, minimizable: bool) {
    let mut mask = unsafe { self.ns_window.styleMask() };
    if minimizable {
      mask |= NSWindowStyleMask::NSMiniaturizableWindowMask;
    } else {
      mask &= !NSWindowStyleMask::NSMiniaturizableWindowMask;
    }
    self.set_style_mask_async(mask);
  }

  #[inline]
  pub fn set_maximizable(&self, maximizable: bool) {
    unsafe {
      let button = self
        .ns_window
        .standardWindowButton_(NSWindowButton::NSWindowZoomButton);
      let _: () = msg_send![button, setEnabled: maximizable];
    }
  }

  #[inline]
  pub fn set_closable(&self, closable: bool) {
    let mut mask = unsafe { self.ns_window.styleMask() };
    if closable {
      mask |= NSWindowStyleMask::NSClosableWindowMask;
    } else {
      mask &= !NSWindowStyleMask::NSClosableWindowMask;
    }
    self.set_style_mask_async(mask);
  }

  pub fn set_cursor_icon(&self, cursor: CursorIcon) {
    let cursor = util::Cursor::from(cursor);
    if let Some(cursor_access) = self.cursor_state.upgrade() {
      cursor_access.lock().unwrap().cursor = cursor;
    }
    unsafe {
      let _: () = msg_send![*self.ns_window,
          invalidateCursorRectsForView:*self.ns_view
      ];
    }
  }

  #[inline]
  pub fn set_cursor_grab(&self, grab: bool) -> Result<(), ExternalError> {
    // TODO: Do this for real https://stackoverflow.com/a/40922095/5435443
    CGDisplay::associate_mouse_and_mouse_cursor_position(!grab)
      .map_err(|status| ExternalError::Os(os_error!(OsError::CGError(status))))
  }

  #[inline]
  pub fn set_cursor_visible(&self, visible: bool) {
    if let Some(cursor_access) = self.cursor_state.upgrade() {
      let mut cursor_state = cursor_access.lock().unwrap();
      if visible != cursor_state.visible {
        cursor_state.visible = visible;
        drop(cursor_state);
        unsafe {
          let _: () = msg_send![*self.ns_window,
              invalidateCursorRectsForView:*self.ns_view
          ];
        }
      }
    }
  }

  #[inline]
  pub fn scale_factor(&self) -> f64 {
    unsafe { NSWindow::backingScaleFactor(*self.ns_window) as _ }
  }

  #[inline]
  pub fn set_cursor_position(&self, cursor_position: Position) -> Result<(), ExternalError> {
    let physical_window_position = self.inner_position().unwrap();
    let scale_factor = self.scale_factor();
    let window_position = physical_window_position.to_logical::<CGFloat>(scale_factor);
    let logical_cursor_position = cursor_position.to_logical::<CGFloat>(scale_factor);
    let point = appkit::CGPoint {
      x: logical_cursor_position.x + window_position.x,
      y: logical_cursor_position.y + window_position.y,
    };
    CGDisplay::warp_mouse_cursor_position(point)
      .map_err(|e| ExternalError::Os(os_error!(OsError::CGError(e))))?;
    CGDisplay::associate_mouse_and_mouse_cursor_position(true)
      .map_err(|e| ExternalError::Os(os_error!(OsError::CGError(e))))?;

    Ok(())
  }

  #[inline]
  pub fn drag_window(&self) -> Result<(), ExternalError> {
    unsafe {
      let event: id = msg_send![NSApp(), currentEvent];
      let _: () = msg_send![*self.ns_window, performWindowDragWithEvent: event];
    }

    Ok(())
  }

  #[inline]
  pub fn set_ignore_cursor_events(&self, ignore: bool) -> Result<(), ExternalError> {
    unsafe {
      util::set_ignore_mouse_events(*self.ns_window, ignore);
    }

    Ok(())
  }

  pub(crate) fn is_zoomed(&self) -> bool {
    // because `isZoomed` doesn't work if the window's borderless,
    // we make it resizable temporalily.
    let curr_mask = unsafe { self.ns_window.styleMask() };

    let required = NSWindowStyleMask::NSTitledWindowMask | NSWindowStyleMask::NSResizableWindowMask;
    let needs_temp_mask = !curr_mask.contains(required);
    if needs_temp_mask {
      self.set_style_mask_sync(required);
    }

    let is_zoomed: BOOL = unsafe { msg_send![*self.ns_window, isZoomed] };

    // Roll back temp styles
    if needs_temp_mask {
      self.set_style_mask_sync(curr_mask);
    }

    is_zoomed != NO
  }

  fn saved_style(&self, shared_state: &mut SharedState) -> NSWindowStyleMask {
    let base_mask = shared_state
      .saved_style
      .take()
      .unwrap_or_else(|| unsafe { self.ns_window.styleMask() });
    if shared_state.resizable {
      base_mask | NSWindowStyleMask::NSResizableWindowMask
    } else {
      base_mask & !NSWindowStyleMask::NSResizableWindowMask
    }
  }

  /// This is called when the window is exiting fullscreen, whether by the
  /// user clicking on the green fullscreen button or programmatically by
  /// `toggleFullScreen:`
  pub(crate) fn restore_state_from_fullscreen(&self) {
    trace!("Locked shared state in `restore_state_from_fullscreen`");
    let mut shared_state_lock = self.shared_state.lock().unwrap();

    shared_state_lock.fullscreen = None;

    let maximized = shared_state_lock.maximized;
    let mask = self.saved_style(&mut *shared_state_lock);

    drop(shared_state_lock);
    trace!("Unocked shared state in `restore_state_from_fullscreen`");

    self.set_style_mask_async(mask);
    self.set_maximized(maximized);
  }

  #[inline]
  pub fn set_minimized(&self, minimized: bool) {
    let is_minimized: BOOL = unsafe { msg_send![*self.ns_window, isMiniaturized] };
    let is_minimized: bool = is_minimized == YES;
    if is_minimized == minimized {
      return;
    }

    if minimized {
      unsafe {
        NSWindow::miniaturize_(*self.ns_window, *self.ns_window);
      }
    } else {
      unsafe {
        NSWindow::deminiaturize_(*self.ns_window, *self.ns_window);
      }
    }
  }

  #[inline]
  pub fn set_maximized(&self, maximized: bool) {
    let is_zoomed = self.is_zoomed();
    if is_zoomed == maximized {
      return;
    };
    unsafe {
      util::set_maximized_async(
        *self.ns_window,
        is_zoomed,
        maximized,
        Arc::downgrade(&self.shared_state),
      );
    }
  }

  #[inline]
  pub fn fullscreen(&self) -> Option<Fullscreen> {
    let shared_state_lock = self.shared_state.lock().unwrap();
    shared_state_lock.fullscreen.clone()
  }

  #[inline]
  pub fn is_visible(&self) -> bool {
    let is_visible: BOOL = unsafe { msg_send![*self.ns_window, isVisible] };
    is_visible == YES
  }

  #[inline]
  pub fn is_maximized(&self) -> bool {
    self.is_zoomed()
  }

  #[inline]
  pub fn is_minimized(&self) -> bool {
    let is_minimized: BOOL = unsafe { msg_send![*self.ns_window, isMiniaturized] };
    is_minimized == YES
  }

  #[inline]
  pub fn is_resizable(&self) -> bool {
    let is_resizable: BOOL = unsafe { msg_send![*self.ns_window, isResizable] };
    is_resizable == YES
  }

  #[inline]
  pub fn is_minimizable(&self) -> bool {
    let is_minimizable: BOOL = unsafe { msg_send![*self.ns_window, isMiniaturizable] };
    is_minimizable == YES
  }

  #[inline]
  pub fn is_maximizable(&self) -> bool {
    let is_maximizable: BOOL;
    unsafe {
      let button = self
        .ns_window
        .standardWindowButton_(NSWindowButton::NSWindowZoomButton);
      is_maximizable = msg_send![button, isEnabled];
    }
    is_maximizable == YES
  }

  #[inline]
  pub fn is_closable(&self) -> bool {
    let is_closable: BOOL = unsafe { msg_send![*self.ns_window, hasCloseBox] };
    is_closable == YES
  }

  #[inline]
  pub fn is_decorated(&self) -> bool {
    self.decorations.load(Ordering::Acquire)
  }

  #[inline]
  pub fn set_fullscreen(&self, fullscreen: Option<Fullscreen>) {
    trace!("Locked shared state in `set_fullscreen`");
    let mut shared_state_lock = self.shared_state.lock().unwrap();
    if shared_state_lock.is_simple_fullscreen {
      trace!("Unlocked shared state in `set_fullscreen`");
      return;
    }
    if shared_state_lock.in_fullscreen_transition {
      // We can't set fullscreen here.
      // Set fullscreen after transition.
      shared_state_lock.target_fullscreen = Some(fullscreen);
      trace!("Unlocked shared state in `set_fullscreen`");
      return;
    }
    let old_fullscreen = shared_state_lock.fullscreen.clone();
    if fullscreen == old_fullscreen {
      trace!("Unlocked shared state in `set_fullscreen`");
      return;
    }
    trace!("Unlocked shared state in `set_fullscreen`");
    drop(shared_state_lock);

    // If the fullscreen is on a different monitor, we must move the window
    // to that monitor before we toggle fullscreen (as `toggleFullScreen`
    // does not take a screen parameter, but uses the current screen)
    if let Some(ref fullscreen) = fullscreen {
      let new_screen = match fullscreen {
        Fullscreen::Borderless(borderless) => {
          let RootMonitorHandle { inner: monitor } = borderless
            .clone()
            .unwrap_or_else(|| self.current_monitor_inner());
          monitor
        }
        Fullscreen::Exclusive(RootVideoMode {
          video_mode: VideoMode { ref monitor, .. },
        }) => monitor.clone(),
      }
      .ns_screen()
      .unwrap();

      unsafe {
        let old_screen = NSWindow::screen(*self.ns_window);
        if old_screen != new_screen {
          let mut screen_frame: NSRect = msg_send![new_screen, frame];
          // The coordinate system here has its origin at bottom-left
          // and Y goes up
          screen_frame.origin.y += screen_frame.size.height;
          util::set_frame_top_left_point_async(*self.ns_window, screen_frame.origin);
        }
      }
    }

    if let Some(Fullscreen::Exclusive(ref video_mode)) = fullscreen {
      // Note: `enterFullScreenMode:withOptions:` seems to do the exact
      // same thing as we're doing here (captures the display, sets the
      // video mode, and hides the menu bar and dock), with the exception
      // of that I couldn't figure out how to set the display mode with
      // it. I think `enterFullScreenMode:withOptions:` is still using the
      // older display mode API where display modes were of the type
      // `CFDictionary`, but this has changed, so we can't obtain the
      // correct parameter for this any longer. Apple's code samples for
      // this function seem to just pass in "YES" for the display mode
      // parameter, which is not consistent with the docs saying that it
      // takes a `NSDictionary`..

      let display_id = video_mode.monitor().inner.native_identifier();

      let mut fade_token = ffi::kCGDisplayFadeReservationInvalidToken;

      if matches!(old_fullscreen, Some(Fullscreen::Borderless(_))) {
        unsafe {
          let app = NSApp();
          trace!("Locked shared state in `set_fullscreen`");
          let mut shared_state_lock = self.shared_state.lock().unwrap();
          shared_state_lock.save_presentation_opts = Some(app.presentationOptions_());
        }
      }

      unsafe {
        // Fade to black (and wait for the fade to complete) to hide the
        // flicker from capturing the display and switching display mode
        if ffi::CGAcquireDisplayFadeReservation(5.0, &mut fade_token) == ffi::kCGErrorSuccess {
          ffi::CGDisplayFade(
            fade_token,
            0.3,
            ffi::kCGDisplayBlendNormal,
            ffi::kCGDisplayBlendSolidColor,
            0.0,
            0.0,
            0.0,
            ffi::TRUE,
          );
        }

        assert_eq!(ffi::CGDisplayCapture(display_id), ffi::kCGErrorSuccess);
      }

      unsafe {
        let result = ffi::CGDisplaySetDisplayMode(
          display_id,
          video_mode.video_mode.native_mode.0,
          std::ptr::null(),
        );
        assert!(result == ffi::kCGErrorSuccess, "failed to set video mode");

        // After the display has been configured, fade back in
        // asynchronously
        if fade_token != ffi::kCGDisplayFadeReservationInvalidToken {
          ffi::CGDisplayFade(
            fade_token,
            0.6,
            ffi::kCGDisplayBlendSolidColor,
            ffi::kCGDisplayBlendNormal,
            0.0,
            0.0,
            0.0,
            ffi::FALSE,
          );
          ffi::CGReleaseDisplayFadeReservation(fade_token);
        }
      }
    }

    trace!("Locked shared state in `set_fullscreen`");
    let mut shared_state_lock = self.shared_state.lock().unwrap();
    shared_state_lock.fullscreen = fullscreen.clone();
    trace!("Unlocked shared state in `set_fullscreen`");

    match (&old_fullscreen, &fullscreen) {
      (&None, &Some(_)) => unsafe {
        util::toggle_full_screen_async(
          *self.ns_window,
          *self.ns_view,
          old_fullscreen.is_none(),
          Arc::downgrade(&self.shared_state),
        );
      },
      (&Some(Fullscreen::Borderless(_)), &None) => unsafe {
        // State is restored by `window_did_exit_fullscreen`
        util::toggle_full_screen_async(
          *self.ns_window,
          *self.ns_view,
          old_fullscreen.is_none(),
          Arc::downgrade(&self.shared_state),
        );
      },
      (&Some(Fullscreen::Exclusive(RootVideoMode { ref video_mode })), &None) => unsafe {
        util::restore_display_mode_async(video_mode.monitor().inner.native_identifier());
        // Rest of the state is restored by `window_did_exit_fullscreen`
        util::toggle_full_screen_async(
          *self.ns_window,
          *self.ns_view,
          old_fullscreen.is_none(),
          Arc::downgrade(&self.shared_state),
        );
      },
      (&Some(Fullscreen::Borderless(_)), &Some(Fullscreen::Exclusive(_))) => unsafe {
        // If we're already in fullscreen mode, calling
        // `CGDisplayCapture` will place the shielding window on top of
        // our window, which results in a black display and is not what
        // we want. So, we must place our window on top of the shielding
        // window. Unfortunately, this also makes our window be on top
        // of the menu bar, and this looks broken, so we must make sure
        // that the menu bar is disabled. This is done in the window
        // delegate in `window:willUseFullScreenPresentationOptions:`.
        let app = NSApp();
        trace!("Locked shared state in `set_fullscreen`");
        shared_state_lock.save_presentation_opts = Some(app.presentationOptions_());

        let presentation_options =
          NSApplicationPresentationOptions::NSApplicationPresentationFullScreen
            | NSApplicationPresentationOptions::NSApplicationPresentationHideDock
            | NSApplicationPresentationOptions::NSApplicationPresentationHideMenuBar;
        app.setPresentationOptions_(presentation_options);

        let () = msg_send![*self.ns_window, setLevel: ffi::CGShieldingWindowLevel() + 1];
      },
      (
        &Some(Fullscreen::Exclusive(RootVideoMode { ref video_mode })),
        &Some(Fullscreen::Borderless(_)),
      ) => unsafe {
        let presentation_options = shared_state_lock.save_presentation_opts.unwrap_or_else(|| {
          NSApplicationPresentationOptions::NSApplicationPresentationFullScreen
            | NSApplicationPresentationOptions::NSApplicationPresentationAutoHideDock
            | NSApplicationPresentationOptions::NSApplicationPresentationAutoHideMenuBar
        });
        NSApp().setPresentationOptions_(presentation_options);

        util::restore_display_mode_async(video_mode.monitor().inner.native_identifier());

        // Restore the normal window level following the Borderless fullscreen
        // `CGShieldingWindowLevel() + 1` hack.
        let () = msg_send![
          *self.ns_window,
          setLevel: ffi::NSWindowLevel::NSNormalWindowLevel
        ];
      },
      _ => {}
    }
    trace!("Unlocked shared state in `set_fullscreen`");
  }

  #[inline]
  pub fn set_decorations(&self, decorations: bool) {
    if decorations != self.decorations.load(Ordering::Acquire) {
      self.decorations.store(decorations, Ordering::Release);

      let (fullscreen, resizable) = {
        trace!("Locked shared state in `set_decorations`");
        let shared_state_lock = self.shared_state.lock().unwrap();
        trace!("Unlocked shared state in `set_decorations`");
        (
          shared_state_lock.fullscreen.is_some(),
          shared_state_lock.resizable,
        )
      };

      // If we're in fullscreen mode, we wait to apply decoration changes
      // until we're in `window_did_exit_fullscreen`.
      if fullscreen {
        return;
      }

      let new_mask = {
        let mut new_mask = if decorations {
          NSWindowStyleMask::NSClosableWindowMask
            | NSWindowStyleMask::NSMiniaturizableWindowMask
            | NSWindowStyleMask::NSResizableWindowMask
            | NSWindowStyleMask::NSTitledWindowMask
        } else {
          NSWindowStyleMask::NSBorderlessWindowMask | NSWindowStyleMask::NSResizableWindowMask
        };
        if !resizable {
          new_mask &= !NSWindowStyleMask::NSResizableWindowMask;
        }
        new_mask
      };
      self.set_style_mask_async(new_mask);
    }
  }

  #[inline]
  pub fn set_always_on_bottom(&self, always_on_bottom: bool) {
    let level = if always_on_bottom {
      ffi::NSWindowLevel::BelowNormalWindowLevel
    } else {
      ffi::NSWindowLevel::NSNormalWindowLevel
    };
    unsafe { util::set_level_async(*self.ns_window, level) };
  }

  #[inline]
  pub fn set_always_on_top(&self, always_on_top: bool) {
    let level = if always_on_top {
      ffi::NSWindowLevel::NSFloatingWindowLevel
    } else {
      ffi::NSWindowLevel::NSNormalWindowLevel
    };
    unsafe { util::set_level_async(*self.ns_window, level) };
  }

  #[inline]
  pub fn set_window_icon(&self, _icon: Option<Icon>) {
    // macOS doesn't have window icons. Though, there is
    // `setRepresentedFilename`, but that's semantically distinct and should
    // only be used when the window is in some way representing a specific
    // file/directory. For instance, Terminal.app uses this for the CWD.
    // Anyway, that should eventually be implemented as
    // `WindowBuilderExt::with_represented_file` or something, and doesn't
    // have anything to do with `set_window_icon`.
    // https://developer.apple.com/library/content/documentation/Cocoa/Conceptual/WinPanel/Tasks/SettingWindowTitle.html
  }

  #[inline]
  pub fn set_ime_position(&self, spot: Position) {
    let scale_factor = self.scale_factor();
    let logical_spot = spot.to_logical(scale_factor);
    unsafe {
      view::set_ime_position(
        *self.ns_view,
        *self.input_context,
        logical_spot.x,
        logical_spot.y,
      );
    }
  }

  #[inline]
  pub fn request_user_attention(&self, request_type: Option<UserAttentionType>) {
    let ns_request_type = request_type.map(|ty| match ty {
      UserAttentionType::Critical => NSRequestUserAttentionType::NSCriticalRequest,
      UserAttentionType::Informational => NSRequestUserAttentionType::NSInformationalRequest,
    });
    unsafe {
      if let Some(ty) = ns_request_type {
        NSApp().requestUserAttention_(ty);
      }
    }
  }

  #[inline]
  pub fn hide_menu(&self) {}

  #[inline]
  pub fn show_menu(&self) {}

  #[inline]
  pub fn is_menu_visible(&self) -> bool {
    warn!("`Window::is_menu_visible` always return true on macOS");
    true
  }

  #[inline]
  // Allow directly accessing the current monitor internally without unwrapping.
  pub(crate) fn current_monitor_inner(&self) -> RootMonitorHandle {
    unsafe {
      let screen: id = msg_send![*self.ns_window, screen];
      let desc = NSScreen::deviceDescription(screen);
      let key = util::ns_string_id_ref("NSScreenNumber");
      let value = NSDictionary::valueForKey_(desc, *key);
      let display_id: NSUInteger = msg_send![value, unsignedIntegerValue];
      RootMonitorHandle {
        inner: MonitorHandle::new(display_id.try_into().unwrap()),
      }
    }
  }

  #[inline]
  pub fn current_monitor(&self) -> Option<RootMonitorHandle> {
    Some(self.current_monitor_inner())
  }

  #[inline]
  pub fn available_monitors(&self) -> VecDeque<MonitorHandle> {
    monitor::available_monitors()
  }

  #[inline]
  pub fn primary_monitor(&self) -> Option<RootMonitorHandle> {
    let monitor = monitor::primary_monitor();
    Some(RootMonitorHandle { inner: monitor })
  }

  #[inline]
  pub fn raw_window_handle(&self) -> RawWindowHandle {
    let mut window_handle = AppKitWindowHandle::empty();
    window_handle.ns_window = *self.ns_window as *mut _;
    window_handle.ns_view = *self.ns_view as *mut _;
    RawWindowHandle::AppKit(window_handle)
  }

  #[inline]
  pub fn raw_display_handle(&self) -> RawDisplayHandle {
    RawDisplayHandle::AppKit(AppKitDisplayHandle::empty())
  }

  #[inline]
  pub fn theme(&self) -> Theme {
    let state = self.shared_state.lock().unwrap();
    state.current_theme
  }

  pub fn set_content_protection(&self, enabled: bool) {
    unsafe {
      let _: () = msg_send![*self.ns_window, setSharingType: !enabled as i32];
    }
  }
}

impl WindowExtMacOS for UnownedWindow {
  #[inline]
  fn ns_window(&self) -> *mut c_void {
    *self.ns_window as *mut _
  }

  #[inline]
  fn ns_view(&self) -> *mut c_void {
    *self.ns_view as *mut _
  }

  #[inline]
  fn simple_fullscreen(&self) -> bool {
    let shared_state_lock = self.shared_state.lock().unwrap();
    shared_state_lock.is_simple_fullscreen
  }

  #[inline]
  fn set_simple_fullscreen(&self, fullscreen: bool) -> bool {
    let mut shared_state_lock = self.shared_state.lock().unwrap();

    unsafe {
      let app = NSApp();
      let is_native_fullscreen = shared_state_lock.fullscreen.is_some();
      let is_simple_fullscreen = shared_state_lock.is_simple_fullscreen;

      // Do nothing if native fullscreen is active.
      if is_native_fullscreen
        || (fullscreen && is_simple_fullscreen)
        || (!fullscreen && !is_simple_fullscreen)
      {
        return false;
      }

      if fullscreen {
        // Remember the original window's settings
        // Exclude title bar
        shared_state_lock.standard_frame = Some(NSWindow::contentRectForFrameRect_(
          *self.ns_window,
          NSWindow::frame(*self.ns_window),
        ));
        shared_state_lock.saved_style = Some(self.ns_window.styleMask());
        shared_state_lock.save_presentation_opts = Some(app.presentationOptions_());

        // Tell our window's state that we're in fullscreen
        shared_state_lock.is_simple_fullscreen = true;

        // Simulate pre-Lion fullscreen by hiding the dock and menu bar
        let presentation_options =
          NSApplicationPresentationOptions::NSApplicationPresentationAutoHideDock
            | NSApplicationPresentationOptions::NSApplicationPresentationAutoHideMenuBar;
        app.setPresentationOptions_(presentation_options);

        // Hide the titlebar
        util::toggle_style_mask(
          *self.ns_window,
          *self.ns_view,
          NSWindowStyleMask::NSTitledWindowMask,
          false,
        );

        // Set the window frame to the screen frame size
        let screen = self.ns_window.screen();
        let screen_frame = NSScreen::frame(screen);
        NSWindow::setFrame_display_(*self.ns_window, screen_frame, YES);

        // Fullscreen windows can't be resized, minimized, or moved
        util::toggle_style_mask(
          *self.ns_window,
          *self.ns_view,
          NSWindowStyleMask::NSMiniaturizableWindowMask,
          false,
        );
        util::toggle_style_mask(
          *self.ns_window,
          *self.ns_view,
          NSWindowStyleMask::NSResizableWindowMask,
          false,
        );
        NSWindow::setMovable_(*self.ns_window, NO);

        true
      } else {
        let new_mask = self.saved_style(&mut *shared_state_lock);
        self.set_style_mask_async(new_mask);
        shared_state_lock.is_simple_fullscreen = false;

        if let Some(presentation_opts) = shared_state_lock.save_presentation_opts {
          app.setPresentationOptions_(presentation_opts);
        }

        let frame = shared_state_lock.saved_standard_frame();
        NSWindow::setFrame_display_(*self.ns_window, frame, YES);
        NSWindow::setMovable_(*self.ns_window, YES);

        true
      }
    }
  }

  #[inline]
  fn has_shadow(&self) -> bool {
    unsafe { self.ns_window.hasShadow() == YES }
  }

  #[inline]
  fn set_has_shadow(&self, has_shadow: bool) {
    unsafe {
      self
        .ns_window
        .setHasShadow_(if has_shadow { YES } else { NO })
    }
  }

  #[inline]
  fn set_is_document_edited(&self, edited: bool) {
    unsafe {
      self
        .ns_window
        .setDocumentEdited_(if edited { YES } else { NO })
    }
  }

  #[inline]
  fn is_document_edited(&self) -> bool {
    unsafe {
      let is_document_edited: BOOL = msg_send![*self.ns_window, isDocumentEdited];
      is_document_edited == YES
    }
  }

  #[inline]
  fn set_allows_automatic_window_tabbing(&self, enabled: bool) {
    unsafe {
      NSWindow::setAllowsAutomaticWindowTabbing_(*self.ns_window, if enabled { YES } else { NO })
    }
  }

  #[inline]
  fn allows_automatic_window_tabbing(&self) -> bool {
    unsafe {
      let allows_tabbing: BOOL = NSWindow::allowsAutomaticWindowTabbing(*self.ns_window);
      allows_tabbing == YES
    }
  }

  #[inline]
  fn set_tabbing_identifier(&self, identifier: &str) {
    unsafe {
      let _: () =
        msg_send![*self.ns_window, setTabbingIdentifier: NSString::alloc(nil).init_str(identifier)];
    }
  }

  #[inline]
  fn tabbing_identifier(&self) -> String {
    unsafe {
      let tabbing_identifier = NSWindow::tabbingIdentifier(*self.ns_window);
      ns_string_to_rust(tabbing_identifier)
    }
  }
}

impl Drop for UnownedWindow {
  fn drop(&mut self) {
    trace!("Dropping `UnownedWindow` ({:?})", self as *mut _);
    // Close the window if it has not yet been closed.
    if *self.ns_window != nil {
      unsafe { util::close_async(self.ns_window.clone()) };
    }
  }
}

unsafe fn set_min_inner_size<V: NSWindow + Copy>(window: V, mut min_size: LogicalSize<f64>) {
  let mut current_rect = NSWindow::frame(window);
  let content_rect = NSWindow::contentRectForFrameRect_(window, NSWindow::frame(window));
  // Convert from client area size to window size
  min_size.width += (current_rect.size.width - content_rect.size.width) as f64; // this tends to be 0
  min_size.height += (current_rect.size.height - content_rect.size.height) as f64;
  window.setMinSize_(NSSize {
    width: min_size.width as CGFloat,
    height: min_size.height as CGFloat,
  });
  // If necessary, resize the window to match constraint
  if current_rect.size.width < min_size.width {
    current_rect.size.width = min_size.width;
    window.setFrame_display_(current_rect, NO)
  }
  if current_rect.size.height < min_size.height {
    // The origin point of a rectangle is at its bottom left in Cocoa.
    // To ensure the window's top-left point remains the same:
    current_rect.origin.y += current_rect.size.height - min_size.height;
    current_rect.size.height = min_size.height;
    window.setFrame_display_(current_rect, NO)
  }
}

unsafe fn set_max_inner_size<V: NSWindow + Copy>(window: V, mut max_size: LogicalSize<f64>) {
  let mut current_rect = NSWindow::frame(window);
  let content_rect = NSWindow::contentRectForFrameRect_(window, NSWindow::frame(window));
  // Convert from client area size to window size
  max_size.width += (current_rect.size.width - content_rect.size.width) as f64; // this tends to be 0
  max_size.height += (current_rect.size.height - content_rect.size.height) as f64;
  window.setMaxSize_(NSSize {
    width: max_size.width as CGFloat,
    height: max_size.height as CGFloat,
  });
  // If necessary, resize the window to match constraint
  if current_rect.size.width > max_size.width {
    current_rect.size.width = max_size.width;
    window.setFrame_display_(current_rect, NO)
  }
  if current_rect.size.height > max_size.height {
    // The origin point of a rectangle is at its bottom left in Cocoa.
    // To ensure the window's top-left point remains the same:
    current_rect.origin.y += current_rect.size.height - max_size.height;
    current_rect.size.height = max_size.height;
    window.setFrame_display_(current_rect, NO)
  }
}
