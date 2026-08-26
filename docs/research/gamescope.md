# Gamescope CLI surface and launcher usage (args for the `graphics` seam) — Research note

**Goal:** Enumerate the authoritative gamescope CLI argument surface from primary sources, document how real launchers (Steam, Heroic, Lutris, Bottles) invoke it, and decide whether Cellar's planned `graphics` config shape (`"gamescope"` bare legacy string vs `{ kind = "gamescope", args = [...] }`) fits gamescope's real config model. Research conducted 2026-08-25 against primary sources: `ValveSoftware/gamescope` master (commit 2026-08-25, latest release tag `3.16.25`; source downloaded and read locally), `HeroicGamesLauncher` main (head commit 2026-08-10), `lutris/lutris` master (2026-08-22), `bottlesdevs/Bottles` main (2026-08-25). Everything below is cited to a repo file or URL; anything from a secondary write-up is flagged as such.

## 1. What gamescope is, and why a Wine/Proton launcher wraps it

Gamescope ("the micro-compositor formerly known as steamcompmgr", Valve, BSD-2-Clause [LICENSE](https://github.com/ValveSoftware/gamescope/blob/master/LICENSE)) has two operating modes [README.md](https://github.com/ValveSoftware/gamescope/blob/master/README.md): **embedded/session mode** — it becomes the *whole* display session via DRM/KMS, gets game frames through Wayland/Xwayland, and can direct-flip frames to the screen (this is the Steam Deck's gaming-mode compositor); and **nested mode** — a window on top of an existing desktop (SDL/Wayland backend), where the game runs inside its own Xwayland "sandbox desktop" behind a **spoofable virtual screen**: "You can spoof a virtual screen with a desired resolution and refresh rate as the only thing the game sees, and control/resize the output as needed" [README.md](https://github.com/ValveSoftware/gamescope/blob/master/README.md). Launchers use it per-game for exactly that second property: force the *game* to render at a low resolution while the *output* is a different one (FSR/NIS upscaling, integer scaling, ultrawide pillarboxing, downsampling), cap FPS independently of the game's vsync, grab the mouse, enable HDR/VRR regardless of the desktop session, and get a MangoHud overlay (`--mangoapp`) without injecting anything into the game. It runs on top of X11 or Wayland desktops; requires Mesa 20.3+ (AMD) / 21.2+ (Intel); NVIDIA 515.43.04+ with `nvidia-drm.modeset=1` [README.md](https://github.com/ValveSoftware/gamescope/blob/master/README.md).

## 2. The full CLI argument list (authoritative, from source)

The option table is a single `getopt_long` table in [`src/main.cpp`](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) (lines 60–165), consumed by **two** parse loops — one in `main()` ([main.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) lines 739–872, core options) and one in `steamcompmgr_main()` ([src/steamcompmgr.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/steamcompmgr.cpp) lines 8682–8754, compositor/steamcompmgr options). The self-describing text is the `usage` string in main.cpp (lines 167–292), printed by `gamescope --help`. Both parse loops read the **same** argv; unknown options hit `case '?'` in the first loop (`main()`) → "See --help for a list of options." and exit 1 (the second loop's `?` case is an unreachable assert; it only ever sees flags the first loop accepted).

Everything after the first `--` becomes the **primary child command** that gamescope supervises (`subCommandArg` in [steamcompmgr.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/steamcompmgr.cpp) lines 8756–8760); when it exits, gamescope shuts down unless `--keep-alive`. Two getopt notes: short flags can be combined (`-bf`); `gamescope <flags> -- <game>` is the invocation shape every launcher uses (see §4).

Grouping by purpose (short form = `-X`, long = `--xxx`; meanings are one-liners from the option table, `usage` string, parse-loop handlers, or README):

### Output / resolution / virtual screen

| Flag | Meaning | Source (master) |
|---|---|---|
| `-W, --output-width N` | Resolution gamescope composites at ("output"). Resizing the window updates it; **ignored in embedded mode**; if only `-H` given, 16:9 assumed; default 1280×720 | table L69; usage L172; README |
| `-H, --output-height N` | Output height, as above | table L70 |
| `-w, --nested-width N` | Resolution the **game** sees; defaults to output values; 16:9 assumed if only `-h` given | table L63; usage L174; README |
| `-h, --nested-height N` | Game-facing height | table L64 |
| `-O, --prefer-output LIST` | Connector preference for embedded mode, e.g. `DP-1,DP-2,HDMI-A-1` | table L93; usage L233 |
| `--display-index N` | Force a specific display in nested mode | table L88; usage L230 |
| `--generate-drm-mode cvt\|fixed` | DRM mode generation algorithm (embedded) | table L95; usage L235 |
| `--force-orientation left\|right\|normal\|upsidedown` | Rotate the internal display (Deck/handheld portrait panels) | table L141; usage L207 |
| `--force-composition-rotation` | Rotate in compositor instead of at scanout | table L140; usage L206 |
| `--virtual-connector-strategy STRAT` | Virtual connector creation strategy (multi-display/Steam) | table L127; usage L210 |
| `--force-windows-fullscreen` | Force windows to fill the nested display (like Deck fullscreen) | table L142; usage L208 |
| `--cursor-scale-height N` | Base output height to linearly scale the cursor against | table L126; usage L209 |

### Scaling / upscaling (FSR, NIS, integer, nearest, stretch)

| Flag | Meaning | Source |
|---|---|---|
| `-S, --scaler auto\|integer\|fit\|fill\|stretch` | *Scaling mode*: how the game image is fit to the output — integer (crisp pixel games), stretch (4:3→16:9 fill), fit/fill, auto | table L67; usage L178; enum in [src/main.hpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.hpp) `GamescopeUpscaleScaler` |
| `-F, --filter linear\|nearest\|fsr\|nis\|pixel` | *Scaling algorithm*: `fsr` = AMD FidelityFX™ Super Resolution 1.0, `nis` = NVIDIA Image Scaling v1.0.3, `nearest` for pixel look, `pixel` | table L68; usage L179–181 |
| `--sharpness N` (alias `--fsr-sharpness`) | Upscaler sharpness, 0 (max) to 20 (min); default 2 | table L71–72; usage L182; main.cpp L319 |
| `-m, --max-scale F` | Maximum window scale factor (limits how large the nested window can grow) | table L66; usage L177 |
| *(legacy, pre-3.12)* `-U` = `--fsr-upscaling`, `-Y` = NIS, `-i` = `--integer-scale`, `-n` = `--nearest-neighbor-filter` | The short-flag era: 3.11.2's table had these; 3.12 replaced them with `-F`/`-S` | [main.cpp @ 3.11.2](https://github.com/ValveSoftware/gamescope/blob/3.11.2/src/main.cpp) L28–41; Heroic/Lutris still detect this (see §4) |

### Framerate cap / vsync / VRR

| Flag | Meaning | Source |
|---|---|---|
| `-r, --nested-refresh N` | Nested display refresh rate in Hz — doubles as the game's FPS cap (`ConvertHztomHz`), default unlimited | table L65; usage L176; README |
| `-o, --nested-unfocused-refresh N` | FPS cap while the gamescope window is unfocused; nested mode only | table L83; usage L225 |
| `--framerate-limit N` | *Different* limiter: divisor of the refresh rate, rounds down (60/59→60, 60/25→30); default 0 = disabled | table L97; usage L220; steamcompmgr.cpp L8741–8742 |
| `--immediate-flips` | Enable immediate flips / tearing (sets `cv_tearing_enabled`) | table L96; usage L236; main.cpp L835–836 |
| `--adaptive-sync` | Enable adaptive sync / variable refresh rate if available (sets `cv_adaptive_sync`) | table L78; usage L222; main.cpp L841–842 |

### HDR / color management

| Flag | Meaning | Source |
|---|---|---|
| `--hdr-enabled` | Enable HDR output; "needs Gamescope WSI layer enabled for support from clients"; unset → HDR clients are tonemapped to SDR | table L146; usage L211–212; steamcompmgr.cpp L8725–8726 (`--hdr-enable` accepted alias) |
| `--hdr-sdr-content-nits N` | Luminance of SDR content presented in HDR, nits; default 400 | table L147; usage L214 |
| `--sdr-gamut-wideness 0–1` | Gamut "wideness" for SDR content | table L145; usage L213 |
| `--hdr-itm-enabled` | SDR→HDR inverse tone mapping (only for SDR input) | table L148; usage L215 |
| `--hdr-itm-sdr-nits N` | Input luminance for ITM; default 100, max 1000 | table L149; usage L216–217 |
| `--hdr-itm-target-nits N` | Target luminance for ITM; default 1000, max 10000 | table L150; usage L218–219 |
| `--hdr-debug-force-support`, `--hdr-debug-force-output`, `--hdr-debug-heatmap` | Debug: force HDR support / force HDR10 PQ output / luminance heatmap | table L151–153; usage L268–270 |
| `--disable-color-management` | Turn color management off | table L144; usage L266 |

### Input / mouse / touch

| Flag | Meaning | Source |
|---|---|---|
| `-s, --mouse-sensitivity F` | Multiply mouse movement by a decimal factor | table L76; usage L184 |
| `-g, --grab` | Grab the keyboard (nested) | table L86; usage L228 |
| `--force-grab-cursor` | Always use relative-mouse mode instead of switching with cursor visibility (FPS-camera capture) | table L87; usage L229 |
| `--default-touch-mode 0–4` | Embedded touch click mode (0 hover, 1 left, 2 right, 3 middle, 4 passthrough) | table L94; usage L234 |
| `--xwayland-force-touch-pointer-emulation` | Emit touch pointer emulation (default off: `g_bNoTouchPointerEmulation = true`) | table L121; main.cpp L809–810, L326 |

### Window forms (nested mode)

| Flag | Meaning | Source |
|---|---|---|
| `-f, --fullscreen` | Fullscreen gamescope window | table L85; usage L227 |
| `-b, --borderless` | Borderless gamescope window | table L84; usage L226 |
| *(read-only)* resizing the window live-updates `-W`/`-H` | README: "Resizing the gamescope window will update these settings" | README |

### Ratio

There is **no ratio flag** — aspect ratio is implicit: if only one dimension of `-w/-h` (or `-W/-H`) is given, 16:9 is assumed [README.md](https://github.com/ValveSoftware/gamescope/blob/master/README.md); `-S fit/stretch` then controls how the game's own aspect maps into the window.

### Streaming / capture

No CLI flag controls streaming. PipeWire screen capture is initialized automatically (compositor + nested children) [main.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) L1099–1104; Steam Remote Play / external streaming pick the gamescope window up through normal desktop capture. No flag needed.

### Backend / session / Steam integration

| Flag | Meaning | Source |
|---|---|---|
| `--backend auto\|drm\|sdl\|openvr\|headless\|wayland` | Rendering backend: `drm` = standalone display session (embedded), `sdl`/`wayland` = windowed (nested), `headless` = no output, `openvr` = VR overlay (compile-gated) | table L80; usage L185–197 |
| `--expose-wayland` | Support Wayland clients via xdg-shell inside the sandbox (needed for games running in Wayland mode / HDR clients on a Wayland desktop) | table L75; usage L183; also sets `XDG_SESSION_TYPE=wayland` for children (main.cpp L1076–1079) |
| `-e, --steam` | Steam integration: enables `STEAM_GAMESCOPE_*` capability env vars, switches virtual-connector strategy to Steam-controlled | table L135; usage L203; main.cpp L785–789 |
| `--xwayland-count N` | Create N Xwayland servers (multi-app / multi-display), default 1, min 1 | table L120; usage L204 |
| `--rt` | Realtime scheduling (needs `CAP_SYS_NICE`; requires restart as root or capabilities) | table L73; usage L200; main.cpp L885–891 |
| `--prefer-vk-device VENDOR:DEVICE` | Prefer a Vulkan device for compositing, e.g. `1002:7300` (multi-GPU pick) | table L74; usage L205 |
| `--allow-deferred-backend` | If the backend fails to init, retry in a deferred way | table L161; usage L280 |
| `--keep-alive` | Keep gamescope alive when the primary child dies (sets `cv_shutdown_on_primary_child_death = false`) | table L162; usage L281; main.cpp L853–854 |
| `--version` | Print version (not in usage text but handled) | table L62; main.cpp L796–797 |
| `--help` | Print version + full usage | table L61; usage L171 |

### Perf overlay / diagnostics

| Flag | Meaning | Source |
|---|---|---|
| `--mangoapp` | Launch mangoapp (MangoHud perf overlay) as a gamescope child; help text: "You should use this instead of using mangohud on the game **or** gamescope" | table L77; usage L221; steamcompmgr.cpp L8658–8663 |
| `-T, --stats-path PATH` | Write compositor statistics to path (thread-backed) | table L129; steamcompmgr.cpp L8689–8695 |
| `-R, --ready-fd FD` | Notify an FD when the compositor is ready (writes nested display names; used by Steam) | table L128; usage L199; steamcompmgr.cpp L8686–8688, L8808–8813 |
| `-C, --hide-cursor-delay N` | Hide cursor after N seconds | table L130; usage L202 |

### Cursor / appearance / effects (nice-to-haves)

`--cursor PATH` (custom cursor image), `--cursor-hotspot X,Y`, `--fade-out-duration N` (ms), `--reshade-effect NAME` (shader from `/usr/share/gamescope/reshade/Shaders` or `~/.local/share/gamescope/reshade/Shaders`) + `--reshade-technique-idx N`, `--mura-map PATH` (Steam Deck OLED mura compensation) — [option table](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) L124–125, L139, L155–156, L159; usage L198, L272–277.

### Debug / development

`--debug-layers`, `--debug-focus`, `--synchronous-x11`, `-v --debug-hud`, `--debug-events`, `-c --force-composition` (disable direct scan-out), `--composite-debug`, `-x --disable-xres`, `--disable-color-management` — usage L257–270; both parse loops. Note: **`--disable-layers` is in the table and usage text but has no handler** in either parse loop (I grepped the whole tree for its target ConVars; none exist) — vestigial in current master.

### VR (OpenVR overlay backend, compile-gated `HAVE_OPENVR`)

`--vr-overlay-key`, `--vr-app-overlay-key`, `--vr-overlay-explicit-name`, `--vr-overlay-default-name`, `--vr-overlay-icon`, `--vr-overlay-show-immediately`, `--vr-overlay-enable-control-bar` (+`-keyboard`, `-close`), `--vr-overlay-enable-click-stabilization`, `--vr-overlay-modal`, `--vr-overlay-physical-width`, `--vr-overlay-physical-curvature`, `--vr-overlay-physical-pre-curve-pitch`, `--vr-scroll-speed`, `--vr-session-manager` — table L99–117; usage L238–255. Not relevant to a Wine/Proton launcher; listed for completeness.

### Keyboard shortcuts (runtime, no flags)

`Super+F` fullscreen toggle, `Super+N` nearest-neighbour toggle, `Super+U` FSR toggle, `Super+Y` NIS toggle, `Super+I`/`Super+O` FSR sharpness ±1, `Super+S` screenshot (`/tmp/gamescope_$DATE.png`), `Super+G` keyboard-grab toggle [README.md](https://github.com/ValveSoftware/gamescope/blob/master/README.md).

## 3. Environment variables

**Read by gamescope itself** (getenv, verified in source): `DISPLAY`/`WAYLAND_DISPLAY` (parent session), `XKB_DEFAULT_LAYOUT/MODEL/OPTIONS/RULES/VARIANT`, `GAMESCOPE_SCRIPT_PATH` (colon-separated extra Lua-script dirs) [src/Script/Script.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/Script/Script.cpp) L113–126, `GAMESCOPE_NV12_COLORSPACE` (colorspace string) [main.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) L979, `GAMESCOPE_MODE_SAVE_FILE` (persist generated DRM modes) [src/Backends/DRMBackend.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/Backends/DRMBackend.cpp) L1037, `GAMESCOPE_DISABLE_ASYNC_FLIPS` / `GAMESCOPE_LIFTOFF_CACHE_DISABLE` (debug off-switches) [DRMBackend.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/Backends/DRMBackend.cpp) L1328/L1984, `ENABLE_VKBASALT` (forces compositing) [steamcompmgr.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/steamcompmgr.cpp) L8762–8766.

**Generic ConVar override — the big one:** any env var named `gamescope_<ConVarName>=<value>` overrides a ConVar at startup (e.g. `gamescope_hdr_enabled=1`, `gamescope_adaptive_sync=1`), applied before backend init [main.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) L915–938. This is an officially supported config channel parallel to CLI flags — enumeration of ConVars is runtime (`gamescopectl`/scripts), not a stable list, and the mechanism is explicitly "experimental and subject to change massively" for the script side [scripts/README.md](https://github.com/ValveSoftware/gamescope/blob/master/scripts/README.md).

**Set by gamescope for its children** (verified): `DISPLAY` (nested Xwayland display), `XDG_SESSION_TYPE` (`wayland` only with `--expose-wayland`, else `x11`), `XDG_CURRENT_DESKTOP=gamescope`, `STEAM_GAME_DISPLAY_0..N` (Xwayland displays), `GAMESCOPE_WAYLAND_DISPLAY`, `WAYLAND_DISPLAY` (only with `--expose-wayland`) [main.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) L1075–1097; nested mode additionally sets `ENABLE_GAMESCOPE_WSI=1` and strips the SDK's `SDL_VIDEODRIVER` [steamcompmgr.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/steamcompmgr.cpp) L8625–8633; the fps limiter state is shared via a temp file path in `GAMESCOPE_LIMITER_FILE` [main.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) L700–706. With `-e/--steam` it announces capability env vars to Steam (`STEAM_GAMESCOPE_VRR_SUPPORTED`, `STEAM_GAMESCOPE_HDR_SUPPORTED`, `STEAM_GAMESCOPE_DYNAMIC_FPSLIMITER`, `STEAM_GAMESCOPE_FANCY_SCALING_SUPPORT`, etc.) [main.cpp `UpdateCompatEnvVars`](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) L614–644. It also deliberately `unsetenv("WAYLAND_DISPLAY")` before launching so clients can't bypass the sandbox and talk to the parent compositor [main.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) L1033–1034.

**For the HDR path, launchers set additional env on the child** (not read by gamescope proper, read by the gamescope WSI Vulkan layer / DXVK): Bottles sets `ENABLE_GAMESCOPE_WSI=1` (+ removes `DISABLE_GAMESCOPE_WSI`) and `DXVK_HDR=1` when gamescope + HDR are active [Bottles `winecommand.py`](https://github.com/bottlesdevs/Bottles/blob/main/bottles/backend/wine/winecommand.py) L165–183; the WSI layer ships in the same repo under `layer/` (VkLayer_FROG_gamescope_wsi) and is what makes Windows Vulkan games see HDR/VRR-capable surfaces.

## 4. How real launchers call it

**Steam launch options.** The gamescope README itself documents the Steam pattern (primary): in the game's launch options put `gamescope <flags> -- %command%`, with the worked examples `gamescope -h 720 -H 1440 -S integer -- %command%` (integer upscale), `gamescope -r 30 -- %command%` (FPS cap), `gamescope -w 1920 -h 1080 -W 3440 -H 1440 -b -- %command%` (pillarboxed ultrawide) [README.md — Examples](https://github.com/ValveSoftware/gamescope/blob/master/README.md). On SteamOS/Steam Deck, gamescope is the *session* compositor (embedded DRM mode) and Steam itself is one of its child apps; the `-e/--steam` flag exists for the Steam integration protocol. Valve's help article ["How do I use gamescope?"](https://help.steampowered.com/en/faqs/view/6F0E-5A31-4AC3-9A24) exists but is a JS-rendered React app — content not directly fetchable from this environment, so I did **not** quote it; the README examples above are the verifiable primary anchor for the Steam case. The ArchWiki [Gamescope](https://wiki.archlinux.org/title/Gamescope) page (secondary) documents the same pattern, e.g. `gamescope -w 1920 -h 1080 -W 3840 -H 2160 -F nis -- <game>`.

**Heroic Games Launcher** (primary, [src/backend/launcher.ts](https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/blob/main/src/backend/launcher.ts) L584–703 + settings UI [Gamescope.tsx](https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/blob/main/src/frontend/screens/Settings/components/Gamescope.tsx)). Settings: `enableUpscaling` (`-w/-h` game res, `-W/-H` upscale res, `upscaleMethod` fsr/nis/integer/stretch, `windowType` fullscreen/borderless), `enableLimiter` (`-r`, `-o`), `enableForceGrabCursor` (`--force-grab-cursor`), `additionalOptions` (free-form shlex-split, appended verbatim), plus `--mangoapp` when MangoHud is on. The command is built as `gamescope <flags…> <additionalOptions> -- <game>`. Heroic also versions-*detects* gamescope by grepping `gamescope --help` for `-F, --filter`: pre-3.12 it emits legacy `-U`/`-Y`/`-i`, post-3.12 `-F fsr`/`-F nis`/`-S integer` (L617–655). Gamescope is skipped entirely when running inside a gamescope session (checks `XDG_CURRENT_DESKTOP === 'gamescope'`).

**Lutris** (primary, [lutris/runner_interpreter.py](https://github.com/lutris/lutris/blob/master/lutris/runner_interpreter.py) L81–149 + [lutris/sysoptions.py](https://github.com/lutris/lutris/blob/master/lutris/sysoptions.py) L255–360). System-options section _Gamescope_: `gamescope` (bool, gated on `gamescope` being on PATH *and* NVIDIA ≥515), `gamescope_hdr` (→ `--hdr-enabled` + `DXVK_HDR=1`), `gamescope_force_grab_cursor`, `gamescope_output_res` (`-W -H`, resolution dropdown incl. custom `WxH`), `gamescope_game_res` (`-w -h`), `gamescope_window_mode` (Fullscreen `-f`/Windowed/Borderless `-b`), `gamescope_fsr_sharpness` (`--fsr-sharpness`), `gamescope_fps_limiter` (`-r`), `gamescope_flags` (raw free-form string, shlex-split and inserted verbatim). Argument assembly builds `gamescope [--mangoapp] [--hdr-enabled] [--prefer-vk-device <pci>] [-w W -h H] [-W W -H H] [-r N] [window-mode] [flags…] [--fsr-sharpness N -F fsr] [--force-grab-cursor] -- <command>` (insertion order reversed in code). Same `--help`-grep version detection for the FSR flag as Heroic (`_get_gamescope_fsr_option`, L140–147).

**Bottles** (primary, [bottles/frontend/windows/gamescope.py](https://github.com/bottlesdevs/Bottles/blob/main/bottles/frontend/windows/gamescope.py) + [bottles/backend/wine/winecommand.py](https://github.com/bottlesdevs/Bottles/blob/main/bottles/backend/wine/winecommand.py) L916–975). Per-bottle parameters: `gamescope_game_width/height` (`-w/-h`), `gamescope_window_width/height` (`-W/-H`), `fsr` + `fsr_sharpening_strength` (`-F fsr --fsr-sharpness N`), `gamescope_fps`/`gamescope_fps_no_focus` (`-r`/`-o`), `gamescope_scaling` (`-S integer`), `gamescope_borderless`/`gamescope_fullscreen` (`-b`/`-f`), `gamescope_custom_options` (raw string). HDR is auto-injected (`--hdr-enabled`) when the bottle's HDR toggle is on and not already in custom options. Bottles writes the wine command to a temp script and runs `gamescope <cmd-built-args> -- <script>` (L868–889). Notably **all three launchers model the same thing**: a handful of typed settings mapped to current flags, plus a free-form raw-args escape hatch.

All three also follow the same habits: `--` is always the last gamescope-side token; the wrapped child is the whole remainder (wine/umu/game launcher chain can be the child); and they *probe* `gamescope --help` at runtime rather than hard-coding the flag surface — because gamescope's flags changed across 3.12.

## 5. The high-value argument set a game launcher should expose

For Cellar (per-prefix, wrapping `umu-run`/wine as the child after `--`), the arguments that matter, with justification:

| Cellar setting | Flags it renders | Why |
|---|---|---|
| Game resolution | `-w N` `-h N` | Spoof the resolution the game sees (render lower, or force aspect/16:9) — the core Deck-style trick |
| Output resolution | `-W N` `-H N` | Virtual-screen size (ultrawide, 4K downsampling, 720p → 1440p integer) |
| Upscale filter | `-F fsr\|nis\|nearest` + `--fsr-sharpness N` | Bigger perf headroom / pixel-perfect look; sharpness 0–20, default 2 |
| Scaling mode | `-S integer\|stretch\|fit` | Integer scaling for retro/2D; stretch for 4:3 classics |
| FPS cap | `-r N` (and `-o N` unfocused) | Frame capping independent of game vsync — highest-value launcher toggle after resolution |
| Window mode | `-f` / `-b` | The two nested-window shapes every launcher exposes |
| Wayland exposure | `--expose-wayland` | Needed if the game runs on the Wayland path (and for HDR clients) |
| HDR | `--hdr-enabled` (+ env `DXVK_HDR=1`, `ENABLE_GAMESCOPE_WSI=1`) | HDR output through the gamescope WSI layer; requires HDR display + layer |
| VRR | `--adaptive-sync` | Variable refresh support on capable monitors |
| Overlay | `--mangoapp` | MangoHud overlay without touching the game process (complements Cellar's mangohud wrapper) |
| Mouse capture | `--force-grab-cursor` | Fixes FPS-camera/aim issues (Lutris and Heroic both expose it) |
| Backend | `--backend wayland\|sdl` (advanced) | The two practical nested backends; `auto` is the safe default |
| GPU pick | `--prefer-vk-device 1002:7300` (advanced) | Multi-GPU compositing choice (Lutris parity) |
| Escape hatch | any extra flags, verbatim before `--` | The universal pattern (Heroic `additionalOptions`, Lutris `gamescope_flags`, Bottles `gamescope_custom_options`); users paste community flags |

Not high-value for Cellar: `-e/--steam` (Steam protocol only), `-O/--display-index/--generate-drm-mode/--immediate-flips/--default-touch-mode` (embedded-session flags), `--rt` (capabilities), VR block, `--mura-map` (Deck hardware), reshade/cursor/debug flags.

## 6. Config-shape implications for the planned `{ kind, args }` seam

**Does gamescope read a config FILE?** Not for CLI options — but yes, it now has an *experimental* Lua config/script system: `.lua` files are executed recursively in alphabetical order from the install dir, `/etc/gamescope/scripts`, and `$XDG_CONFIG_DIR/gamescope/scripts` (i.e. `~/.config/gamescope/scripts`), overridable/appendable via `GAMESCOPE_SCRIPT_PATH`, and the default scripts ship in the repo under `scripts/` (display calibration per handheld, dev utils) [src/Script/Script.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/Script/Script.cpp) L111–138; [scripts/README.md](https://github.com/ValveSoftware/gamescope/blob/master/scripts/README.md) ("currently experimental and subject to change massively"). Plus the ConVar env-override channel (`gamescope_<name>=<value>`, §3). So: a user's *global* gamescope config can exist independently of Cellar's per-prefix args — Cellar's seam only needs to produce a valid argv slice; it does not need to model the Lua system. **This is the strongest argument for the planned shape over, say, reimplementing gamescope settings as Cellar-owned config files.**

**Flag conflicts / mutual exclusions inside one argv:**
- `--` is a hard separator: gamescope args **must** come before it, the wrapped command after. A flat `args: Vec<String>` + Cellar appending `--` then the child argv is the exact contract; a config shape that cannot express "the args list is order-sensitive before `--`" would be wrong (the planned list does express it).
- Mode gating: `-W/-H` are ignored in embedded mode; `-f/-b/-g/-o/--display-index` are nested-only; `-O/--immediate-flips/--generate-drm-mode` embedded-only [README + usage](https://github.com/ValveSoftware/gamescope/blob/master/README.md). A launcher that only ever wraps per-game on a desktop will always run nested, but a future "embeddable session" mode would need different validation — worth a comment in the seam, not a schema change.
- `-r` and `--framerate-limit` are **two different limiter mechanisms** (refresh-cycle cap vs divisor cap) — a config UI exposing "fps limit" should pick one (launchers use `-r`).
- `-f` and `-b` both only set booleans; they're not validated against each other (Bottles' UI keeps them mutually exclusive in the UI layer, not enforced by gamescope).
- One child: everything after `--` is a single primary command string; there is no multi-app CLI syntax (multi-app needs scripts/`gamescope.create_xwayland_server` semantics). Cellar's child = its normal launch chain, so this is fine.

**Resolution: dedicated keys vs flat args?** Every real launcher surveyed models `width/height` per-side (`-w/-h` vs `-W/-H`) as discrete numeric fields plus a raw-args string, never a fully flat list. That's ecosystem evidence that a launcher UI wants typed resolution fields. But the *storage* shape doesn't have to mirror the UI: the planned `{ kind = "gamescope", args = [...] }` can hold any rendered argv, and nothing about gamescope's model prevents Cellar from later adding a third variant with typed fields (`{ kind = "gamescope", game_width = 1280, ... }`) that *renders itself into* the same argv at launch time. `serde(untagged)` handles the string-vs-map disjunction cleanly (TOML string vs TOML table are disjoint types; keep the legacy string variant first in the enum, and keep future sibling wrappers (`mangohud`, `gamemode`) in mind for ordering).

**Things that argue for validating args at launch time, not at config-write time:**
- Flag churn is real: 3.12 renamed `-U/-Y/-i/-n` → `-F/-S`; both Heroic and Lutris cope by probing `gamescope --help` output. If Cellar renders *semantic* config into current flags at launch, old configs don't break; if Cellar persists raw flags, they go stale (and are silently accepted-but-ignored by old gamescope, or rejected with exit 1 by new one — unknown flags fail with "See --help" + exit 1 in the `main()` parse loop).
- Unknown/`?` arguments fail hard (exit 1), and numeric parses (e.g. `parse_integer`) abort on garbage — so Cellar's seam should either validate against the flag table or render only from its own typed settings. A doctor-time `gamescope --help` probe (the Heroic/Lutris idiom, §4) is cheap and authoritative for version-aware rendering.
- gamescope is installed-system-wide, and its exact surface varies by build (NVIDIA/VR/disabled-builds compile things out); a static Cellar-side flag table is a fallback, not the source of truth.

**Net assessment:** the planned shape — `graphics = "gamescope"` (bare, legacy, defaults-only) OR `graphics = { kind = "gamescope", args = ["--flag", "value"] }`, with `args` rendered before `--` — matches gamescope's actual config model and launcher practice. Nothing in gamescope's design argues for a *different* union shape; the only refinements the evidence suggests: (1) treat `args` as the *rendering target* of typed settings later, not the UI contract; (2) do version-aware rendering (probe `--help`, prefer `-F/-S`); (3) document that `args` may include anything after the `--`-boundary semantics (order-preserving, one child); (4) HDR additionally needs child env vars (`DXVK_HDR=1`, `ENABLE_GAMESCOPE_WSI=1`) in the LaunchPlan — args alone don't express that.

## Sources

Primary (all fetched/read 2026-08-25):

* gamescope repository `ValveSoftware/gamescope`, master @ 2026-08-25, latest release tag `3.16.25` (per GitHub tags API):
  * Option table, `usage` string, `main()` parse loop, defaults, ConVar env-override loop, child-environment setup, `UpdateCompatEnvVars`, PipeWire init — [src/main.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/main.cpp) (option table L60–165, usage L167–292, main loop L739–872, ConVar env override L915–938, compat envs L614–644, child env L1075–1097, PipeWire L1099–1104)
  * `steamcompmgr_main()` parse loop (steamcompmgr flags), `subCommandArg`/`--` handling, `LaunchNestedChildren` (ENABLE_GAMESCOPE_WSI, mangoapp spawn), `--framerate-limit`/`--hdr-*`/`--reshade-*` handlers — [src/steamcompmgr.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/steamcompmgr.cpp) (loop L8682–8754, child spawn L8756–8760 + L8615–8664, framerate-limit L8741–8742)
  * Script/config system (Lua dirs, GAMESCOPE_SCRIPT_PATH) — [src/Script/Script.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/Script/Script.cpp) L111–138; [scripts/README.md](https://github.com/ValveSoftware/gamescope/blob/master/scripts/README.md); default scripts under `scripts/00-gamescope/`
  * Env vars read by gamescope — [src/Backends/DRMBackend.cpp](https://github.com/ValveSoftware/gamescope/blob/master/src/Backends/DRMBackend.cpp) L1037/L1328/L1984; main.cpp L979
  * README (modes, keyboard shortcuts, steam launch-option examples, `-W/-H/-w/-h/-r/-F/-S/-b/-f` semantics, defaults, reshade) — [README.md](https://github.com/ValveSoftware/gamescope/blob/master/README.md)
  * Pre-3.12 legacy flags (`-i --integer-scale`, `-n --nearest-neighbor-filter`, `-U --fsr-upscaling`, `-Y --nis-upscaling`) — [src/main.cpp @ tag 3.11.2](https://github.com/ValveSoftware/gamescope/blob/3.11.2/src/main.cpp) L28–41
* Heroic Games Launcher `HeroicGamesLauncher/HeroicGamesLauncher` main @ 2026-08-10 (primary launcher): gamescope command assembly, `--help` version detection, `--` last, `--mangoapp` coupling — [src/backend/launcher.ts](https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/blob/main/src/backend/launcher.ts) L584–703; settings model — [Gamescope.tsx](https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/blob/main/src/frontend/screens/Settings/components/Gamescope.tsx)
* Lutris `lutris/lutris` master @ 2026-08-22 (primary launcher): Gamescope system options and argv builder, FSR version detection, NVIDIA ≥515 gate, multi-GPU `--prefer-vk-device` — [lutris/runner_interpreter.py](https://github.com/lutris/lutris/blob/master/lutris/runner_interpreter.py) L81–149; [lutris/sysoptions.py](https://github.com/lutris/lutris/blob/master/lutris/sysoptions.py) L255–360; [lutris/util/linux.py](https://github.com/lutris/lutris/blob/master/lutris/util/linux.py) L269–287
* Bottles `bottlesdevs/Bottles` main @ 2026-08-25 (primary launcher): gamescope dialog fields, argv builder incl. auto `--hdr-enabled`, HDR env (`ENABLE_GAMESCOPE_WSI`/`DXVK_HDR`), temp-script wrapping — [bottles/frontend/windows/gamescope.py](https://github.com/bottlesdevs/Bottles/blob/main/bottles/frontend/windows/gamescope.py); [bottles/backend/wine/winecommand.py](https://github.com/bottlesdevs/Bottles/blob/main/bottles/backend/wine/winecommand.py) L165–183 + L916–975

Cross-check for the man page (see caveats): upstream ships **no** `gamescope.1` in master or recent tags (verified 404 for `gamescope.1.scd` at tags 3.11.2 and 3.14.1 — the man page was dropped from the tree years ago); neither [man.archlinux.org](https://man.archlinux.org) nor [manpages.debian.org](https://manpages.debian.org) indexes a gamescope man page (404 on direct lookup, 2026-08-25), so the flag list above is cross-checked against the source's own `usage` string instead. The [Linux Command Library "gamescope man"](https://linuxcommandlibrary.com/man/gamescope) page (secondary) is an unversioned community copy of the old man page — treats as unreliable; its flag set is a subset of the current source list.

Secondary:
* ArchWiki — [Gamescope](https://wiki.archlinux.org/title/Gamescope) (community wiki; upscaling launch-option examples matching the README pattern)
* Valve's support FAQ ["How do I use gamescope?"](https://help.steampowered.com/en/faqs/view/6F0E-5A31-4AC3-9A24) — exists, but is a JS-rendered React page; content could not be fetched from this environment, so nothing is quoted from it

## Unverified / caveats

* **Steam help FAQ content not verified** — see above; the Steam-launch-options claim rests on the gamescope README examples (primary), not the FAQ.
* **No upstream or distro man page to diff against** — Arch and Debian mirrors 404 (checked 2026-08-25); the option list is verified against the two parse loops + usage string at master. Distro packaging may carry out-of-tree man pages (e.g. Fedora `gamescope.spec` patches) that I could not verify; Debian's package filelist was bot-walled ("I Challenge Thee") on packages.debian.org.
* **VR/OpenVR flags** are compile-gated (`HAVE_OPENVR`) and were not exhaustively cross-checked beyond the table + usage string.
* **Line numbers cite master @ 2026-08-25** (option surface = 3.16.x series; latest tag 3.16.25). Gamescope's flag surface has changed before (3.12 `-U/-Y/-i` → `-F/-S`) and will again — treat the table as a snapshot and prefer `--help` probing for version-aware rendering.
* **ConVar names** are deliberately not enumerated: the env-override channel (`gamescope_<name>=<value>`) is runtime-defined and the script system is explicitly experimental; the note lists the mechanism, not a ConVar table.
* **`--disable-layers`** appears in the option table/usage but has no handler in either parse loop as of master — labeled vestigial; unverified whether a ConVar route exists in a build configuration I didn't grep (low risk).
## Cellar integration notes (#52)

Implemented in this slice; the notes pin the seam to the research above.

- **Parse point**: `PrefixDefaults::graphics_selection` in `cellar-core/src/entities.rs` — the only place that knows the recognized vocabulary (today exactly `gamescope`). Documented future shape: an untagged union — the legacy bare string (`graphics = "gamescope"`) or `{ kind = "gamescope", args = ["--flag", "value"] }`; args stay unparsed until a version-probed surface validates them at launch/doctor time (3.12 renamed `-U/-Y/-i/-n` → `-F/-S`; Heroic and Lutris probe `gamescope --help` for the same reason).
- **Unknown value**: warn-and-continue unwrapped — launch warns on stderr naming the prefix file and the valid values; the doctor plan section flags it (plan differs from intent); `prefix list` marks it `(unrecognized)`; JSON keeps the raw string.
- **Presence probe** at the activation rule (`wrappers_for`), via the shared execvp PATH predicate in `cellar-core/src/exec_lookup.rs` (the wine provider's lookup, generalized). Missing binary → `LaunchError::WrapperMissing` pre-spawn ("install gamescope via your system package manager"); the doctor plan section catches it through the same `plan_for`. The Spawn family's as-is OS error stays the backstop for a plan→spawn race.
- **Invocation boundary when args ship**: `gamescope [args] -- <wrapped-argv>` — args are order-sensitive and must precede `--`; the seam stores them as an ordered list.
- **HDR when implemented**: `--hdr-enabled` is two channels — `DXVK_HDR=1` + `ENABLE_GAMESCOPE_WSI=1` belong on the *wrapped* process, mapped through the existing plan env rungs.
- **High-value launcher arg set** (recorded from the option tables above): `-w/-h`, `-W/-H`, `-F fsr|nis|nearest` + `--fsr-sharpness`, `-S integer|stretch|fit`, `-r` FPS cap, `-f`/`-b`, `--expose-wayland`, `--hdr-enabled`, `--adaptive-sync`, `--mangoapp`, `--force-grab-cursor`, plus a free-form args escape hatch.
