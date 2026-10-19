import { strFromU8, unzipSync } from "fflate";

/**
 * Image types an HTML broadcast can host — mirrors `BROADCAST_ASSET_TYPES`
 * in the API's `graphql/mutations.rs`, which is the authority.
 */
const IMAGE_TYPES: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
};

/** Mirrors `MAX_BROADCAST_ASSET_BYTES` in the API. */
export const MAX_ASSET_BYTES = 5 * 1024 * 1024;

/** Mirrors `broadcast_html::MAX_HTML_BYTES` in the API. */
export const MAX_HTML_BYTES = 100_000;

/**
 * Gmail hides everything past ~102 KB of a message behind "View entire
 * message" — worth a warning well before the API's own limit.
 */
export const GMAIL_CLIP_BYTES = 90_000;

/** The content type to upload `filename` as, or `null` if it can't be used. */
export function imageContentType(filename: string): string | null {
  const ext = filename.split(".").pop()?.toLowerCase() ?? "";
  return IMAGE_TYPES[ext] ?? null;
}

/**
 * The key an image path is matched on — mirrors
 * `broadcast_html::normalize_asset_path`: `./images/a.png`, `images/a.png`
 * and `/images/a.png` are all `images/a.png`.
 */
export function normalizeAssetPath(path: string): string {
  return path
    .trim()
    .replaceAll("&amp;", "&")
    .replaceAll("\\", "/")
    .replace(/^(\.\/|\/)+/, "");
}

/** UTF-8 byte length — what the API's size limits count. */
export function byteLength(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** One image from an export, ready to upload. */
export interface ExportImage {
  /** Its path relative to the HTML file — what the HTML refers to it by. */
  path: string;
  file: File;
}

/** An unpacked email export: the HTML, and the images it refers to. */
export interface EmailExport {
  html: string;
  images: ExportImage[];
}

/**
 * Unpack an email export `.zip` (`index.html` plus an images folder — what
 * most email builders, MJML builds, or Claude produce) in the browser. The
 * HTML is `index.html` if there is one, else the shallowest `.html` file;
 * the images are every supported image file under its folder that the HTML
 * mentions by path. Nothing is uploaded here.
 */
export async function readEmailExport(zip: Blob): Promise<EmailExport> {
  let entries: Record<string, Uint8Array>;
  try {
    entries = unzipSync(new Uint8Array(await zip.arrayBuffer()));
  } catch {
    throw new Error("That file isn't a readable .zip.");
  }
  const files = Object.keys(entries).filter(
    (name) => !name.endsWith("/") && !name.startsWith("__MACOSX/"),
  );
  const depth = (name: string) => name.split("/").length;
  const byDepth = (a: string, b: string) => depth(a) - depth(b);
  const htmlFile =
    files
      .filter((n) => n.split("/").pop()?.toLowerCase() === "index.html")
      .sort(byDepth)[0] ??
    files.filter((n) => /\.html?$/i.test(n)).sort(byDepth)[0];
  if (!htmlFile) throw new Error("The .zip has no HTML file in it.");

  const html = strFromU8(entries[htmlFile]);
  const dir = htmlFile.includes("/")
    ? htmlFile.slice(0, htmlFile.lastIndexOf("/") + 1)
    : "";
  const images: ExportImage[] = [];
  for (const name of files) {
    if (!name.startsWith(dir)) continue;
    const type = imageContentType(name);
    const path = name.slice(dir.length);
    if (!type || !html.includes(path)) continue;
    const base = name.split("/").pop() ?? name;
    images.push({
      path,
      file: new File([entries[name] as Uint8Array<ArrayBuffer>], base, {
        type,
      }),
    });
  }
  return { html, images };
}
