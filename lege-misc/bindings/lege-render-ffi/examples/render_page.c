/*
 * Minimal C consumer of the Lege renderer.
 *
 *   cc -Iinclude examples/render_page.c -o render_page \
 *      -L../../../target/debug -llege_render -lm -lpthread -ldl
 *   ./render_page in.pdf 0 out.png 300
 */
#include "lege_render.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void fail(const char *what) {
    const char *detail = lege_last_error_message();
    fprintf(stderr, "%s: %s\n", what, detail ? detail : "(no message)");
    exit(1);
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: %s <in.pdf> <page> <out.png> [dpi]\n", argv[0]);
        return 2;
    }
    const char *input = argv[1];
    unsigned page = (unsigned)strtoul(argv[2], NULL, 10);
    const char *output = argv[3];
    double dpi = argc > 4 ? strtod(argv[4], NULL) : 150.0;

    FILE *file = fopen(input, "rb");
    if (!file) { perror("open"); return 1; }
    fseek(file, 0, SEEK_END);
    long size = ftell(file);
    fseek(file, 0, SEEK_SET);
    unsigned char *pdf = malloc((size_t)size);
    if (fread(pdf, 1, (size_t)size, file) != (size_t)size) { perror("read"); return 1; }
    fclose(file);

    LegeDocument *doc = lege_document_open(pdf, (size_t)size, NULL);
    free(pdf);
    if (!doc) fail("lege_document_open");

    printf("%u pages\n", lege_document_page_count(doc));

    double width_pt = 0, height_pt = 0;
    if (lege_document_page_size(doc, page, &width_pt, &height_pt) != LEGE_OK)
        fail("lege_document_page_size");
    printf("page %u: %.1f x %.1f pt\n", page, width_pt, height_pt);

    uint32_t width_px = 0, height_px = 0;
    if (lege_document_page_pixel_size(doc, page, dpi, &width_px, &height_px) != LEGE_OK)
        fail("lege_document_page_pixel_size");
    printf("at %.0f dpi: %u x %u px\n", dpi, width_px, height_px);

    LegeRenderOptions options = lege_render_options_default();
    options.dpi = dpi;
    if (strstr(output, ".jpg") || strstr(output, ".jpeg")) {
        options.format = LEGE_FORMAT_JPEG;
    }

    LegeBuffer image = {0};
    if (lege_document_render_page(doc, page, &options, NULL, &image) != LEGE_OK)
        fail("lege_document_render_page");

    FILE *out = fopen(output, "wb");
    if (!out) { perror("create"); return 1; }
    fwrite(image.data, 1, image.len, out);
    fclose(out);
    printf("wrote %s (%zu bytes)\n", output, image.len);

    lege_buffer_free(&image);
    lege_document_close(doc);
    return 0;
}
