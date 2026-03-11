# Lege Document Processing Application

This is a walkthrough of all the features in the *Lege* document processing application.  The name is derived from the Latin verb for reading, Legere.
---

### Book Processing Ecosystem

Lege is intended to be part of a mini-ecosystem of book processing:

1. **Get the Book PDF Scan**  
   Obtain the full book PDF scan on your computer. Use a hardware book scanner for old books you own, or access sites like  
   [Internet Archive](https://www.archive.org) or [Project Gutenberg](https://www.gutenberg.org).  
   For archive.org, the [Internet Archive Downloader](https://github.com/elementdavv/internet_archive_downloader) is the best way to download the full book as a PDF if not directly available.

2. **Manage with Calibre**  
   Use Calibre to manage your books. Lege is designed to complement Calibre at this stage of development.

3. **Process with Lege**  
   Process your books with Lege, the output folder defaults to the input folder. Supports PDF and DJVU outputs, with an optional OCR layer.  
   EPUB support was planned but is not offered due to severe technical difficulties, best outlined [here](https://manual.calibre-ebook.com/conversion.html#pdfconversion). Future support possible, using Epub-fxl.  
   Rendered images in 1-bit binarized fax format provide the best compromise for PDF inputs.

4. **Send to E-ink Reader**  
   Transfer the processed file to your E-ink reader. The best compact E-ink reader for external books is a **Kobo** model, ideally used with **KoReader** for DJVU support, faster book loading, page turning, WebDAV support, and more.

---

## Features

There is also a CLI in the main program folder that is quick and easy to use. Some installers make a shortcut for it, some, like the Microsoft Store version, don't.

### Outputs

- **PDF and DJVU**  
  PDF is offered as an output just as it is the sole input besides a folder of images if they are numbered sequentially.  
  DJVU is the other supported format. DJVU was a much-publicized archival format in the early 2000s which ran into copyright issues and poor marketing, so it never gained wide adoption—and then Adobe essentially adopted most of its best features for the PDF 2.0 standard.  
  But DJVU is still the superior format for this specific use; its image compression is better across the board, and its standardized encoding and decoding allow much faster loading than your standard PDF document.  
  If your e-ink reader supports DJVU, definitely choose it here.

---

### Image Output Type

- **Dithering and Original**  
  Dithering is the method of reducing diffusion error when reducing the color space of an image, leading to a much more natural result.  
  When dithering is enabled, a picture is reduced from 16 million color space down to 1-bit color while preserving fine detail where possible.  
  CCITT4 uses a custom dithering method while JBIG2 uses Stucki by default. There is also an option that uses spec-correct JBIG2 halftone regions in the CLI, the output is worse quality but the file sizes are lower.
  
  CCITT Group 4 and JBIG2, fax formats from the 90's, are used for text and dithered image areas. In the GUI the choice is between dithered or original image areas. JBIG2 is used if dithered is chosen and CCITT4 is used if original images by default.

---

### Layout Detection

This is achieved with a PaddleX model. It detects 21 different layout elements on each page, but for the program’s purposes we only pay attention to 8.

Layout detection per-page takes the same amount of time no matter what options are selected. Windows uses DirectML, Linux uses WebGPU via Dawn, MacOS uses CoreML (March 2026, CUDA is now an option again for Windows and Linux, but you need CUDA and CUDNN on PATH). The purpose of layout detection is to identify and protect image regions from binarization, because the binarization methods will ruin picture quality so they must be treated separately per page.  Layout detection is on by default. The best reason to turn it off is to get image areas processed by the same binarization as text for certain situations like documents with only line art.

---

### OCR Text Layer

This is an **HOCR** layer added via a temporarily created high-quality OCR image per page.  Windows uses WinOCR from the Windows SDK while Linux and MacOS use Tesseract. Now if your input PDF has an OCR layer, the program will retain it in the output if you keep OCR off. If you enable OCR, a new layer will be made instead. To clarify, the program does not edit the old PDF in order to make the output. It is making an entirely new PDF from scratch, per-page.
---

### Page Range

You can set the page range to process.  
If you set a page range, the cover image feature will be disabled automatically.  
Otherwise, the first page is always treated as the cover, and thus as a full-page image area.

---

### Target Height / Width

You can set the target height of the output PDF pages in pixels.  
Width is set proportionately based on the original proportions of the page.  

Functionality has been recently added to set the output, unproportionately, to the exact screen dimensions of specific e-ink readers.  
That way no fiddling is necessary to get the pages to crop or center or scale to the screen—although the text itself will appear different than the original page since its proportions have been altered.  

There is no clean final way to do this, but we want to provide you with all the compromises.

---

### Margin Correction and Deskew

Margin correction is done via the bounding boxes from layout detection. Centering will equalize the margins on either side versus the dimensions of the main bounding box areas. Cropping will crop as much as possible towards those boxes while preserving aspect ratio. Footnotes will be preserved. Forced margin cropping can be set. There is also algorithmic margin detection that works for when layout detection is off, and produces similar results.

Deskew is another PaddleX trained neural model. It performs well in testing for pages distorted in some form from a physical scanning process. Be aware that it will subtly skew unskewed documents if enabled for them. Additionally, the PaddleX orientation model is run before deskewing to verify page orientation, and it will correct any rotated documents before deskewing. 

### Adaptive Binarization

Uses a **Sauvola / Otsu fusion** method (two well-known binarization techniques) which works great every time.  
Source cited in the *Licenses* section even though it was unlicensed and free to use, it's just that good so we want people to know where it came from.  There is one issue where it will try to binarize blank or almost blank pages and produce static patterns. There is code to prevent this when layout mode is turned on (if there are no detections on a page, it uses a threshold at 128 for any blank page), but when layout detection is turned off and adaptive binarization is used, you will see blank pages being affected this way.

**Heavy model**

Document binarization is an entire field—there is a yearly international contest for the best document binarization method.  
In recent years they have moved completely to neural models as the contestants, where before it was all algorithms.  

Thus, Lege also supports a heavier neural model that was a participant in one such contest and performs very well for historical and degraded documents.  
It’s offered as a backup in case the “light” method creates or retains imperfections in the final output.  

It is much slower, up to 3 seconds per page to process.

---

### Log Feature

Lastly, there is a log feature in the bottom right that shows the last 20 documents processed in Lege.  
You can save your current settings in the top right, and also reset them to defaults.

### Recommended settings

Here's a few recommendations for using the program based on the input book-
1. Old book with yellowed but undamaged pages with images with solid, simple coloring - Just keep layout detection off and run it with fixed threshold from between 128 to 200. The fixed threshold will affect images the least.
2. Old book with yellowed damaged discolored spotty pages - use adaptive binarization, it's designed for that kind of book. The heavy model is for when even the text is degraded.
3. Book with gray or other color pages and full color images - use adaptive binarization and enable layout detection, the image areas will be preserved. if the pages are gray/blue, original images look better, if yellowed, dithering usually looks better as the entire page gets binarized with image areas dithered nicely.

### One thing
There's a question someone out there might be thinking of asking; "I have a book that's 500mb but it's already black and white, how do I re-encode it to a manageable size without running it through binarization?" The answer is, the process of re-encoding the book is binarization, the decision still has to be made for what pixels to turn white and what pixels to turn black. The best option is to test different simple threshold modes starting from 180 or 200. If you want to re-encode in JPEG without affecting page color much, there is an option for it in the CLI, but file size won't be reduced as much as with the two fax-based encoding modes.

