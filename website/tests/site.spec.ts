import { test, expect } from "@playwright/test";

test("page, base-prefixed assets, and expanded capture fit the viewport", async ({
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
    "Keep the work.",
  );
  await page.getByText("See a real workspace capture").click();
  const capture = page.locator(".capture img");
  await expect(capture).toBeVisible();
  await expect
    .poll(() =>
      capture.evaluate(
        (img: HTMLImageElement) => img.complete && img.naturalWidth > 0,
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

test("install selection preserves exact mode-specific commands and supports keyboard navigation", async ({
  page,
}) => {
  await page.goto("./#install");
  const desktop = page.getByRole("tab", { name: "Desktop + CLI" });
  await expect(desktop).toHaveAttribute("aria-selected", "true");
  await desktop.focus();
  await page.keyboard.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "CLI only" })).toBeFocused();
  await expect(page.locator("#install-desktop")).toBeHidden();
  await expect(page.locator("#command-cli")).toHaveText(
    "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh -s -- --cli",
  );
  await page.keyboard.press("Home");
  await expect(page.locator("#install-desktop")).toBeVisible();
  await expect(page.locator("#command-desktop")).toContainText(
    "sh -s -- --desktop",
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

test("both install commands and navigation work without JavaScript", async ({
  browser,
}) => {
  const context = await browser.newContext({ javaScriptEnabled: false });
  const page = await context.newPage();
  await page.goto("http://127.0.0.1:4321/boomux/");
  await expect(page.locator("#command-desktop")).toBeVisible();
  await expect(page.locator("#command-cli")).toBeVisible();
  await expect(page.locator("#install-tabs")).toBeHidden();
  await page.getByRole("link", { name: "Get Boomux" }).click();
  await expect(page).toHaveURL(/#install$/);
  await context.close();
});
