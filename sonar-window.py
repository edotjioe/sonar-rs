#!/usr/bin/env python3
# Sonar windows (WebKitGTK, no browser chrome). One running instance serves both:
#   sonar-window.py           -> the normal mixer window
#   sonar-window.py --toggle  -> show/hide the full-screen overlay (bound to a global hotkey)
#   sonar-window.py --tray    -> the tray icon (autostarted at login): its own small process, without WebKit
# The mixer's OBS button (window.open) opens the OBS page in its own window.
# The window process quits a minute after nothing is showing: idle WebKit holds ~150 MiB.
import json
import os
import sys
import urllib.request

import gi
gi.require_version('Gtk', '3.0')
gi.require_version('Gdk', '3.0')
from gi.repository import Gdk, Gio, GLib, Gtk
TRAY = '--tray' in sys.argv[1:]
if TRAY:
    AppIndicator = None  # tray icon (StatusNotifierItem): Ayatana or the older libappindicator, whichever is installed
    for name in ('AyatanaAppIndicator3', 'AppIndicator3'):
        try:
            gi.require_version(name, '0.1')
            AppIndicator = __import__('gi.repository.' + name, fromlist=[name])
            break
        except (ValueError, ImportError):
            pass
else:
    gi.require_version('WebKit2', '4.1')
    from gi.repository import WebKit2
    try:
        gi.require_version('GtkLayerShell', '0.1')
        from gi.repository import GtkLayerShell
    except (ValueError, ImportError):
        GtkLayerShell = None  # not installed (e.g. stock Bazzite): the overlay falls back to a plain fullscreen window

URL = 'http://localhost:8080/'
OVERLAY_URL = URL + '?overlay=1'
SCRIPT = os.path.abspath(__file__)
IDLE_QUIT_S = 60  # keep the window process this long after the last window closes: reopening stays instant


def script_mtime():
    try:
        return os.stat(SCRIPT).st_mtime
    except OSError:
        return None


def retry(view, _event, uri, err):
    if err.code == WebKit2.NetworkError.CANCELLED:
        return False  # we navigated away ourselves (e.g. hiding the overlay)
    # backend may still be booting (e.g. right after login): retry instead of showing an error page
    GLib.timeout_add_seconds(1, lambda: view.get_uri() == uri and view.load_uri(uri))
    return True


def hotkey_opens():
    """'overlay' or 'window', from the backend's preferences; overlay when it can't be reached."""
    try:
        with urllib.request.urlopen(URL + 'api/preferences', timeout=0.5) as r:
            return json.load(r).get('hotkeyOpens', 'overlay')
    except Exception:
        return 'overlay'


def web_view():
    view = WebKit2.WebView()
    view.connect('load-failed', retry)
    return view


class Tray(Gtk.Application):
    """Just the tray icon; its menu launches this script, so the windows run (and quit) in their own process."""

    def __init__(self):
        super().__init__(application_id='dev.sonar.tray')

    def do_activate(self):
        if not AppIndicator:
            return  # no tray library: nothing to show, so don't linger
        self.hold()
        menu = Gtk.Menu()
        for label, args in (('Open Sonar', []), ('Overlay', ['--toggle'])):
            item = Gtk.MenuItem(label=label)
            item.connect('activate', lambda _item, a=args: Gio.Subprocess.new([sys.executable, SCRIPT, *a], Gio.SubprocessFlags.NONE))
            menu.append(item)
        item = Gtk.MenuItem(label='Quit')
        item.connect('activate', lambda _item: self.quit())
        menu.append(item)
        menu.show_all()
        self.tray = AppIndicator.Indicator.new('dev.sonar', 'sonar', AppIndicator.IndicatorCategory.APPLICATION_STATUS)
        self.tray.set_title('Sonar')
        self.tray.set_menu(menu)
        self.tray.set_status(AppIndicator.IndicatorStatus.ACTIVE)


class Sonar(Gtk.Application):
    def __init__(self):
        super().__init__(application_id='dev.sonar', flags=Gio.ApplicationFlags.HANDLES_COMMAND_LINE)
        self.main = self.overlay = self.overlay_view = self.obs = None
        self.layer_shell = False  # the overlay is a layer-shell surface (its keyboard mode can change)
        self.started_mtime = script_mtime()  # this file as loaded; see restart_if_updated
        self.idle_timer = None

    def showing(self):
        return bool(self.main or self.obs or (self.overlay and self.overlay.get_visible()))

    def schedule_idle_quit(self, *_):
        """Nothing on screen: quit in a minute, unless a window opens meanwhile (then this is called again)."""
        if self.idle_timer:
            GLib.source_remove(self.idle_timer)
        def quit_if_idle():
            self.idle_timer = None
            if not self.showing():
                self.quit()
            return False
        self.idle_timer = GLib.timeout_add_seconds(IDLE_QUIT_S, quit_if_idle)

    def do_command_line(self, cmdline):
        # a second launch doesn't start a new process: its arguments arrive here, in the running one
        if self.restart_if_updated(cmdline.get_arguments()[1:]):
            return 0
        if '--toggle' in cmdline.get_arguments()[1:]:
            if hotkey_opens() == 'window':
                self.toggle_main()
            else:
                self.toggle_overlay()
        else:
            self.show_main()
        return 0

    def restart_if_updated(self, args):
        """
        This process lives for the whole session, so after an update it would keep running the old
        script against the new page. When the file has changed and no window is showing, replace the
        process with the new script, which then handles this launch's arguments itself.
        """
        mtime = script_mtime()
        if mtime is None or mtime == self.started_mtime:
            return False
        if self.showing():
            return False  # don't close what's on screen; restart on a later launch
        def exec_new():
            # let the launching process get its reply first; exec closes our D-Bus connection, which
            # frees the app id for the new process
            conn = self.get_dbus_connection()
            if conn:
                conn.flush_sync(None)
            os.execv(sys.executable, [sys.executable, SCRIPT, *args])
        GLib.timeout_add(100, exec_new)
        return True

    def show_main(self):
        if not self.main:
            self.main = Gtk.ApplicationWindow(application=self, title='Sonar')
            self.main.set_default_size(900, 600)
            view = web_view()
            view.connect('create', self.open_obs)
            view.load_uri(URL)
            self.main.add(view)
            self.main.connect('destroy', lambda *_: (setattr(self, 'main', None), self.schedule_idle_quit()))
            self.main.show_all()
        self.main.present()

    def open_obs(self, view, _action):
        """window.open from the mixer (its OBS button): one OBS window, reused."""
        if self.obs:
            self.obs.present()
            return None  # cancels creating another view
        # the new view must be related to the opener, or WebKit won't hand it the popup
        child = WebKit2.WebView.new_with_related_view(view)
        child.connect('load-failed', retry)
        win = Gtk.Window(application=self, title='Sonar — OBS')
        win.set_default_size(620, 560)
        win.add(child)
        child.connect('ready-to-show', lambda *_: win.show_all())
        child.connect('close', lambda *_: win.destroy())  # window.close()
        win.connect('destroy', lambda *_: (setattr(self, 'obs', None), self.schedule_idle_quit()))
        self.obs = win
        return child

    def toggle_main(self):
        if self.main and self.main.is_active():
            self.main.close()
        else:
            self.show_main()

    def toggle_overlay(self):
        if not self.overlay:
            self.make_overlay()
        if self.overlay.get_visible():
            self.hide_overlay()
        else:
            # a fresh view (and WebKit page process) each time: ~130 ms to the mixer with data
            self.overlay_view = self.make_overlay_view()
            self.overlay.add(self.overlay_view)
            self.overlay_view.load_uri(OVERLAY_URL)
            self.overlay.show_all()

    def on_message(self, _manager, result):
        # from the overlay page: 'close' (backdrop clicked), 'keyboard' / 'nokeyboard' (typing starts / ends)
        msg = result.get_js_value().to_string()
        if msg == 'close':
            self.hide_overlay()
        elif msg in ('keyboard', 'nokeyboard'):
            self.set_keyboard(msg == 'keyboard')

    def set_keyboard(self, on):
        if self.layer_shell:
            GtkLayerShell.set_keyboard_mode(
                self.overlay, GtkLayerShell.KeyboardMode.EXCLUSIVE if on else GtkLayerShell.KeyboardMode.NONE)

    def hide_overlay(self, *_):
        self.set_keyboard(False)
        self.overlay.hide()
        # destroy the page, not just blank it: stops its polling (level meters, the mic) and ends its
        # WebKit page process, ~100 MiB that would otherwise sit idle while the overlay is closed
        if self.overlay_view:
            self.overlay.remove(self.overlay_view)
            self.overlay_view.destroy()
            self.overlay_view = None
        self.schedule_idle_quit()

    def make_overlay(self):
        # the hidden overlay window keeps this process alive for IDLE_QUIT_S (no Python/GTK cold start on a
        # quick re-toggle); its page is created on show and destroyed on hide
        win = Gtk.Window(application=self, title='Sonar overlay')
        if GtkLayerShell and GtkLayerShell.is_supported():
            # layer-shell (KDE, wlroots compositors): a layer above everything, fullscreen games included
            GtkLayerShell.init_for_window(win)
            GtkLayerShell.set_namespace(win, 'sonar')
            GtkLayerShell.set_layer(win, GtkLayerShell.Layer.OVERLAY)
            for edge in (GtkLayerShell.Edge.TOP, GtkLayerShell.Edge.BOTTOM,
                         GtkLayerShell.Edge.LEFT, GtkLayerShell.Edge.RIGHT):
                GtkLayerShell.set_anchor(win, edge, True)
            GtkLayerShell.set_exclusive_zone(win, -1)  # cover panels too
            # no keyboard focus by default: fullscreen games (SDL, Wine/Proton) minimize when they lose it.
            # The mouse still works (pointer focus moves to the overlay, which also breaks a game's
            # pointer lock); close with the hotkey or a backdrop click. The page asks for the keyboard
            # only while you type in it (see on_message), so only typing risks minimizing the game.
            GtkLayerShell.set_keyboard_mode(win, GtkLayerShell.KeyboardMode.NONE)
            self.layer_shell = True
        else:
            # e.g. GNOME, or gtk-layer-shell not installed: best effort (a fullscreen game may minimize)
            win.fullscreen()
            win.set_keep_above(True)
        win.set_visual(win.get_screen().get_rgba_visual())
        win.set_app_paintable(True)
        self.overlay = win

    def make_overlay_view(self):
        view = web_view()
        view.set_background_color(Gdk.RGBA(0, 0, 0, 0))
        manager = view.get_user_content_manager()
        manager.connect('script-message-received::sonar', self.on_message)
        manager.register_script_message_handler('sonar')
        return view


# Wayland app_id = prgname; it must match dev.sonar.desktop for the taskbar to show the icon
GLib.set_prgname('dev.sonar')
# native popups (dropdown lists) use the GTK theme, not page CSS; Breeze ignores prefer-dark, so force the dark variant
Gtk.Settings.get_default().set_property('gtk-theme-name', 'Breeze-Dark')
Gtk.Window.set_default_icon_name('sonar')
# the tray takes no options: GApplication would reject --tray
Tray().run(sys.argv[:1]) if TRAY else Sonar().run(sys.argv)
