# JPEG decoder fixtures

Small (64x48) JPEG streams produced by independent encoders, with
libjpeg-derived ground truth, used by `tests/jpeg_fixtures.rs`:

| file | producer | exercises |
|---|---|---|
| `prog_gray.jpg` | Pillow | progressive scans, grayscale |
| `prog_rgb420.jpg` | Pillow | progressive, YCbCr 4:2:0 |
| `base_rgb422.jpg` | Pillow | baseline, YCbCr 4:2:2 |
| `restart_gray.jpg` | cjpeg `-restart 1` | restart markers, baseline |
| `prog_restart_gray.jpg` | cjpeg `-progressive -restart 1` | restart markers inside progressive scans |
| `cmyk.jpg` | Pillow | Adobe APP14 transform 0, Photoshop-inverted CMYK |
| `cmyk_noninverted.jpg` | Pillow (source pre-inverted) | convention-violating CMYK writer |
| `ycck.jpg` | ImageMagick | Adobe APP14 transform 2 (YCCK), pure-ink patches |

`*.truth.bin` = Pillow/libjpeg's decode of the `.jpg` (raw samples,
row-major). `*.src.bin` = the pre-JPEG source pixels. Regeneration
commands live in the git history of this directory.
