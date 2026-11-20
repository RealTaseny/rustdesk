#include "gamepad_window_focus.h"

#ifdef GDK_WINDOWING_X11
#include <gdk/gdkx.h>
#endif

namespace {

struct GamepadWindowFocus {
  GtkWindow* window;
  FlEventChannel* channel;
  bool listening = false;
  bool focused = false;
#ifdef GDK_WINDOWING_X11
  bool watching_x11 = false;
  bool added_core_focus = false;
#endif
};

bool is_focused(GtkWindow* window) {
  GtkWidget* widget = GTK_WIDGET(window);
  if (!gtk_widget_get_mapped(widget)) return false;
#ifdef GDK_WINDOWING_X11
  GdkWindow* gdk_window = gtk_widget_get_window(widget);
  if (GDK_IS_X11_WINDOW(gdk_window)) {
    GdkDisplay* display = gdk_window_get_display(gdk_window);
    Display* xdisplay = GDK_DISPLAY_XDISPLAY(display);
    Window toplevel = GDK_WINDOW_XID(gdk_window);
    Window focus;
    int revert;
    if (!XGetInputFocus(xdisplay, &focus, &revert)) return false;
    if (focus == toplevel) return true;

    gdk_x11_display_error_trap_push(display);
    while (focus != None && focus != PointerRoot && focus != toplevel) {
      Window root, parent, *children;
      unsigned int nchildren;
      if (!XQueryTree(xdisplay, focus, &root, &parent, &children, &nchildren)) {
        break;
      }
      if (children != nullptr) XFree(children);
      focus = parent;
    }
    gdk_x11_display_error_trap_pop_ignored(display);
    return focus == toplevel;
  }
#endif
  return gtk_window_is_active(window);
}

void report_focus(GamepadWindowFocus* state, bool initial = false) {
  if (!state->listening) return;
  bool focused = is_focused(state->window);
  if (!initial && focused == state->focused) return;
  state->focused = focused;
  g_autoptr(FlValue) event = fl_value_new_bool(focused);
  g_autoptr(GError) error = nullptr;
  if (!fl_event_channel_send(state->channel, event, nullptr, &error)) {
    g_debug("Cannot report gamepad window focus: %s", error->message);
  }
}

#ifdef GDK_WINDOWING_X11
GdkFilterReturn on_x11_focus(GdkXEvent* raw, GdkEvent*, gpointer data) {
  auto* state = static_cast<GamepadWindowFocus*>(data);
  auto* event = static_cast<XEvent*>(raw);
  if (event->type != FocusIn && event->type != FocusOut) {
    return GDK_FILTER_CONTINUE;
  }
  if (event->xfocus.mode == NotifyNormal ||
      event->xfocus.mode == NotifyWhileGrabbed) {
    report_focus(state);
  }
  return state->added_core_focus ? GDK_FILTER_REMOVE : GDK_FILTER_CONTINUE;
}

void watch_x11_focus(GtkWidget* widget, gpointer data) {
  auto* state = static_cast<GamepadWindowFocus*>(data);
  GdkWindow* window = gtk_widget_get_window(widget);
  if (!state->listening || state->watching_x11 || !GDK_IS_X11_WINDOW(window)) {
    return;
  }
  Display* display = GDK_WINDOW_XDISPLAY(window);
  Window xid = GDK_WINDOW_XID(window);
  XWindowAttributes attrs;
  if (!XGetWindowAttributes(display, xid, &attrs)) return;
  // XI2 focus can vanish under the keyboard grab. Core focus still reports
  // genuine focus changes as NotifyWhileGrabbed.
  if (!(attrs.your_event_mask & FocusChangeMask)) {
    XSelectInput(display, xid, attrs.your_event_mask | FocusChangeMask);
    state->added_core_focus = true;
  }
  gdk_window_add_filter(window, on_x11_focus, state);
  state->watching_x11 = true;
}

void unwatch_x11_focus(GtkWidget* widget, gpointer data) {
  auto* state = static_cast<GamepadWindowFocus*>(data);
  if (!state->watching_x11) return;
  gdk_window_remove_filter(gtk_widget_get_window(widget), on_x11_focus, state);
  state->watching_x11 = false;
  state->added_core_focus = false;
}
#endif

void on_visibility_changed(GtkWidget*, gpointer data) {
  report_focus(static_cast<GamepadWindowFocus*>(data));
}

void on_active_changed(GtkWindow*, GParamSpec*, gpointer data) {
  auto* state = static_cast<GamepadWindowFocus*>(data);
#ifdef GDK_WINDOWING_X11
  if (state->watching_x11) return;
#endif
  report_focus(state);
}

FlMethodErrorResponse* on_listen(FlEventChannel*, FlValue*, gpointer data) {
  auto* state = static_cast<GamepadWindowFocus*>(data);
  state->listening = true;
#ifdef GDK_WINDOWING_X11
  watch_x11_focus(GTK_WIDGET(state->window), state);
#endif
  report_focus(state, true);
  return nullptr;
}

FlMethodErrorResponse* on_cancel(FlEventChannel*, FlValue*, gpointer data) {
  static_cast<GamepadWindowFocus*>(data)->listening = false;
  return nullptr;
}

void destroy_state(gpointer data) {
  auto* state = static_cast<GamepadWindowFocus*>(data);
  fl_event_channel_set_stream_handlers(state->channel, nullptr, nullptr,
                                       nullptr, nullptr);
  g_object_unref(state->channel);
  delete state;
}

}  // namespace

void gamepad_window_focus_init(GtkWindow* window, FlEngine* engine) {
  g_autoptr(FlStandardMethodCodec) codec = fl_standard_method_codec_new();
  auto* channel = fl_event_channel_new(fl_engine_get_binary_messenger(engine),
      "org.rustdesk.rustdesk/gamepad_window_focus", FL_METHOD_CODEC(codec));
  auto* state = new GamepadWindowFocus{window, channel};
  g_object_set_data_full(G_OBJECT(window), "gamepad-window-focus", state,
                         destroy_state);
  fl_event_channel_set_stream_handlers(channel, on_listen, on_cancel, state,
                                       nullptr);
  g_signal_connect_after(window, "map", G_CALLBACK(on_visibility_changed), state);
  g_signal_connect_after(window, "unmap", G_CALLBACK(on_visibility_changed), state);
  g_signal_connect(window, "notify::is-active", G_CALLBACK(on_active_changed),
                   state);
#ifdef GDK_WINDOWING_X11
  g_signal_connect_after(window, "realize", G_CALLBACK(watch_x11_focus), state);
  g_signal_connect(window, "unrealize", G_CALLBACK(unwatch_x11_focus), state);
#endif
}
