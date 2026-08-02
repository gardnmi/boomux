---
date: 2026-08-01
topic: omarchy-terminal-selection
---

# Omarchy Terminal Selection

## Summary

Boomux will open new windows through Omarchy's default-terminal mechanism while
allowing a persistent configured terminal or a one-invocation CLI override.

---

## Problem Frame

Boomux currently requires Ghostty for every window it opens even though Herdr
owns the persistent terminal process independently. On an Omarchy installation
whose default terminal is Alacritty, this makes `boomux doctor` fail and forces
the user to install a second terminal emulator solely for Boomux.

---

## Key Flows

- F1. Open with the Omarchy default
  - **Trigger:** The user asks Boomux to open one or more new windows without selecting a terminal.
  - **Steps:** Boomux resolves Omarchy's current default and launches each Herdr attachment through it.
  - **Outcome:** The workspace opens in the same terminal emulator Omarchy would normally launch.
  - **Covered by:** R1, R4, R7
- F2. Open with an explicit terminal
  - **Trigger:** The user configures a terminal or supplies a terminal override for one invocation.
  - **Steps:** Boomux resolves the effective desktop entry, validates it, and launches new windows with it.
  - **Outcome:** The requested terminal is used without changing Omarchy's system-wide preference.
  - **Covered by:** R2, R3, R5, R6

---

## Requirements

**Terminal selection**

- R1. Boomux must use Omarchy's default terminal when no Boomux-specific terminal is selected.
- R2. Users must be able to persist a preferred terminal in Boomux configuration using an XDG desktop-entry ID.
- R3. Users must be able to select a terminal for one invocation using `--terminal <desktop-entry>`.
- R4. Selection precedence must be CLI override, Boomux configuration, then Omarchy's default.
- R5. Selecting a terminal while creating a workspace must imply opening it in a new window, even when `--new` is omitted.
- R6. An invalid or unavailable explicit terminal must fail with an actionable error rather than silently falling back.

**Launching and diagnostics**

- R7. Every flow that opens a new terminal window must use the effective terminal selection, including workspace restoration, dashboard actions, and `boomux open`.
- R8. Window titles and other optional terminal capabilities must be applied when supported without making unsupported capabilities a launch failure.
- R9. `boomux doctor` must validate Omarchy's terminal launcher and the configured or default terminal instead of requiring Ghostty.
- R10. Direct attachment in the invoking terminal must remain unchanged when neither `--new` nor `--terminal` is supplied.

---

## Acceptance Examples

- AE1. **Covers R1, R4, R7.** Given Omarchy defaults to Alacritty and Boomux has no terminal preference, when a workspace is restored, its windows open in Alacritty.
- AE2. **Covers R2, R4.** Given Omarchy defaults to Alacritty and Boomux is configured for a valid Ghostty desktop entry, when a workspace is restored, its windows open in Ghostty.
- AE3. **Covers R3, R4.** Given Boomux is configured for Ghostty, when the user restores a workspace with an Alacritty desktop-entry override, that invocation opens in Alacritty and the saved preference remains Ghostty.
- AE4. **Covers R5.** Given the user runs `boomux . --terminal Alacritty.desktop`, Boomux opens a new Alacritty window rather than attaching in the invoking terminal.
- AE5. **Covers R6.** Given an override names a missing desktop entry, Boomux reports that the selection cannot be launched and does not use another terminal.
- AE6. **Covers R9.** Given Ghostty is absent but Omarchy's default terminal is valid, `boomux doctor` reports the terminal check as healthy.
- AE7. **Covers R10.** Given the user runs `boomux .` without terminal-related options, Boomux attaches in the current terminal as it does today.

---

## Success Criteria

- An Omarchy user can install and use Boomux without installing Ghostty when another valid default terminal is configured.
- Users can temporarily or persistently choose a different installed terminal without changing Omarchy's global default.
- Planning can implement every launch and diagnostic path without inventing selection precedence, fallback behavior, or capability guarantees.

---

## Scope Boundaries

- Support is targeted at Omarchy and its `xdg-terminal-exec` integration; other Linux environments are best-effort only.
- Boomux will not change the system-wide terminal preference.
- macOS and Windows support are excluded.
- Per-emulator adapters, arbitrary command templates, and terminal-specific profiles are excluded.
- Optional capabilities such as titles do not carry cross-terminal guarantees.

---

## Key Decisions

- Use XDG desktop-entry IDs for explicit selection so Boomux relies on Omarchy's terminal metadata rather than emulator-specific command syntax.
- Keep both configuration and CLI selection; the CLI value is temporary and takes precedence.
- Treat an explicit CLI selection as a request for a new window because it cannot alter the terminal already running Boomux.
- Prefer a single Omarchy terminal-launching contract over maintaining dedicated Ghostty, Alacritty, or Kitty integrations.

---

## Dependencies / Assumptions

- The supported Omarchy environment provides `xdg-terminal-exec` and valid desktop-entry metadata for installed terminals.
- Herdr terminal attachment remains independent of the terminal emulator used to display it.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R2, R3, R6][Needs research] Determine the safest way to ask `xdg-terminal-exec` to resolve a specific desktop entry without changing the user's global preference or leaking a temporary XDG configuration into the attached shell.
