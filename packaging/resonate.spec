# Resonate RPM spec — builds a local, installable package.
#
# Build (from a clean checkout):
#   VERSION=$(awk -F'"' '/^version/{print $2; exit}' Cargo.toml)
#   git archive --format=tar.gz --prefix=resonate-$VERSION/ -o ~/rpmbuild/SOURCES/resonate-$VERSION.tar.gz HEAD
#   rpmbuild -bb packaging/resonate.spec
# The .rpm lands in ~/rpmbuild/RPMS/<arch>/.
#
# NOTE: %build runs `cargo build`, which fetches crates from the network. For a
# fully offline build, run `cargo vendor` first and point CARGO_HOME at it.

Name:           resonate
Version:        0.2.0
Release:        1%{?dist}
Summary:        Soundboard with a virtual microphone and real-time mic effects

License:        GPL-3.0-or-later
URL:            https://github.com/kilo2071/Resonate
Source0:        %{name}-%{version}.tar.gz

ExclusiveArch:  %{rust_arches}

# The release binary is built without debug symbols, so skip the (empty)
# -debuginfo subpackage that rpmbuild would otherwise try to create.
%global debug_package %{nil}

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  pkgconfig(libpipewire-0.3)
BuildRequires:  pkgconfig(dbus-1)
BuildRequires:  lilv-devel
BuildRequires:  lv2-devel
BuildRequires:  serd-devel
BuildRequires:  sord-devel
BuildRequires:  sratom-devel
BuildRequires:  desktop-file-utils

# Runtime: PipeWire + the WirePlumber tooling used to set the default input.
Requires:       pipewire
Requires:       pipewire-pulseaudio
Requires:       wireplumber

# Effects work with the built-in Gain/Gate/Distortion/Bitcrusher/Telephone out of
# the box; these supply the curated LV2 entries. Curated effects whose plugin is
# missing are simply not listed, so none of these are required.
Recommends:     lsp-plugins-lv2
# Character effects: reverb, delays, ring modulator, vocoder, rotary speaker,
# chorus/flanger/phaser, saturator, crusher, tape.
Recommends:     lv2-calf-plugins
# Pitch shifting (the low-latency "live" variant) and auto-tune.
Recommends:     lv2-rubberband-plugins
Recommends:     lv2-x42-plugins

%global appid io.github.kilo2071.Resonate

%description
Resonate is a native GNOME soundboard built with GTK 4 and Libadwaita. It
exposes a PipeWire virtual microphone that mixes soundboard playback with your
real microphone, and applies a real-time effects chain to the mic input using
built-in plugins (Noise Gate, Gain, Distortion, Bitcrusher, Telephone) plus a
curated set of LV2 plugins, grouped into voice cleanup (RNNoise suppression,
auto gain, compressor, de-esser, limiter, parametric EQ) and character effects
(pitch shifter, auto-tune, ring modulator, vocoder, reverb, delays, rotary
speaker, chorus, flanger, phaser, saturator, crusher, tape). Each effect offers
ready-made presets, ten chain presets ship with the app, and whole chains can be
saved and switched from the tray. Sounds have per-file volume, start markers,
trimming and fades, and can be triggered by global numpad hotkeys. It
can run in the background (with a tray indicator) and start on login, acting as
a voice-effects processor for calls, streaming and recording.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --locked

%install
install -Dm0755 target/release/resonate %{buildroot}%{_bindir}/resonate
install -Dm0644 data/%{appid}.desktop \
    %{buildroot}%{_datadir}/applications/%{appid}.desktop
install -Dm0644 data/icons/%{appid}.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/%{appid}.svg

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/%{appid}.desktop

%files
%license LICENSE
%doc README.md
%{_bindir}/resonate
%{_datadir}/applications/%{appid}.desktop
%{_datadir}/icons/hicolor/scalable/apps/%{appid}.svg

%changelog
* Tue Sep 08 2026 kilo2071 <74557968+kilo2071@users.noreply.github.com> - 0.2.0-1
- Effects are rebuilt around an LV2 host: installed LV2 plugins run through the
  same interface as the built-ins, from a curated catalogue grouped into
  "Voice & Cleanup" and "Character & Fun" (RNNoise suppression, compressor,
  de-esser, limiter, parametric EQ; pitch shifting, auto-tune, ring modulator,
  vocoder, reverb, delays, rotary speaker, chorus, flanger, phaser, saturator,
  crusher, tape). Effects whose plugin is missing are simply not listed.
- Every effect has a preset dropdown, and ten chain presets ship with the app
  (Podcast, Broadcast, Noisy Room, Old Radio, Robot, Vocoder Robot, Demon,
  Chipmunk, Stadium Announcer, Cathedral); chains can be saved, reordered and
  switched from the tray, which marks the active one
- Big effects show their important knobs first, with the rest under an
  "All parameters" expander, and the same effect can be added more than once
- New built-in effects: Distortion, Bitcrusher, Telephone
- Per-sound settings (volume, start marker, trimming, fades) with a waveform
  editor, LCD scrubbing and a live oscilloscope, search, tile reorder and
  import normalisation
- Global numpad hotkeys to trigger tiles via the GlobalShortcuts portal
- New sounds are left where they are by default instead of being moved into the
  Sounds Folder (the setting is unchanged for existing installs)
- "Play Multiple Sounds" now shows its saved state after a restart, rather than
  always appearing on
- A settings file from an older version no longer resets every other setting,
  and one that cannot be parsed is kept aside instead of being overwritten

* Tue Sep 01 2026 kilo2071 <74557968+kilo2071@users.noreply.github.com> - 0.1.0-7
- Chain presets now ship with the app: Podcast, Broadcast, Noisy Room, Old
  Radio, Robot, Vocoder Robot, Demon, Chipmunk, Stadium Announcer, Cathedral
  (only those whose plugins are installed are offered)
- The vocoder works as a voice effect at last: its presets raise the per-band
  noise generator into a carrier, giving the whispered-robot sound
- Dropped the graphic equaliser (the parametric one is a superset), exciter and
  bass enhancer
- Fixed a crash from using the plugin database from two threads at once

* Tue Sep 01 2026 kilo2071 <74557968+kilo2071@users.noreply.github.com> - 0.1.0-6
- Big effects show their important knobs first, with the rest under an
  "All parameters" expander (the parametric EQ has 181 controls)
- The same effect can be added to the chain more than once; repeats are numbered
- Plugin-supplied presets are read once per plugin instead of on every click

* Tue Sep 01 2026 kilo2071 <74557968+kilo2071@users.noreply.github.com> - 0.1.0-5
- Curated a lot more effects: Calf character plugins (ring modulator, vocoder,
  reverb, delays, rotary speaker, chorus, flanger, phaser, pulsator, saturator,
  crusher, tape), Rubber Band pitch shifting and x42 auto-tune, plus
  de-esser, exciter and bass enhancer for voices
- The Add Effect sheet is grouped into "Voice & Cleanup" and "Character & Fun"
- Per-effect presets: a Preset dropdown above each effect's controls, from a
  curated table plus any presets the plugin itself ships

* Tue Sep 01 2026 kilo2071 <74557968+kilo2071@users.noreply.github.com> - 0.1.0-4
- Tray menu shows which effect preset is active: the presets are a radio group
  (with a "Custom" slot for an edited chain) and the name is repeated in the
  submenu label and tooltip
- The active preset is derived from the chain, so it survives a restart; the
  in-app presets popover marks it too

* Mon Aug 31 2026 kilo2071 <74557968+kilo2071@users.noreply.github.com> - 0.1.0-3
- Effect parameters can be typed exactly (spin button beside each slider)
- Effect presets can be switched from the tray menu

* Mon Aug 31 2026 kilo2071 <74557968+kilo2071@users.noreply.github.com> - 0.1.0-2
- Per-sound settings (volume/start/trim/fades), sound editor, LCD scrubbing +
  oscilloscope, search, tile reorder, import normalization
- New built-in effects (Distortion, Bitcrusher, Telephone), curated LV2 picker,
  chain presets and reorder, mic level meter
- Global numpad hotkeys via the GlobalShortcuts portal (dbus crate; the dbus-1
  BuildRequires was already in place); fixed the --hidden autostart entry

* Sun Jun 07 2026 kilo2071 <74557968+kilo2071@users.noreply.github.com> - 0.1.0-1
- Initial package: soundboard, PipeWire virtual mic, LV2 mic effects,
  background mode with tray indicator and start-on-login.
