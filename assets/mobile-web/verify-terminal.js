const expected = {
  "terminal.js": "bc99d2ced6b43d8e029b7bc7d6c244dab8f2f0e49d441783841915de3b4d5270",
  "ghostty-vt.wasm": "d6f0326f1874ad2ce9f289e3a4a0c5f3507d4cb38d8747e4b287def470a0c60a",
  "../licenses/ghostty-web-MIT.txt": "5eccd0eeca906db6d661b64dd05e1d4a4b2e49d37d43bbcfd2c8cce4d7832920",
  "../licenses/ghostty-MIT.txt": "386211873e5b7a02f663ae4d7adf96285999f91608f8f9f31fecfd0f4095e6f1",
};

for (const [path, digest] of Object.entries(expected)) {
  const bytes = await Bun.file(path).arrayBuffer();
  const actual = new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
  if (actual !== digest) {
    throw new Error(`${path} digest ${actual} does not match ${digest}`);
  }
}
