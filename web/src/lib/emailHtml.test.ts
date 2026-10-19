import { describe, expect, it } from "vitest";
import { strToU8, zipSync } from "fflate";
import {
  imageContentType,
  normalizeAssetPath,
  readEmailExport,
} from "./emailHtml";

function zip(files: Record<string, string>): Blob {
  const entries: Record<string, Uint8Array> = {};
  for (const [name, body] of Object.entries(files))
    entries[name] = strToU8(body);
  return new Blob([zipSync(entries) as Uint8Array<ArrayBuffer>]);
}

describe("normalizeAssetPath", () => {
  it("matches the API's normalization", () => {
    expect(normalizeAssetPath("./images/a.png")).toBe("images/a.png");
    expect(normalizeAssetPath("/images/a.png")).toBe("images/a.png");
    expect(normalizeAssetPath(" images\\a.png ")).toBe("images/a.png");
  });
});

describe("imageContentType", () => {
  it("knows the raster types and refuses the rest", () => {
    expect(imageContentType("Logo.PNG")).toBe("image/png");
    expect(imageContentType("hero.jpg")).toBe("image/jpeg");
    expect(imageContentType("icon.svg")).toBeNull();
    expect(imageContentType("notes")).toBeNull();
  });
});

describe("readEmailExport", () => {
  it("finds index.html and the images it uses, relative to it", async () => {
    const exported = await readEmailExport(
      zip({
        "newsletter/index.html":
          '<img src="images/logo.png"><img src="./images/hero.jpg">',
        "newsletter/images/logo.png": "png",
        "newsletter/images/hero.jpg": "jpg",
        "newsletter/images/unused.png": "png",
        "newsletter/images/icon.svg": "svg",
        "__MACOSX/newsletter/._index.html": "junk",
      }),
    );
    expect(exported.html).toContain("images/logo.png");
    expect(exported.images.map((i) => i.path).sort()).toEqual([
      "images/hero.jpg",
      "images/logo.png",
    ]);
    const logo = exported.images.find((i) => i.path === "images/logo.png");
    expect(logo?.file.name).toBe("logo.png");
    expect(logo?.file.type).toBe("image/png");
  });

  it("falls back to the shallowest HTML file", async () => {
    const exported = await readEmailExport(
      zip({ "deep/a/b.html": "deep", "email.html": "top" }),
    );
    expect(exported.html).toBe("top");
  });

  it("explains a zip with no HTML, or a file that isn't a zip", async () => {
    await expect(readEmailExport(zip({ "a.png": "x" }))).rejects.toThrow(
      /no HTML/,
    );
    await expect(readEmailExport(new Blob(["nope"]))).rejects.toThrow(
      /readable/,
    );
  });
});
