"use strict";

const STORAGE_KEY = "boomux-agent-theme";

const PALETTES = [
  ["catppuccin", "Catppuccin", "dark", "#89b4fa", "#45475a", "#1e1e2e", "#161622", "#313244", "#cdd6f4", "#bac2de", "#f38ba8", "#f9e2af", "#a6e3a1", "#94e2d5", "#89b4fa"],
  ["catppuccin-latte", "Catppuccin Latte", "light", "#1e66f5", "#ccd0da", "#eff1f5", "#e3e4e8", "#dce0e8", "#4c4f69", "#5c5f77", "#d20f39", "#df8e1d", "#40a02b", "#179299", "#1e66f5"],
  ["ethereal", "Ethereal", "dark", "#7d82d9", "#252e56", "#060B1E", "#040816", "#131a3a", "#ffcead", "#c9b8a6", "#ED5B5A", "#E9BB4F", "#92a593", "#a3bfd1", "#7d82d9"],
  ["everforest", "Everforest", "dark", "#7fbbb3", "#3d484d", "#2d353b", "#21272c", "#343f44", "#d3c6aa", "#9da9a0", "#e67e80", "#dbbc7f", "#a7c080", "#83c092", "#7fbbb3"],
  ["flexoki-light", "Flexoki Light", "light", "#205EA6", "#CECDC3", "#FFFCF0", "#f2efe4", "#E6E4D9", "#100F0F", "#403E3C", "#D14D41", "#D0A215", "#879A39", "#3AA99F", "#205EA6"],
  ["gruvbox", "Gruvbox", "dark", "#7daea3", "#504945", "#282828", "#1e1e1e", "#3c3836", "#d4be98", "#bdae93", "#ea6962", "#d8a657", "#a9b665", "#89b482", "#7daea3"],
  ["hackerman", "Hackerman", "dark", "#82FB9C", "#1f253a", "#0B0C16", "#080910", "#151828", "#ddf7ff", "#b5c5db", "#50f872", "#50f7d4", "#4fe88f", "#7cf8f7", "#829dd4"],
  ["kanagawa", "Kanagawa", "dark", "#dcd7ba", "#363646", "#1f1f28", "#17171e", "#223249", "#dcd7ba", "#c8c093", "#c34043", "#c0a36e", "#76946a", "#6a9589", "#7e9cd8"],
  ["last-horizon", "Last Horizon", "dark", "#b59790", "#584e51", "#0c0b0c", "#090809", "#0c0b0c", "#FAFCFB", "#cfd3cd", "#c38b7b", "#6B5E73", "#87a9b0", "#a5a0b6", "#b59790"],
  ["lumon", "Lumon", "dark", "#8bc9eb", "#243d56", "#16242d", "#101b21", "#1b2d40", "#d6e2ee", "#d6e2ee", "#4d86b0", "#6fa4c9", "#5e95bc", "#b4e4f6", "#6fb8e3"],
  ["lupine", "Lupine", "light", "#3264eb", "#d0d0d0", "#fafafa", "#ececec", "#f5f5f5", "#212121", "#424242", "#c900c4", "#026fde", "#4a2fd0", "#0c67de", "#3264eb"],
  ["matte-black", "Matte Black", "dark", "#e68e0d", "#2a2a2a", "#121212", "#0d0d0d", "#1e1e1e", "#bebebe", "#8a8a8d", "#D35F5F", "#b91c1c", "#FFC107", "#bebebe", "#e68e0d"],
  ["miasma", "Miasma", "dark", "#78824b", "#383838", "#222222", "#191919", "#2c2c2c", "#c2c2b0", "#8a8a7e", "#685742", "#b36d43", "#5f875f", "#c9a554", "#78824b"],
  ["nord", "Nord", "dark", "#81a1c1", "#434c5e", "#2e3440", "#222730", "#3b4252", "#d8dee9", "#adb5c4", "#bf616a", "#ebcb8b", "#a3be8c", "#88c0d0", "#81a1c1"],
  ["osaka-jade", "Osaka Jade", "dark", "#509475", "#32473B", "#111c18", "#0c1512", "#23372B", "#C1C497", "#D6D5BC", "#FF5345", "#459451", "#549e6a", "#2DD5B7", "#509475"],
  ["retro-82", "Retro 82", "dark", "#faa968", "#134e5a", "#05182e", "#031222", "#0a2540", "#f6dcac", "#a7c9c6", "#f85525", "#e97b3c", "#028391", "#8cbfb8", "#3f8f8a"],
  ["ristretto", "Ristretto", "dark", "#f38d70", "#403e41", "#2c2525", "#211b1b", "#3d2f2a", "#e6d9db", "#c3b7b8", "#fd6883", "#f9cc6c", "#adda78", "#85dacc", "#f38d70"],
  ["rose-pine", "Rose Pine", "light", "#56949f", "#dfdad9", "#faf4ed", "#ede7e1", "#f2e9e1", "#575279", "#6e6a86", "#b4637a", "#ea9d34", "#286983", "#d7827e", "#56949f"],
  ["solitude", "Solitude", "dark", "#798186", "#343d41", "#101315", "#0c0e10", "#101315", "#cacccc", "#cbc2be", "#565d60", "#d9dbdc", "#9fa5a9", "#707070", "#798186"],
  ["tokyo-night", "Tokyo Night", "dark", "#7aa2f7", "#292e42", "#1a1b26", "#13141c", "#24283b", "#a9b1d6", "#b4bee6", "#f7768e", "#e0af68", "#9ece6a", "#449dab", "#7aa2f7"],
  ["vantablack", "Vantablack", "dark", "#8d8d8d", "#1a1a1a", "#000000", "#090909", "#1a1a1a", "#ffffff", "#ececec", "#a4a4a4", "#cecece", "#b6b6b6", "#b0b0b0", "#8d8d8d"],
  ["white", "White", "light", "#6e6e6e", "#c0c0c0", "#ffffff", "#f5f5f5", "#c0c0c0", "#000000", "#000000", "#2a2a2a", "#4a4a4a", "#3a3a3a", "#3e3e3e", "#1a1a1a"],
];

export const THEMES = PALETTES.map(([
  slug,
  name,
  mode,
  accent,
  border,
  background,
  surface,
  surfaceRaised,
  text,
  muted,
  attention,
  warning,
  idle,
  working,
  heading,
]) => ({
  slug,
  name,
  mode,
  colors: {
    background,
    surface,
    surfaceRaised,
    border,
    borderActive: accent,
    text,
    textSubtle: muted,
    muted,
    heading,
    selection: accent,
    selectionText: background,
    idle,
    working,
    attention,
    warning,
  },
}));

export const DEFAULT_THEME = "catppuccin";

export function savedTheme() {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    return THEMES.some((theme) => theme.slug === saved) ? saved : DEFAULT_THEME;
  } catch (_) {
    return DEFAULT_THEME;
  }
}

export function applyTheme(slug, persist = true) {
  const theme = THEMES.find((candidate) => candidate.slug === slug)
    || THEMES.find((candidate) => candidate.slug === DEFAULT_THEME);
  const root = document.documentElement;
  root.dataset.theme = theme.slug;
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", theme.colors.background);
  if (persist) {
    try {
      localStorage.setItem(STORAGE_KEY, theme.slug);
    } catch (_) {
      // The active theme still applies when storage is unavailable.
    }
  }
  return theme;
}
