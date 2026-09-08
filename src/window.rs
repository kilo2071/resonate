use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use crate::audio::AudioEngine;
use crate::config::Config;
use crate::ui::{ResonateEffectsPage, ResonateSettingsPage, ResonateSoundTile, ResonateSoundboardPage};

const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "ogg", "flac", "aac", "m4a", "opus", "wma"];

fn is_audio_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

mod imp {
    use super::*;

    #[derive(gtk::CompositeTemplate)]
    #[template(resource = "/io/github/kilo2071/Resonate/ui/window.ui")]
    pub struct ResonateWindow {
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub add_sound_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub soundboard_page: TemplateChild<ResonateSoundboardPage>,
        #[template_child]
        pub effects_page: TemplateChild<ResonateEffectsPage>,
        #[template_child]
        pub settings_page: TemplateChild<ResonateSettingsPage>,

        pub config: RefCell<Config>,
        pub audio_engine: RefCell<Option<AudioEngine>>,
        /// True once the user chose Quit (vs. just closing → hide to background).
        pub force_quit: Cell<bool>,
        /// Tray handle, for pushing the preset list into the tray menu.
        pub tray: RefCell<Option<crate::tray::TrayHandle>>,
        /// Last preset name pushed to the tray/effects page. Purely a memo of
        /// what the UI is showing — the real answer is derived from the chain by
        /// `Config::active_preset()` — so a slider drag only touches D-Bus when
        /// the marked preset actually changes.
        pub active_preset: RefCell<Option<String>>,
    }

    impl Default for ResonateWindow {
        fn default() -> Self {
            Self {
                toast_overlay: Default::default(),
                add_sound_button: Default::default(),
                soundboard_page: Default::default(),
                effects_page: Default::default(),
                settings_page: Default::default(),
                config: RefCell::new(Config::load()),
                audio_engine: RefCell::new(None),
                force_quit: Cell::new(false),
                tray: RefCell::new(None),
                active_preset: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ResonateWindow {
        const NAME: &'static str = "ResonateWindow";
        type Type = super::ResonateWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            ResonateSoundTile::ensure_type();
            ResonateSoundboardPage::ensure_type();
            ResonateEffectsPage::ensure_type();
            ResonateSettingsPage::ensure_type();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ResonateWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let win = self.obj();

            match AudioEngine::new() {
                Ok(mut engine) => {
                    let cfg = self.config.borrow().clone();
                    engine.set_polyphonic(cfg.polyphonic);
                    engine.set_monitor_volume(cfg.monitor_volume);
                    engine.set_monitor_enabled(cfg.monitor_enabled);
                    engine.set_mic_volume(cfg.mic_volume);
                    engine.set_soundboard_mic_volume(cfg.soundboard_mic_volume);
                    *self.audio_engine.borrow_mut() = Some(engine);
                }
                Err(e) => log::error!("Audio engine init failed: {}", e),
            }

            // Start virtual mic device
            {
                let cfg = self.config.borrow().clone();
                if cfg.virtual_device_enabled {
                    // Build the mic-effects chain and share it with the PW thread.
                    let effects = match self.audio_engine.borrow().as_ref() {
                        Some(e) => {
                            e.rebuild_effects(&cfg.effects_chain);
                            e.effects_handle()
                        }
                        None => std::sync::Arc::new(std::sync::Mutex::new(
                            crate::plugins::host::PluginChain::new(),
                        )),
                    };
                    match crate::audio::virtual_device::start(
                        &cfg.virtual_device_name,
                        &cfg.input_device_name,
                        effects,
                        cfg.mic_volume,
                    ) {
                        Ok(dev) => {
                            if let Some(engine) = self.audio_engine.borrow_mut().as_mut() {
                                engine.set_virtual_device(dev);
                            }
                            // After PipeWire registers the node (~1 s), set it as default input.
                            glib::timeout_add_local_once(
                                std::time::Duration::from_millis(1500),
                                glib::clone!(
                                    #[weak]
                                    win,
                                    move || {
                                        win.set_virtual_mic_as_default_input();
                                    }
                                ),
                            );
                        }
                        Err(e) => log::error!("Virtual device start failed: {}", e),
                    }
                }
            }

            win.register_actions();
            win.setup_audio();
            win.setup_effects();
            win.sync_settings_ui();
            win.scan_sounds_folder();

            // Closing the window hides it to the background so the mic effects
            // keep running (this is what makes Resonate an Easy Effects-style
            // background processor). Real teardown happens only on Quit, which
            // sets `force_quit` and drops the PipeWire thread (bridge + streams).
            win.connect_close_request(glib::clone!(
                #[weak(rename_to = w)]
                win,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_| {
                    if w.imp().force_quit.get() {
                        if let Some(e) = w.imp().audio_engine.borrow_mut().as_mut() {
                            e.virtual_device = None;
                        }
                        glib::Propagation::Proceed
                    } else {
                        w.set_visible(false);
                        glib::Propagation::Stop
                    }
                }
            ));

            // Global hotkeys via the GlobalShortcuts portal. Digits accumulate
            // into a tile number (Ctrl+Alt+5 then 6 → tile 56) committed after a
            // short pause; Numpad Enter stops everything.
            let hk_rx = crate::hotkeys::spawn();
            let pending_digits: std::rc::Rc<RefCell<String>> =
                std::rc::Rc::new(RefCell::new(String::new()));
            let commit_source: std::rc::Rc<RefCell<Option<glib::SourceId>>> =
                std::rc::Rc::new(RefCell::new(None));
            glib::timeout_add_local(
                std::time::Duration::from_millis(100),
                glib::clone!(
                    #[weak]
                    win,
                    #[upgrade_or]
                    glib::ControlFlow::Break,
                    move || {
                        while let Ok(ev) = hk_rx.try_recv() {
                            match ev {
                                crate::hotkeys::HotkeyEvent::StopAll => {
                                    pending_digits.borrow_mut().clear();
                                    if let Some(id) = commit_source.borrow_mut().take() {
                                        id.remove();
                                    }
                                    win.stop_all_playback();
                                }
                                crate::hotkeys::HotkeyEvent::Digit(d) => {
                                    pending_digits.borrow_mut().push((b'0' + d) as char);
                                    // Restart the commit timer on every digit.
                                    if let Some(id) = commit_source.borrow_mut().take() {
                                        id.remove();
                                    }
                                    let digits = pending_digits.clone();
                                    let source = commit_source.clone();
                                    let id = glib::timeout_add_local_once(
                                        std::time::Duration::from_millis(600),
                                        glib::clone!(
                                            #[weak]
                                            win,
                                            move || {
                                                *source.borrow_mut() = None;
                                                let number =
                                                    std::mem::take(&mut *digits.borrow_mut());
                                                if let Ok(n) = number.parse::<usize>() {
                                                    if n >= 1 {
                                                        if let Some((path, volume)) = win
                                                            .imp()
                                                            .soundboard_page
                                                            .sound_at_index(n - 1)
                                                        {
                                                            win.trigger_play(&path, volume, false);
                                                        }
                                                    }
                                                }
                                            }
                                        ),
                                    );
                                    *commit_source.borrow_mut() = Some(id);
                                }
                            }
                        }
                        glib::ControlFlow::Continue
                    }
                ),
            );

            // Tray indicator (SNI). Tray clicks arrive off the GTK thread; poll
            // the channel on the main loop and act here.
            let (rx, tray) = crate::tray::spawn();
            *self.tray.borrow_mut() = Some(tray);
            win.refresh_preset_list();
            glib::timeout_add_local(
                std::time::Duration::from_millis(150),
                glib::clone!(
                    #[weak]
                    win,
                    #[upgrade_or]
                    glib::ControlFlow::Break,
                    move || {
                        while let Ok(cmd) = rx.try_recv() {
                            match cmd {
                                crate::tray::TrayCmd::Show => {
                                    win.set_visible(true);
                                    win.present();
                                }
                                crate::tray::TrayCmd::Quit => win.do_quit(),
                                crate::tray::TrayCmd::LoadPreset(name) => {
                                    win.apply_preset(&name)
                                }
                            }
                        }
                        glib::ControlFlow::Continue
                    }
                ),
            );
        }
    }

    impl WidgetImpl for ResonateWindow {}
    impl WindowImpl for ResonateWindow {}
    impl ApplicationWindowImpl for ResonateWindow {}
    impl AdwApplicationWindowImpl for ResonateWindow {}
}

glib::wrapper! {
    pub struct ResonateWindow(ObjectSubclass<imp::ResonateWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap,
                    gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget,
                    gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl ResonateWindow {
    pub fn new(app: &impl IsA<adw::Application>) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    fn register_actions(&self) {
        let add_files = gio::SimpleAction::new("add-files", None);
        add_files.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| win.open_file_dialog()
        ));
        self.add_action(&add_files);

        let choose_folder = gio::SimpleAction::new("choose-sounds-folder", None);
        choose_folder.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| win.choose_sounds_folder()
        ));
        self.add_action(&choose_folder);
    }

    fn setup_audio(&self) {
        let page = self.imp().soundboard_page.get();

        // Set slider positions from config
        {
            let cfg = self.imp().config.borrow().clone();
            page.set_initial_volumes(cfg.mic_volume, cfg.monitor_volume, cfg.soundboard_mic_volume);
        }

        // Mic / virtual device volume slider
        page.connect_mic_volume_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |v| {
                win.imp().config.borrow_mut().mic_volume = v;
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.set_mic_volume(v);
                }
            }
        ));

        // Monitor (headphone) volume slider
        page.connect_monitor_volume_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |v| {
                win.imp().config.borrow_mut().monitor_volume = v;
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.set_monitor_volume(v);
                }
            }
        ));

        // Soundboard → virtual mic level (how loud sounds are for others)
        page.connect_soundboard_volume_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |v| {
                win.imp().config.borrow_mut().soundboard_mic_volume = v;
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.set_soundboard_mic_volume(v);
                }
            }
        ));

        // Play button on tile → polyphonic/sequential start
        page.set_play_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |path, volume| win.trigger_play(&path, volume, false)
        ));

        // Cue button → always queue regardless of polyphonic mode
        page.set_cue_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |path, volume| win.trigger_play(&path, volume, true)
        ));

        // Preview button (bottom-left): monitor-only playback of the selected
        // sound from its one-shot scrub point (or persistent marker).
        page.connect_preview(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move || win.toggle_lcd_preview()
        ));

        // LCD waveform scrub finished: if a preview runs, chase the new point.
        page.set_scrub_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |path, secs| {
                let previewing = win
                    .imp()
                    .audio_engine
                    .borrow_mut()
                    .as_mut()
                    .and_then(|e| e.preview_position())
                    .is_some();
                if previewing {
                    let params = win.preview_params_for(&path, Some(secs));
                    if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                        if let Err(e) = engine.preview(&path, params) {
                            log::warn!("Preview failed: {e}");
                        }
                    }
                }
            }
        ));

        // Tile drag-reorder → persist the new order.
        page.set_reorder_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |order| {
                win.imp().config.borrow_mut().sound_order = order;
                win.imp().config.borrow().save();
            }
        ));

        // Per-tile volume slider → persist + live-apply to playing instances
        page.set_volume_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |path, fraction| {
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    cfg.sound_settings_mut(&path).volume = fraction;
                }
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.set_sound_volume(&path, fraction);
                }
            }
        ));

        // Edit… menu item → waveform editor dialog
        page.set_edit_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |path| win.open_sound_editor(path)
        ));

        // Rename / Remove callbacks
        page.set_rename_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |path| win.show_rename_dialog(path)
        ));

        page.set_remove_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |path| win.confirm_remove_sound(path)
        ));

        // Stop all
        page.connect_stop_all(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move || win.stop_all_playback()
        ));

        // Play/pause: starts the selected sound when idle, else pause/resume.
        page.connect_play_pause(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move || {
                let playing = win
                    .imp()
                    .audio_engine
                    .borrow()
                    .as_ref()
                    .map(|e| e.is_anything_playing())
                    .unwrap_or(false);
                if playing {
                    if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                        engine.toggle_pause();
                    }
                } else if let Some(path) = win.imp().soundboard_page.selected_path() {
                    let volume = win.imp().config.borrow().sound_settings(&path).volume;
                    win.trigger_play(&path, volume, false);
                }
            }
        ));

        // Progress-bar scrub while playing → seek.
        page.set_seek_callback(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |frac| {
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.seek_playing(frac);
                }
            }
        ));

        // Skip: remove first queued item and try to start it immediately
        page.connect_skip(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move || {
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.skip_queue();
                }
            }
        ));

        // Progress timer — 100 ms
        glib::timeout_add_local(
            std::time::Duration::from_millis(100),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let (playing, queue, preview_pos, mic_level, scope) = {
                        if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                            let (p, q) = engine.tick();
                            (p, q, engine.preview_position(), engine.mic_level(), engine.scope())
                        } else {
                            (vec![], vec![], None, 0.0, vec![])
                        }
                    };

                    let sb = win.imp().soundboard_page.get();
                    sb.update_playback_display(&playing, &queue);
                    // Play also starts the selected sound, so selection alone enables it.
                    sb.set_play_pause_sensitive(!playing.is_empty() || sb.selected_path().is_some());
                    sb.set_preview_pos(preview_pos);
                    sb.set_preview_active(preview_pos.is_some());
                    sb.set_scope(&scope);
                    win.imp().effects_page.set_mic_level(mic_level);

                    let active = win
                        .imp()
                        .audio_engine
                        .borrow()
                        .as_ref()
                        .map(|e| e.is_anything_playing() && !e.is_all_paused())
                        .unwrap_or(false);
                    sb.set_play_pause_icon(active);

                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// Start offset (secs) for the next play of `path`, honouring the start
    /// mode. A legacy "just once" marker is consumed here and reverts to Off.
    fn take_start_point(&self, path: &std::path::Path) -> f32 {
        use crate::config::StartMode;
        let (start, consumed) = {
            let cfg = self.imp().config.borrow();
            let s = cfg.sound_settings(path);
            (s.effective_start(), s.start_mode == StartMode::Once)
        };
        if consumed {
            self.imp().config.borrow_mut().sound_settings_mut(path).start_mode = StartMode::Off;
            self.imp().config.borrow().save();
        }
        start
    }

    /// Full playback shape for `path`: LCD one-shot scrub wins over the
    /// persistent marker; trim + fades come from the saved settings.
    fn play_params_for(&self, path: &std::path::Path, volume: f32) -> crate::audio::engine::PlayParams {
        let s = self.imp().config.borrow().sound_settings(path);
        let start = self
            .imp()
            .soundboard_page
            .consume_one_shot(path)
            .unwrap_or_else(|| self.take_start_point(path));
        crate::audio::engine::PlayParams {
            volume,
            start_secs: start,
            end_secs: s.end_secs,
            fade_in_ms: s.fade_in_ms,
            fade_out_ms: s.fade_out_ms,
        }
    }

    /// Like `play_params_for` but non-consuming (previews must not eat the
    /// one-shot marker). `start_override` takes precedence when given.
    fn preview_params_for(&self, path: &std::path::Path, start_override: Option<f32>) -> crate::audio::engine::PlayParams {
        let s = self.imp().config.borrow().sound_settings(path);
        let start = start_override
            .or_else(|| self.imp().soundboard_page.peek_one_shot(path))
            .unwrap_or_else(|| s.effective_start());
        crate::audio::engine::PlayParams {
            volume: s.volume,
            start_secs: start,
            end_secs: s.end_secs,
            fade_in_ms: s.fade_in_ms,
            fade_out_ms: s.fade_out_ms,
        }
    }

    /// Play or cue a sound with its full playback shape (tiles + hotkeys).
    fn trigger_play(&self, path: &std::path::Path, volume: f32, cue: bool) {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Sound").to_string();
        let params = self.play_params_for(path, volume);
        if let Some(engine) = self.imp().audio_engine.borrow_mut().as_mut() {
            if cue {
                engine.cue(path, &name, params);
            } else if let Err(e) = engine.play(path, &name, params) {
                log::error!("Playback failed: {}", e);
            }
        }
    }

    /// Stop everything (Stop button and the global stop hotkey).
    fn stop_all_playback(&self) {
        if let Some(engine) = self.imp().audio_engine.borrow_mut().as_mut() {
            engine.stop_all();
        }
        let sb = self.imp().soundboard_page.get();
        sb.update_playback_display(&[], &[]);
        sb.set_play_pause_sensitive(sb.selected_path().is_some());
        sb.set_play_pause_icon(false);
    }

    /// Toggle the LCD preview of the selected sound (monitor only).
    fn toggle_lcd_preview(&self) {
        let Some(path) = self.imp().soundboard_page.selected_path() else { return };
        let previewing = self
            .imp()
            .audio_engine
            .borrow_mut()
            .as_mut()
            .and_then(|e| e.preview_position())
            .is_some();
        if previewing {
            if let Some(engine) = self.imp().audio_engine.borrow_mut().as_mut() {
                engine.stop_preview();
            }
        } else {
            let params = self.preview_params_for(&path, None);
            if let Some(engine) = self.imp().audio_engine.borrow_mut().as_mut() {
                if let Err(e) = engine.preview(&path, params) {
                    log::warn!("Preview failed: {e}");
                }
            }
        }
    }

    /// Present the waveform editor for a sound (scrub, start marker, preview).
    fn open_sound_editor(&self, path: PathBuf) {
        use crate::ui::sound_editor::{self, EditorHooks};

        let initial = self.imp().config.borrow().sound_settings(&path);
        let w1 = self.downgrade();
        let w2 = self.downgrade();
        let w3 = self.downgrade();
        let w4 = self.downgrade();

        let hooks = EditorHooks {
            preview: Box::new(move |p, params| {
                let Some(win) = w1.upgrade() else { return };
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    if let Err(e) = engine.preview(p, params) {
                        log::warn!("Preview failed: {e}");
                    }
                }
            }),
            stop_preview: Box::new(move || {
                let Some(win) = w2.upgrade() else { return };
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.stop_preview();
                }
            }),
            preview_pos: Box::new(move || {
                let win = w3.upgrade()?;
                let mut engine = win.imp().audio_engine.borrow_mut();
                engine.as_mut()?.preview_position()
            }),
            save: Box::new(move |p, settings| {
                let Some(win) = w4.upgrade() else { return };
                let volume = settings.volume;
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    *cfg.sound_settings_mut(p) = settings;
                }
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.set_sound_volume(p, volume);
                }
                win.imp().soundboard_page.set_tile_volume(&p.to_path_buf(), volume);
            }),
        };

        sound_editor::present(self, path, initial, hooks);
    }

    // ── File picker ──────────────────────────────────────────────────────────

    fn audio_filter() -> gtk::FileFilter {
        let f = gtk::FileFilter::new();
        f.set_name(Some("Audio files"));
        f.add_mime_type("audio/*");
        for ext in AUDIO_EXTENSIONS {
            f.add_suffix(ext);
        }
        f
    }

    fn open_file_dialog(&self) {
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&Self::audio_filter());

        let dialog = gtk::FileDialog::builder()
            .title("Add Sounds")
            .accept_label("Add")
            .filters(&filters)
            .build();

        dialog.open_multiple(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |result| {
                    if let Ok(files) = result {
                        for i in 0..files.n_items() {
                            if let Some(file) = files.item(i).and_downcast::<gio::File>() {
                                if let Some(path) = file.path() {
                                    win.add_sound_file(path);
                                }
                            }
                        }
                    }
                }
            ),
        );
    }

    fn choose_sounds_folder(&self) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose Sounds Folder")
            .accept_label("Select")
            .build();

        dialog.select_folder(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |result| {
                    if let Ok(folder) = result {
                        if let Some(path) = folder.path() {
                            let display = path.to_string_lossy().to_string();
                            win.imp().config.borrow_mut().sounds_folder = path;
                            win.imp().config.borrow().save();
                            win.imp().settings_page.set_sounds_folder_label(&display);
                        }
                    }
                }
            ),
        );
    }

    fn add_sound_file(&self, path: PathBuf) {
        if !is_audio_file(&path) {
            return;
        }
        let config = self.imp().config.borrow().clone();
        let final_path = if config.move_files_to_folder {
            self.move_to_sounds_folder(path, &config.sounds_folder)
        } else {
            path
        };
        let volume = config.sound_settings(&final_path).volume;
        let is_new = !config
            .sounds
            .contains_key(&crate::config::Config::sound_key(&final_path));
        self.imp()
            .soundboard_page
            .add_sound_from_path(final_path.clone(), volume);
        if is_new {
            self.normalize_new_sound(final_path);
        }
    }

    /// Peak-normalise a freshly imported sound: analyse in the background and
    /// pre-set its volume so its peak lands near -1 dBFS (only ever turning
    /// loud files down — the per-sound gain cannot exceed 1.0).
    fn normalize_new_sound(&self, path: PathBuf) {
        let slot = crate::audio::wave::load_async(&path);
        let win_weak = self.downgrade();
        glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
            let Some(win) = win_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let ready = slot.lock().ok().and_then(|mut g| g.take());
            if let Some(wave) = ready {
                if wave.peak > 0.0 {
                    let target = 0.89f32; // ≈ -1 dBFS
                    let vol = (target / wave.peak).clamp(0.0, 1.0);
                    win.imp().config.borrow_mut().sound_settings_mut(&path).volume = vol;
                    win.imp().config.borrow().save();
                    win.imp().soundboard_page.set_tile_volume(&path, vol);
                    if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                        engine.set_sound_volume(&path, vol);
                    }
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    fn scan_sounds_folder(&self) {
        let folder = self.imp().config.borrow().sounds_folder.clone();
        let Ok(entries) = std::fs::read_dir(&folder) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_audio_file(p))
            .collect();
        paths.sort();
        // Apply the saved tile order; files not in it keep alphabetical order
        // after the ordered ones.
        let order = self.imp().config.borrow().sound_order.clone();
        let rank = |p: &PathBuf| {
            let key = crate::config::Config::sound_key(p);
            order.iter().position(|k| *k == key).unwrap_or(usize::MAX)
        };
        paths.sort_by_key(|p| (rank(p), p.clone()));
        for path in paths {
            let volume = self.imp().config.borrow().sound_settings(&path).volume;
            self.imp().soundboard_page.add_sound_from_path(path, volume);
        }
    }

    fn move_to_sounds_folder(&self, src: PathBuf, dest_dir: &PathBuf) -> PathBuf {
        if src.parent().map(|p| p == dest_dir).unwrap_or(false) {
            return src;
        }
        if std::fs::create_dir_all(dest_dir).is_err() {
            return src;
        }
        let Some(file_name) = src.file_name() else {
            return src;
        };
        let dest = dest_dir.join(file_name);
        if std::fs::rename(&src, &dest).is_ok() {
            return dest;
        }
        if std::fs::copy(&src, &dest).is_ok() {
            let _ = std::fs::remove_file(&src);
            return dest;
        }
        src
    }

    // ── Rename ───────────────────────────────────────────────────────────────

    fn show_rename_dialog(&self, path: PathBuf) {
        let current_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let entry = gtk::Entry::builder()
            .text(&current_name)
            .placeholder_text("New name")
            .activates_default(true)
            .margin_top(8)
            .margin_bottom(4)
            .margin_start(4)
            .margin_end(4)
            .build();

        let dialog = adw::AlertDialog::new(
            Some("Rename Sound"),
            Some("Enter a new name for this sound."),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("rename", "Rename");
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        dialog.set_extra_child(Some(&entry));

        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[weak]
                entry,
                move |_, response| {
                    if response == "rename" {
                        let new_name = entry.text().trim().to_string();
                        if !new_name.is_empty() && new_name != current_name {
                            win.do_rename_sound(path.clone(), new_name);
                        }
                    }
                }
            ),
        );

        dialog.present(Some(self));
    }

    fn do_rename_sound(&self, old_path: PathBuf, new_name: String) {
        let ext = old_path.extension().map(|e| e.to_os_string());
        let parent = old_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        let mut new_path = parent.join(&new_name);
        if let Some(e) = ext {
            new_path.set_extension(e);
        }

        if let Err(e) = std::fs::rename(&old_path, &new_path) {
            log::error!("Rename failed: {}", e);
            let toast = adw::Toast::new(&format!("Rename failed: {e}"));
            self.imp().toast_overlay.add_toast(toast);
            return;
        }

        // Carry the sound's saved volume/start settings to the new file name.
        self.imp().config.borrow_mut().rename_sound_key(&old_path, &new_path);
        self.imp().config.borrow().save();

        self.imp()
            .soundboard_page
            .rename_sound_by_path(&old_path, new_name.clone(), new_path);

        let toast = adw::Toast::new(&format!("Renamed to \"{new_name}\""));
        self.imp().toast_overlay.add_toast(toast);
    }

    // ── Remove + undo ────────────────────────────────────────────────────────

    fn confirm_remove_sound(&self, path: PathBuf) {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Sound")
            .to_string();

        let dialog = adw::AlertDialog::new(
            Some("Remove Sound"),
            Some(&format!(
                "\"{name}\" will be permanently deleted from your Sounds Folder."
            )),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_, response| {
                    if response == "delete" {
                        win.do_remove_sound(path.clone(), name.clone());
                    }
                }
            ),
        );

        dialog.present(Some(self));
    }

    fn do_remove_sound(&self, path: PathBuf, name: String) {
        if let Some(engine) = self.imp().audio_engine.borrow_mut().as_mut() {
            engine.stop_sound_by_path(&path);
        }

        // Use as_millis() for a unique timestamp (subsec_millis repeats every second)
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let temp_path = std::env::temp_dir().join(format!("resonate_undo_{ts}"));

        // rename(2) fails with EXDEV across filesystems (/tmp is tmpfs, sounds folder is ext4).
        // Fall back to copy + delete so the file always leaves the Sounds Folder.
        let moved = std::fs::rename(&path, &temp_path).is_ok()
            || (std::fs::copy(&path, &temp_path).is_ok()
                && std::fs::remove_file(&path).is_ok());

        if !moved {
            log::error!("Could not move '{}' to temp; deleting directly", path.display());
            let _ = std::fs::remove_file(&path);
        }

        self.imp().soundboard_page.remove_sound_by_path(&path);

        let toast = adw::Toast::builder()
            .title(format!("Removed \"{name}\""))
            .button_label("Undo")
            .timeout(6)
            .build();

        let path_restore = path.clone();
        let temp_restore = temp_path.clone();
        toast.connect_button_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                let restored = std::fs::rename(&temp_restore, &path_restore).is_ok()
                    || (std::fs::copy(&temp_restore, &path_restore).is_ok()
                        && { let _ = std::fs::remove_file(&temp_restore); true });
                if restored {
                    let volume = win.imp().config.borrow().sound_settings(&path_restore).volume;
                    win.imp().soundboard_page.add_sound_from_path(path_restore.clone(), volume);
                }
            }
        ));

        let temp_for_dismiss = temp_path.clone();
        let path_for_dismiss = path.clone();
        toast.connect_dismissed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                if temp_for_dismiss.exists() {
                    // Deletion became permanent — drop the saved settings too.
                    let _ = std::fs::remove_file(&temp_for_dismiss);
                    win.imp().config.borrow_mut().remove_sound_settings(&path_for_dismiss);
                    win.imp().config.borrow().save();
                }
            }
        ));

        self.imp().toast_overlay.add_toast(toast);
    }

    // ── Effects ───────────────────────────────────────────────────────────────

    fn setup_effects(&self) {
        let chain = self.imp().config.borrow().effects_chain.clone();
        let page = self.imp().effects_page.get();
        page.init_chain(&chain);

        // Relay parameter changes to the engine (and save to config).
        page.connect_effect_enabled(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |idx, enabled| {
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    if let Some(e) = cfg.effects_chain.get_mut(idx) {
                        e.enabled = enabled;
                    }
                }
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow().as_ref() {
                    engine.set_effect_enabled(idx, enabled);
                }
                win.sync_active_preset();
            }
        ));

        page.connect_effect_param(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |idx, param, value| {
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    if let Some(e) = cfg.effects_chain.get_mut(idx) {
                        e.params.insert(param.clone(), value);
                    }
                }
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow().as_ref() {
                    engine.set_effect_param(idx, &param, value);
                }
                win.sync_active_preset();
            }
        ));

        // Per-effect presets: set the named params, leave the rest alone.
        page.connect_effect_preset(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |idx, values| {
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    if let Some(e) = cfg.effects_chain.get_mut(idx) {
                        for (param, value) in &values {
                            e.params.insert(param.clone(), *value);
                        }
                    }
                }
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow().as_ref() {
                    for (param, value) in &values {
                        engine.set_effect_param(idx, param, *value);
                    }
                }
                // Redraw the sliders where the preset put them, then re-check
                // whether the chain still matches a saved chain preset.
                win.imp().effects_page.get().refresh_params();
                win.sync_active_preset();
            }
        ));

        // Supplies live parameter metadata so the panel works for LV2 too.
        page.connect_param_provider(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or_default]
            move |idx| {
                win.imp()
                    .audio_engine
                    .borrow()
                    .as_ref()
                    .map(|e| e.effect_params(idx))
                    .unwrap_or_default()
            }
        ));

        // Add a new effect: append to config, rebuild the live chain, refresh rows.
        page.connect_effect_add(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |id| {
                let entry = default_entry_for(&id);
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    cfg.effects_chain.push(entry);
                }
                win.rebuild_and_refresh_effects();
            }
        ));

        // Remove an effect by index.
        page.connect_effect_remove(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |idx| {
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    if idx < cfg.effects_chain.len() {
                        cfg.effects_chain.remove(idx);
                    }
                }
                win.rebuild_and_refresh_effects();
            }
        ));

        // Reorder an effect (up/down arrows).
        page.connect_effect_move(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |idx, up| {
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    let len = cfg.effects_chain.len();
                    let target = if up { idx.checked_sub(1) } else { Some(idx + 1) };
                    match target {
                        Some(t) if idx < len && t < len => cfg.effects_chain.swap(idx, t),
                        _ => return,
                    }
                }
                win.rebuild_and_refresh_effects();
            }
        ));

        // Presets: save/load/delete named chains.
        self.refresh_preset_list();
        page.connect_preset_save(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |name| {
                {
                    let mut cfg = win.imp().config.borrow_mut();
                    let chain = cfg.effects_chain.clone();
                    cfg.effect_presets.insert(name, chain);
                }
                win.imp().config.borrow().save();
                win.refresh_preset_list();
            }
        ));
        page.connect_preset_load(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |name| win.apply_preset(&name)
        ));
        page.connect_preset_delete(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |name| {
                win.imp().config.borrow_mut().effect_presets.remove(&name);
                win.imp().config.borrow().save();
                win.refresh_preset_list();
            }
        ));
    }

    fn refresh_preset_list(&self) {
        let cfg = self.imp().config.borrow();
        let mut user: Vec<String> = cfg.effect_presets.keys().cloned().collect();
        user.sort();
        // Factory chains first, then saved ones; a saved chain of the same name
        // shadows the factory one (lookups check saved first).
        let mut presets: Vec<(String, bool)> = crate::plugins::chains::factory_chains()
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !cfg.effect_presets.contains_key(name))
            .map(|name| (name, true))
            .collect();
        presets.extend(user.into_iter().map(|name| (name, false)));

        let active = cfg
            .active_preset()
            .or_else(|| crate::plugins::chains::matching_factory(&cfg.effects_chain));
        drop(cfg);

        *self.imp().active_preset.borrow_mut() = active.clone();
        self.imp().effects_page.set_presets(&presets, active.as_deref());
        if let Some(tray) = self.imp().tray.borrow().as_ref() {
            let names = presets.into_iter().map(|(name, _)| name).collect();
            crate::tray::set_presets(tray, names, active);
        }
    }

    /// Re-mark the active preset after a chain edit. The match is recomputed
    /// from the chain itself, so it survives a restart and comes back when an
    /// edit is undone; the list is only re-pushed when the answer changed.
    fn sync_active_preset(&self) {
        let cfg = self.imp().config.borrow();
        let active = cfg
            .active_preset()
            .or_else(|| crate::plugins::chains::matching_factory(&cfg.effects_chain));
        drop(cfg);
        if *self.imp().active_preset.borrow() != active {
            self.refresh_preset_list();
        }
    }

    /// Load a named chain preset (from the effects page or the tray menu).
    /// Saved presets win over factory ones of the same name.
    fn apply_preset(&self, name: &str) {
        let saved = self.imp().config.borrow().effect_presets.get(name).cloned();
        let Some(chain) = saved.or_else(|| {
            crate::plugins::chains::factory_chain(name).map(|c| c.to_vec())
        }) else {
            log::warn!("apply_preset: no preset named '{}'", name);
            return;
        };
        self.imp().config.borrow_mut().effects_chain = chain;
        // rebuild_and_refresh_effects re-marks the preset via sync_active_preset.
        self.rebuild_and_refresh_effects();
    }

    /// Persist the chain, rebuild the live engine chain, and refresh the rows.
    fn rebuild_and_refresh_effects(&self) {
        self.sync_active_preset();
        let chain = self.imp().config.borrow().effects_chain.clone();
        self.imp().config.borrow().save();
        if let Some(engine) = self.imp().audio_engine.borrow().as_ref() {
            engine.rebuild_effects(&chain);
        }
        self.imp().effects_page.get().init_chain(&chain);
    }

    // ── Virtual mic default ───────────────────────────────────────────────────

    fn set_virtual_mic_as_default_input(&self) {
        use crate::audio::virtual_device::SOURCE_NAME;
        let nodes = crate::audio::virtual_device::enumerate_nodes();
        let Some(node) = nodes
            .iter()
            .find(|n| n.name == SOURCE_NAME)
            .or_else(|| {
                nodes
                    .iter()
                    .find(|n| n.media_class.contains("Source") && n.description.contains("Resonate"))
            })
        else {
            log::warn!("set_virtual_mic_as_default: Resonate source node not found");
            return;
        };
        let id_str = node.id.to_string();
        match std::process::Command::new("wpctl")
            .args(["set-default", &id_str])
            .status()
        {
            Ok(s) if s.success() => {
                log::info!("Set '{}' (id={}) as default audio input", node.description, id_str);
            }
            Ok(s) => log::warn!("wpctl set-default {} exited with {}", id_str, s),
            Err(e) => log::warn!("wpctl not found or failed: {}", e),
        }
    }

    /// Tear down the background process and exit (from the tray "Quit" item).
    fn do_quit(&self) {
        self.imp().force_quit.set(true);
        if let Some(e) = self.imp().audio_engine.borrow_mut().as_mut() {
            e.virtual_device = None;
        }
        if let Some(app) = self.application() {
            app.quit();
        }
        self.close();
    }

    // ── Settings sync ────────────────────────────────────────────────────────

    fn sync_settings_ui(&self) {
        let config = self.imp().config.borrow().clone();
        let display = config.sounds_folder.to_string_lossy().to_string();
        self.imp().settings_page.set_sounds_folder_label(&display);
        self.imp().settings_page.set_move_files_active(config.move_files_to_folder);

        let settings = self.imp().settings_page.get();
        settings.imp().sounds_folder_row.connect_activated(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| win.choose_sounds_folder()
        ));

        settings.imp().move_files_row.connect_active_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |row| {
                win.imp().config.borrow_mut().move_files_to_folder = row.is_active();
                win.imp().config.borrow().save();
            }
        ));

        // Polyphonic toggle → update engine
        settings.imp().polyphonic_row.set_active(config.polyphonic);
        settings.imp().polyphonic_row.connect_active_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |row| {
                let v = row.is_active();
                win.imp().config.borrow_mut().polyphonic = v;
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.set_polyphonic(v);
                }
            }
        ));

        // Default volume for newly added tiles
        settings.imp().default_volume_row.set_value(config.default_volume as f64);
        settings.imp().default_volume_row.connect_value_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |row| {
                let v = row.value();
                win.imp().config.borrow_mut().default_volume = v as u32;
                win.imp().config.borrow().save();
            }
        ));

        // Monitor enabled toggle
        settings.set_monitor_enabled(config.monitor_enabled);
        settings.imp().monitor_enabled_row.connect_active_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |row| {
                let v = row.is_active();
                win.imp().config.borrow_mut().monitor_enabled = v;
                win.imp().config.borrow().save();
                if let Some(engine) = win.imp().audio_engine.borrow_mut().as_mut() {
                    engine.set_monitor_enabled(v);
                }
            }
        ));

        // Virtual device autostart
        settings.set_autostart_virtual_device(config.virtual_device_enabled);
        settings.imp().autostart_virtual_device_row.connect_active_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |row| {
                win.imp().config.borrow_mut().virtual_device_enabled = row.is_active();
                win.imp().config.borrow().save();
            }
        ));

        // Start on login (background) — writes/removes the XDG autostart entry.
        settings.imp().start_on_login_row.set_active(autostart_enabled());
        settings.imp().start_on_login_row.connect_active_notify(|row| {
            set_autostart(row.is_active());
        });

        // Virtual device name — save on change
        settings.set_virtual_device_name(&config.virtual_device_name);
        settings.imp().virtual_device_name_row.connect_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |row| {
                use gtk::prelude::EditableExt;
                win.imp().config.borrow_mut().virtual_device_name = row.text().to_string();
                win.imp().config.borrow().save();
            }
        ));

        // Enumerate input devices and populate the mic combo. (Monitor playback
        // uses the system default output, so there is no monitor-device picker.)
        let nodes = crate::audio::virtual_device::enumerate_nodes();
        let sources: Vec<String> = nodes
            .iter()
            .filter(|n| n.media_class.contains("Source") && !n.name.starts_with("resonate"))
            .map(|n| n.description.clone())
            .collect();
        settings.set_input_device_list(&sources, &config.input_device_name);

        // Save mic selection when changed
        let sources_for_cb = sources.clone();
        settings.imp().input_device_row.connect_selected_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |row| {
                let idx = row.selected() as usize;
                let name = if idx == 0 {
                    String::new()
                } else {
                    sources_for_cb.get(idx - 1).cloned().unwrap_or_default()
                };
                win.imp().config.borrow_mut().input_device_name = name;
                win.imp().config.borrow().save();
            }
        ));
    }
}

// ── Autostart (XDG ~/.config/autostart) ──────────────────────────────────────

/// Path of the per-user autostart entry.
fn autostart_path() -> PathBuf {
    gtk::glib::user_config_dir()
        .join("autostart")
        .join(format!("{}.desktop", crate::APP_ID))
}

/// Whether "start on login" is currently enabled.
fn autostart_enabled() -> bool {
    autostart_path().exists()
}

/// Write or remove the autostart entry. The entry launches with `--hidden` so
/// the effects run in the background without popping the window on login.
fn set_autostart(enabled: bool) {
    let path = autostart_path();
    if enabled {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "resonate".to_string());
        let contents = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Resonate\n\
             Comment=Soundboard with a virtual microphone and real-time mic effects\n\
             Exec={exe} --hidden\n\
             Icon={id}\n\
             Categories=AudioVideo;Audio;\n\
             X-GNOME-Autostart-enabled=true\n",
            id = crate::APP_ID,
        );
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, contents) {
            log::warn!("Could not write autostart entry: {e}");
        }
    } else if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("Could not remove autostart entry: {e}");
        }
    }
}

/// Default config entry for a newly-added effect. Built-ins get sensible defaults;
/// LV2 entries start with no param overrides (the plugin's own defaults apply).
fn default_entry_for(id: &str) -> crate::config::EffectEntry {
    use crate::config::EffectEntry;
    match id {
        "gain" => EffectEntry::gain(1.0, true),
        "gate" => EffectEntry::gate(0.02, 10.0, 100.0, true),
        // Other built-ins start from their plugin defaults with no overrides.
        other => EffectEntry {
            id: other.to_string(),
            enabled: true,
            params: std::collections::HashMap::new(),
        },
    }
}
