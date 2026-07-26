#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const pdfjsRoot = path.resolve(here, "../../pdf.js");
const { getDocument } = await import(
  path.join(pdfjsRoot, "build/lib-legacy/pdf.js")
);

const [pdfPath, pagesText, scaleText, outputDir] = process.argv.slice(2);
if (!pdfPath || !pagesText || !scaleText || !outputDir) {
  console.error("usage: render.mjs <pdf> <zero-based-pages> <scale> <output-dir>");
  process.exit(2);
}

const pages = pagesText.split(",").map(Number);
const scale = Number(scaleText);
fs.mkdirSync(outputDir, { recursive: true });
const task = getDocument({
  data: new Uint8Array(fs.readFileSync(pdfPath)),
  cMapUrl: path.join(pdfjsRoot, "external/bcmaps/"),
  cMapPacked: true,
  standardFontDataUrl: path.join(pdfjsRoot, "external/standard_fonts/"),
  wasmUrl: path.join(pdfjsRoot, "external/openjpeg/"),
  useSystemFonts: true,
});
const document = await task.promise;
try {
  for (const pageIndex of pages) {
    const page = await document.getPage(pageIndex + 1);
    const viewport = page.getViewport({ scale });
    const canvasFactory = document.canvasFactory;
    const target = canvasFactory.create(
      Math.floor(viewport.width),
      Math.floor(viewport.height)
    );
    await page.render({
      canvasContext: target.context,
      viewport,
      background: "rgb(255,255,255)",
      annotationMode: 1,
    }).promise;
    fs.writeFileSync(
      path.join(outputDir, `page-${pageIndex}.png`),
      target.canvas.toBuffer("image/png")
    );
    canvasFactory.destroy(target);
    page.cleanup();
  }
} finally {
  await task.destroy();
}
