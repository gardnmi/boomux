import { test, expect } from "@playwright/test";

test("page, base-prefixed recordings, and poster fit the viewport", async ({
  page,
}) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("response", (response) => {
    if (
      response.url().startsWith("http://127.0.0.1:4321") &&
      response.status() >= 400
    )
      errors.push(response.url());
  });
  await page.goto("./");
  await expect(page.getByRole("heading", { level: 1 })).toContainText(
    "Your rules.",
  );
  await page.getByRole("link", { name: "See it move" }).click();
  const capture = page.locator("#motion-move video");
  await expect(capture).toBeVisible();
  await expect
    .poll(() =>
      capture.evaluate(
        (video: HTMLVideoElement) => video.readyState >= 2 && video.videoWidth === 1280,
      ),
    )
    .toBeTruthy();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBeTruthy();
  await expect(page.locator('link[rel="canonical"]')).toHaveAttribute(
    "href",
    "https://gardnmi.github.io/boomux/",
  );
  expect(errors).toEqual([]);
});

test("demonstrations support keyboard selection and pause hidden clips", async ({ page }) => {
  await page.goto("./#in-motion");
  const move = page.getByRole("tab", { name: "Move", exact: true });
  await move.focus();
  await page.keyboard.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "Resize", exact: true })).toBeFocused();
  await expect(page.locator("#motion-move")).toBeHidden();
  expect(await page.locator("#motion-move video").evaluate((v: HTMLVideoElement) => v.paused)).toBe(true);
  await expect.poll(() => page.locator("#motion-resize video").evaluate((v: HTMLVideoElement) => v.videoWidth)).toBe(1280);
  await page.keyboard.press("End");
  await expect(page.locator("#motion-keyboard")).toBeVisible();
  await expect.poll(() => page.locator("#motion-keyboard video").evaluate((v: HTMLVideoElement) => v.videoWidth)).toBe(1280);
  await expect(page.locator("#motion-keyboard a[download]")).toHaveAttribute("href", "/boomux/demos/keyboard.gif");
  await page.keyboard.press("Home");
  await expect(move).toBeFocused();
  await page.locator("#motion-move video").evaluate((v: HTMLVideoElement) => v.pause());
  await page.locator("#install").scrollIntoViewIfNeeded();
  await page.locator("#in-motion").scrollIntoViewIfNeeded();
  expect(await page.locator("#motion-move video").evaluate((v: HTMLVideoElement) => v.paused)).toBe(true);
});

test("reduced motion prevents autoplay but keeps manual playback available", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("./#in-motion");
  const video = page.locator("#motion-move video");
  expect(await video.evaluate((v: HTMLVideoElement) => v.paused && !v.autoplay)).toBe(true);
  await video.evaluate((v: HTMLVideoElement) => v.play());
  await expect.poll(() => video.evaluate((v: HTMLVideoElement) => v.currentTime)).toBeGreaterThan(0);
  await page.getByRole("tab", { name: "Keyboard", exact: true }).click();
  expect(await page.locator("#motion-keyboard video").evaluate((v: HTMLVideoElement) => v.paused)).toBe(true);
  expect(await video.evaluate((v: HTMLVideoElement) => v.paused)).toBe(true);
});

test("installation offers only the exact Desktop command", async ({
  page,
}) => {
  await page.goto("./#install");
  await expect(page.locator("#install-desktop")).toBeVisible();
  await expect(page.locator("#install code")).toHaveCount(1);
  await expect(page.locator("#install [role=tab]")).toHaveCount(0);
  await expect(page.locator("#command-desktop")).toHaveText(
    "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh -s -- --desktop",
  );
});

test("clipboard copies displayed command and reports denied access", async ({
  page,
}) => {
  await page.goto("./#install");
  await page.evaluate(() => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        writeText: async (value: string) => {
          document.documentElement.dataset.copied = value;
        },
      },
    });
  });
  await page
    .getByRole("button", { name: "Copy desktop install command" })
    .click();
  expect(await page.locator("html").getAttribute("data-copied")).toEqual(
    await page.locator("#command-desktop").textContent(),
  );
  await expect(page.getByRole("status")).toContainText("Copied.");
  await page.evaluate(() =>
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        writeText: async () => {
          throw new Error("denied");
        },
      },
    }),
  );
  await page
    .getByRole("button", { name: "Copy desktop install command" })
    .click();
  await expect(page.getByRole("status")).toContainText("Select and copy");
});

test("theme persists and remains usable when storage is blocked", async ({
  page,
}) => {
  await page.goto("./");
  await page.getByRole("button", { name: "Use light theme" }).click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await page.addInitScript(() => {
    Storage.prototype.getItem = () => {
      throw new Error("blocked");
    };
    Storage.prototype.setItem = () => {
      throw new Error("blocked");
    };
  });
  await page.reload();
  await page.getByRole("button", { name: "Use light theme" }).click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
});

test("Desktop installation and navigation work without JavaScript", async ({
  browser,
}) => {
  const context = await browser.newContext({ javaScriptEnabled: false });
  const page = await context.newPage();
  await page.goto("http://127.0.0.1:4321/boomux/");
  await expect(page.locator("#command-desktop")).toBeVisible();
  await expect(page.locator("#install code")).toHaveCount(1);
  await expect(page.locator(".motion-tabs")).toBeHidden();
  for (const clip of ["move", "resize", "keyboard"]) {
    await expect(page.locator(`#motion-${clip} video`)).toBeVisible();
    await expect(page.locator(`#motion-${clip} video`)).toHaveAttribute("controls", "");
  }
  await page.getByRole("link", { name: "Get Boomux" }).click();
  await expect(page).toHaveURL(/#install$/);
  await context.close();
});
