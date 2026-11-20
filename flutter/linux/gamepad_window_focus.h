#ifndef RUSTDESK_GAMEPAD_WINDOW_FOCUS_H_
#define RUSTDESK_GAMEPAD_WINDOW_FOCUS_H_

#include <flutter_linux/flutter_linux.h>
#include <gtk/gtk.h>

void gamepad_window_focus_init(GtkWindow* window, FlEngine* engine);

#endif
