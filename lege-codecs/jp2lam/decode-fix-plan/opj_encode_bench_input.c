#include <openjpeg.h>
#include <png.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void msg_cb(const char *msg, void *data) {
    (void)data;
    fputs(msg, stderr);
}

static unsigned char *load_png_rgb(const char *path, unsigned *w, unsigned *h) {
    FILE *fp = fopen(path, "rb");
    if (!fp) return NULL;
    png_structp png = png_create_read_struct(PNG_LIBPNG_VER_STRING, NULL, NULL, NULL);
    png_infop info = png_create_info_struct(png);
    if (!png || !info || setjmp(png_jmpbuf(png))) {
        if (png) png_destroy_read_struct(&png, info ? &info : NULL, NULL);
        fclose(fp);
        return NULL;
    }
    png_init_io(png, fp);
    png_read_info(png, info);
    *w = png_get_image_width(png, info);
    *h = png_get_image_height(png, info);
    int color = png_get_color_type(png, info);
    int depth = png_get_bit_depth(png, info);
    if (depth == 16) png_set_strip_16(png);
    if (color == PNG_COLOR_TYPE_PALETTE) png_set_palette_to_rgb(png);
    if (color == PNG_COLOR_TYPE_GRAY && depth < 8) png_set_expand_gray_1_2_4_to_8(png);
    if (png_get_valid(png, info, PNG_INFO_tRNS)) png_set_tRNS_to_alpha(png);
    if (color == PNG_COLOR_TYPE_GRAY || color == PNG_COLOR_TYPE_GRAY_ALPHA) png_set_gray_to_rgb(png);
    if (color & PNG_COLOR_MASK_ALPHA) png_set_strip_alpha(png);
    png_read_update_info(png, info);
    size_t rowbytes = png_get_rowbytes(png, info);
    unsigned char *tmp = malloc(rowbytes * (*h));
    unsigned char **rows = malloc(sizeof(*rows) * (*h));
    if (!tmp || !rows) { free(tmp); free(rows); png_destroy_read_struct(&png, &info, NULL); fclose(fp); return NULL; }
    for (unsigned y=0; y<*h; ++y) rows[y] = tmp + (size_t)y * rowbytes;
    png_read_image(png, rows);
    png_read_end(png, NULL);
    png_destroy_read_struct(&png, &info, NULL);
    fclose(fp);
    free(rows);
    if (rowbytes == (size_t)(*w)*3) return tmp;
    unsigned char *rgb = malloc((size_t)(*w)*(*h)*3);
    if (!rgb) { free(tmp); return NULL; }
    for (unsigned y=0; y<*h; ++y) memcpy(rgb+(size_t)y*(*w)*3, tmp+(size_t)y*rowbytes, (size_t)(*w)*3);
    free(tmp);
    return rgb;
}

static int encode(const char *in, const char *out, int gray) {
    unsigned w=0,h=0;
    unsigned char *rgb = load_png_rgb(in, &w, &h);
    if (!rgb) { fprintf(stderr,"failed to load %s\n",in); return 1; }
    int ncomp = gray ? 1 : 3;
    opj_image_cmptparm_t cp[3]; memset(cp,0,sizeof(cp));
    for (int c=0;c<ncomp;c++) { cp[c].dx=1; cp[c].dy=1; cp[c].w=w; cp[c].h=h; cp[c].prec=8; cp[c].sgnd=0; }
    opj_image_t *img = opj_image_create((OPJ_UINT32)ncomp, cp, gray?OPJ_CLRSPC_GRAY:OPJ_CLRSPC_SRGB);
    if (!img) { free(rgb); return 1; }
    img->x0=0; img->y0=0; img->x1=w; img->y1=h;
    size_t n=(size_t)w*h;
    for (size_t i=0;i<n;i++) {
        unsigned r=rgb[i*3], g=rgb[i*3+1], b=rgb[i*3+2];
        if (gray) img->comps[0].data[i]=(OPJ_INT32)((77*r + 150*g + 29*b + 128)>>8);
        else { img->comps[0].data[i]=r; img->comps[1].data[i]=g; img->comps[2].data[i]=b; }
    }
    free(rgb);
    opj_cparameters_t p; opj_set_default_encoder_parameters(&p);
    p.cod_format=1;              /* JP2 */
    p.tcp_numlayers=1;
    p.cp_disto_alloc=1;
    p.tcp_rates[0]=20.0f;
    p.irreversible=1;
    p.numresolution=6;
    p.prog_order=OPJ_LRCP;
    opj_codec_t *codec=opj_create_compress(OPJ_CODEC_JP2);
    if (!codec) { opj_image_destroy(img); return 1; }
    opj_set_info_handler(codec,msg_cb,NULL); opj_set_warning_handler(codec,msg_cb,NULL); opj_set_error_handler(codec,msg_cb,NULL);
    opj_codec_set_threads(codec,4);
    if (!opj_setup_encoder(codec,&p,img)) { fprintf(stderr,"setup failed\n"); opj_destroy_codec(codec); opj_image_destroy(img); return 1; }
    opj_stream_t *s=opj_stream_create_default_file_stream(out,OPJ_FALSE);
    if (!s) { opj_destroy_codec(codec); opj_image_destroy(img); return 1; }
    int ok=opj_start_compress(codec,img,s) && opj_encode(codec,s) && opj_end_compress(codec,s);
    opj_stream_destroy(s); opj_destroy_codec(codec); opj_image_destroy(img);
    if (!ok) { fprintf(stderr,"encode failed\n"); return 1; }
    fprintf(stderr,"encoded %s %ux%u comps=%d\n",out,w,h,ncomp);
    return 0;
}

int main(int argc,char **argv) {
    if (argc!=4) { fprintf(stderr,"usage: %s input.png output.jp2 gray|rgb\n",argv[0]); return 2; }
    return encode(argv[1],argv[2],strcmp(argv[3],"gray")==0);
}
