# Boomux Website

Static Astro landing page, independent of the Rust application and the embedded
web dashboard. No backend, analytics, or GPUI dependencies.

## Local preview

Use Node 22.12 or newer:

```console
cd website
npm ci
npm run dev
```

Open `http://localhost:4322/boomux/`. Browser tests reserve port 4321, so they
can run alongside your preview. For a production preview and browser checks:

```console
npm run build
npx playwright install chromium
npm test
npm run preview
```

An existing Chromium can be used with `CHROMIUM_PATH=/usr/bin/chromium npm test`.
Tests cover mobile layout, media playback and selection, reduced motion, assets,
Desktop installation, clipboard failure, theme persistence, blocked storage, and the
no-JavaScript fallback.

## Content and assets

- `src/pages/index.astro`: copy, links, and the official Desktop installer command.
- `src/components/MotionShowcase.astro`: real Desktop recordings with a Move /
  Resize / Keyboard selector, native playback controls, and GIF downloads.
- `src/scripts/motion.ts`: progressively enhanced selection and playback. Reduced
  motion disables autoplay; leaving the showcase or browser tab pauses playback.
  Without JavaScript, all three recordings remain manually playable.
- `src/styles/site.css`: responsive dark/light design.
- `public/demos/`: reviewed real application captures. MP4 is used on the page;
  GIFs are downloaded only on request. See [recording notes](demos.md) before
  replacing them. Do not include private paths, conversations, credentials, or
  client data in new captures.

## Publishing

The Website workflow checks pull requests and deploys successful builds from
`main`. Enable **Settings → Pages → Build and deployment → GitHub Actions** in
the repository when ready to publish. No Pages setting is changed by local builds.
The default URL is `https://gardnmi.github.io/boomux/`.

For a custom domain, set `SITE_URL` and `BASE_PATH` in the build environment
(for example `https://boomux.example` and `/`) and configure Pages/DNS separately.
The browser fixtures deliberately test the default project subpath.

Website-only changes have their own checks and do not select Rust builds.
Screenshot changes also trigger the site workflow. Keep installer claims aligned
with `docs/install.md`; the website does not publish a separate installer.
