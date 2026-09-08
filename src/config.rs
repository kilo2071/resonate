use gtk::glib;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EffectEntry {
    pub id: String,
    pub enabled: bool,
    pub params: HashMap<String, f32>,
}

impl EffectEntry {
    pub fn gain(gain: f32, enabled: bool) -> Self {
        Self { id: "gain".into(), enabled, params: [("gain".into(), gain)].into() }
    }

    pub fn gate(threshold: f32, attack_ms: f32, release_ms: f32, enabled: bool) -> Self {
        Self {
            id: "gate".into(),
            enabled,
            params: [
                ("threshold".into(), threshold),
                ("attack_ms".into(), attack_ms),
                ("release_ms".into(), release_ms),
            ].into(),
        }
    }
}

/// Structural equality for two chain entries: same effect, same enabled state
/// and the same control values (float-tolerant, so a value that round-tripped
/// through the UI still matches the preset it came from).
fn entries_match(a: &EffectEntry, b: &EffectEntry) -> bool {
    if a.id != b.id || a.enabled != b.enabled || a.params.len() != b.params.len() {
        return false;
    }
    a.params.iter().all(|(k, v)| match b.params.get(k) {
        Some(other) => (v - other).abs() <= 1e-6 * v.abs().max(1.0),
        None => false,
    })
}

/// True if two effect chains are the same effects, in the same order, with the
/// same settings.
pub fn chains_match(a: &[EffectEntry], b: &[EffectEntry]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| entries_match(x, y))
}

/// When a sound's saved start point applies.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StartMode {
    /// Play from the beginning; the marker is kept but inactive.
    #[default]
    Off,
    /// Every play starts at the marker.
    Every,
    /// Only the next play starts at the marker, then the mode reverts to Off.
    Once,
}

/// Persisted per-sound settings, keyed by the sound's file name in `Config::sounds`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SoundSettings {
    /// Linear gain 0.0–1.0 (the tile slider, applied to monitor and virtual mic).
    pub volume: f32,
    /// Start marker in seconds.
    #[serde(default)]
    pub start_secs: f32,
    #[serde(default)]
    pub start_mode: StartMode,
    /// End trim in seconds; 0.0 = play to the end.
    #[serde(default)]
    pub end_secs: f32,
    #[serde(default)]
    pub fade_in_ms: f32,
    #[serde(default)]
    pub fade_out_ms: f32,
}

impl SoundSettings {
    pub fn with_volume(volume: f32) -> Self {
        Self {
            volume,
            start_secs: 0.0,
            start_mode: StartMode::Off,
            end_secs: 0.0,
            fade_in_ms: 0.0,
            fade_out_ms: 0.0,
        }
    }

    /// Effective start offset for the next play, in seconds.
    pub fn effective_start(&self) -> f32 {
        match self.start_mode {
            StartMode::Off => 0.0,
            StartMode::Every | StartMode::Once => self.start_secs.max(0.0),
        }
    }
}

/// `#[serde(default)]` is on the struct, so a field missing from an older (or
/// hand-edited) config.json falls back to `Config::default()` for that field
/// alone instead of failing the whole parse — which used to cost the user their
/// sounds map, tile order and saved presets. Add new fields freely.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub sounds_folder: PathBuf,
    pub move_files_to_folder: bool,
    pub polyphonic: bool,
    pub default_volume: u32,

    // Virtual device
    pub virtual_device_name: String,
    pub virtual_device_enabled: bool,

    // Monitor output (plays on the system default output device)
    pub monitor_enabled: bool,
    pub monitor_volume: f32,

    // Microphone input
    pub input_device_name: String,
    pub mic_volume: f32,

    // Soundboard level into the virtual mic (balances sounds against the mic;
    // the monitor path uses monitor_volume instead)
    pub soundboard_mic_volume: f32,

    // Per-sound settings keyed by file name (volume, start marker, start mode)
    pub sounds: HashMap<String, SoundSettings>,

    // Tile order on the soundboard, as sound file names; unknown files sort after
    pub sound_order: Vec<String>,

    // Named effect-chain presets
    pub effect_presets: HashMap<String, Vec<EffectEntry>>,

    // Effect chain applied to mic input (gate → gain order)
    pub effects_chain: Vec<EffectEntry>,
}

fn default_effects_chain() -> Vec<EffectEntry> {
    vec![
        EffectEntry::gate(0.02, 10.0, 100.0, false),
        EffectEntry::gain(1.0, true),
    ]
}

impl Default for Config {
    fn default() -> Self {
        let docs = glib::user_special_dir(glib::UserDirectory::Documents)
            .unwrap_or_else(|| glib::home_dir().join("Documents"));
        Self {
            sounds_folder: docs.join("Sounds"),
            move_files_to_folder: false,
            polyphonic: true,
            default_volume: 100,
            virtual_device_name: "Resonate Microphone".to_string(),
            virtual_device_enabled: true,
            monitor_enabled: true,
            monitor_volume: 1.0,
            input_device_name: String::new(),
            mic_volume: 1.0,
            soundboard_mic_volume: 1.0,
            sounds: HashMap::new(),
            sound_order: Vec::new(),
            effect_presets: HashMap::new(),
            effects_chain: default_effects_chain(),
        }
    }
}

impl Config {
    /// Name of the saved preset the live chain currently matches, if any.
    /// Derived (never stored), so the tray/popover marker is right after a
    /// restart and comes back on its own when an edit is undone.
    pub fn active_preset(&self) -> Option<String> {
        let mut hits: Vec<&String> = self
            .effect_presets
            .iter()
            .filter(|(_, chain)| chains_match(chain, &self.effects_chain))
            .map(|(name, _)| name)
            .collect();
        hits.sort();
        hits.first().map(|n| (*n).clone())
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    // Unreadable (permissions, a race) rather than invalid —
                    // start from defaults for this run but leave the file be,
                    // so the next run can still pick it up.
                    log::warn!("Cannot read {}: {e}; using defaults", path.display());
                    return Self::default();
                }
            };
            match serde_json::from_str(&text) {
                Ok(cfg) => return cfg,
                Err(e) => {
                    // With `#[serde(default)]` on the struct a missing field is
                    // no longer fatal, so getting here means the file is really
                    // corrupt. Move it aside instead of overwriting it — it
                    // holds the sounds map, tile order and saved presets.
                    let backup = path.with_extension("json.bak");
                    match std::fs::rename(&path, &backup) {
                        Ok(()) => log::warn!(
                            "Config at {} is invalid ({e}); kept a copy at {} and started from defaults",
                            path.display(),
                            backup.display()
                        ),
                        Err(re) => log::warn!(
                            "Config at {} is invalid ({e}) and could not be preserved ({re}); starting from defaults",
                            path.display()
                        ),
                    }
                }
            }
        }
        let cfg = Self::default();
        cfg.save();
        cfg
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, text);
        }
    }

    fn config_path() -> PathBuf {
        glib::user_config_dir()
            .join("io.github.kilo2071.Resonate")
            .join("config.json")
    }

    // ── Per-sound settings ───────────────────────────────────────────────────

    /// Key into `sounds` for a sound file: its file name (folder-independent).
    pub fn sound_key(path: &std::path::Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Settings for a sound, falling back to a default built from `default_volume`.
    pub fn sound_settings(&self, path: &std::path::Path) -> SoundSettings {
        self.sounds
            .get(&Self::sound_key(path))
            .cloned()
            .unwrap_or_else(|| SoundSettings::with_volume(self.default_volume as f32 / 100.0))
    }

    /// Mutable settings entry for a sound, created from defaults on first access.
    pub fn sound_settings_mut(&mut self, path: &std::path::Path) -> &mut SoundSettings {
        let default = SoundSettings::with_volume(self.default_volume as f32 / 100.0);
        self.sounds.entry(Self::sound_key(path)).or_insert(default)
    }

    /// Carry settings over when a sound file is renamed.
    pub fn rename_sound_key(&mut self, old_path: &std::path::Path, new_path: &std::path::Path) {
        if let Some(s) = self.sounds.remove(&Self::sound_key(old_path)) {
            self.sounds.insert(Self::sound_key(new_path), s);
        }
    }

    pub fn remove_sound_settings(&mut self, path: &std::path::Path) {
        self.sounds.remove(&Self::sound_key(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(chain: Vec<EffectEntry>, presets: &[(&str, Vec<EffectEntry>)]) -> Config {
        let mut cfg = Config::default();
        cfg.effects_chain = chain;
        cfg.effect_presets = presets
            .iter()
            .map(|(n, c)| (n.to_string(), c.clone()))
            .collect();
        cfg
    }

    #[test]
    fn a_config_missing_fields_keeps_the_rest() {
        // The pre-`#[serde(default)]` struct failed the whole parse here, and
        // `load()` then wrote defaults over the file.
        let json = r#"{"sounds_folder":"/tmp/s","sound_order":["a.wav"]}"#;
        let cfg: Config = serde_json::from_str(json).expect("partial config parses");
        assert_eq!(cfg.sound_order, vec!["a.wav".to_string()]);
        assert_eq!(cfg.sounds_folder, std::path::PathBuf::from("/tmp/s"));
        // …and the absent fields fall back to Config::default() one by one.
        assert_eq!(cfg.mic_volume, 1.0);
        assert_eq!(cfg.soundboard_mic_volume, 1.0);
        assert!(!cfg.move_files_to_folder);
        assert!(chains_match(&cfg.effects_chain, &default_effects_chain()));
    }

    #[test]
    fn active_preset_matches_the_loaded_chain() {
        let broadcast = vec![EffectEntry::gate(0.02, 10.0, 100.0, true), EffectEntry::gain(1.5, true)];
        let cfg = cfg_with(broadcast.clone(), &[("Broadcast", broadcast)]);
        assert_eq!(cfg.active_preset().as_deref(), Some("Broadcast"));
    }

    #[test]
    fn an_edited_chain_matches_nothing() {
        let broadcast = vec![EffectEntry::gain(1.5, true)];
        let mut cfg = cfg_with(broadcast.clone(), &[("Broadcast", broadcast)]);
        cfg.effects_chain[0].params.insert("gain".into(), 2.0);
        assert_eq!(cfg.active_preset(), None);
        // …and comes back when the edit is undone.
        cfg.effects_chain[0].params.insert("gain".into(), 1.5);
        assert_eq!(cfg.active_preset().as_deref(), Some("Broadcast"));
    }

    #[test]
    fn order_and_enabled_state_are_part_of_the_match() {
        let a = EffectEntry::gain(1.0, true);
        let b = EffectEntry::gate(0.02, 10.0, 100.0, true);
        let cfg = cfg_with(vec![b.clone(), a.clone()], &[("P", vec![a.clone(), b.clone()])]);
        assert_eq!(cfg.active_preset(), None);

        let mut off = a.clone();
        off.enabled = false;
        let cfg = cfg_with(vec![off], &[("P", vec![a])]);
        assert_eq!(cfg.active_preset(), None);
    }
}
