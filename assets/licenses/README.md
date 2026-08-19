# Third-Party Web Assets

`assets/mobile-web/terminal.js` and `assets/mobile-web/ghostty-vt.wasm` are
generated from the pinned `ghostty-web` package in
`assets/mobile-web/package.json` and `bun.lock`.

- `ghostty-web` 0.4.0 is MIT licensed by Coder; see `ghostty-web-MIT.txt`.
- Its WASM embeds Ghostty code licensed by Mitchell Hashimoto and Ghostty
  contributors; see `ghostty-MIT.txt`.

The checked-in 0.4.0 outputs have these SHA-256 digests:

```text
851865a99f745e3ad2cdae15731dde08035412f7b8bb1b2a22ff04a969a07d31  terminal.js
d6f0326f1874ad2ce9f289e3a4a0c5f3507d4cb38d8747e4b287def470a0c60a  ghostty-vt.wasm
```

Regenerate the checked-in assets with:

```console
cd assets/mobile-web
bun install --frozen-lockfile
bun run build
```

`bun run check` rebuilds the assets and verifies these pinned digests.
