#include <openjpeg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#ifdef _WIN32
#include <windows.h>
static double now_ms(void) {
    static LARGE_INTEGER freq;
    static int init;
    LARGE_INTEGER c;
    if (!init) {
        QueryPerformanceFrequency(&freq);
        init = 1;
    }
    QueryPerformanceCounter(&c);
    return (double)c.QuadPart * 1000.0 / (double)freq.QuadPart;
}
#else
#define _POSIX_C_SOURCE 200809L
#include <time.h>
static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000.0 + t.tv_nsec / 1e6;
}
#endif
static void quiet(const char *m, void *d){ (void)m; (void)d; }
static int cmp_d(const void *a,const void *b){ double x=*(const double*)a,y=*(const double*)b; return (x>y)-(x<y); }
static int clamp8(int v){ return v<0?0:v>255?255:v; }

static int bench_once(const char *file,int threads,int reduce,double *parse,double *decode,double *pack,uint64_t *sum,unsigned *ow,unsigned *oh,unsigned *oc){
    double t0=now_ms();
    opj_dparameters_t p; opj_set_default_decoder_parameters(&p); p.cp_reduce=(OPJ_UINT32)reduce;
    opj_codec_t *codec=opj_create_decompress(OPJ_CODEC_JP2); if(!codec) return 0;
    opj_set_info_handler(codec,quiet,NULL); opj_set_warning_handler(codec,quiet,NULL); opj_set_error_handler(codec,quiet,NULL);
    if(!opj_setup_decoder(codec,&p) || !opj_codec_set_threads(codec,threads)){ opj_destroy_codec(codec); return 0; }
    opj_stream_t *s=opj_stream_create_default_file_stream(file,OPJ_TRUE); if(!s){opj_destroy_codec(codec);return 0;}
    opj_image_t *img=NULL;
    if(!opj_read_header(s,codec,&img)){opj_stream_destroy(s);opj_destroy_codec(codec);return 0;}
    double t1=now_ms();
    if(!opj_decode(codec,s,img) || !opj_end_decompress(codec,s)){opj_image_destroy(img);opj_stream_destroy(s);opj_destroy_codec(codec);return 0;}
    double t2=now_ms();
    unsigned w=img->comps[0].w,h=img->comps[0].h,c=img->numcomps;
    uint64_t checksum=0;
    /* Deliberately generic conversion so decode timing is isolated; this pack loop is not an optimized renderer path. */
    for(unsigned y=0;y<h;y++) for(unsigned x=0;x<w;x++){
        unsigned use=c<3?1:3;
        for(unsigned k=0;k<use;k++){
            opj_image_comp_t *cp=&img->comps[k];
            unsigned sx=(unsigned)(((uint64_t)x*cp->w)/w); if(sx>=cp->w)sx=cp->w-1;
            unsigned sy=(unsigned)(((uint64_t)y*cp->h)/h); if(sy>=cp->h)sy=cp->h-1;
            int v=cp->data[(size_t)sy*cp->w+sx];
            if(cp->sgnd) v += 1 << (cp->prec-1);
            if(cp->prec>8) v=(v + (1<<(cp->prec-9)))>>(cp->prec-8); else if(cp->prec<8) v <<= (8-cp->prec);
            checksum += (uint64_t)clamp8(v);
        }
    }
    double t3=now_ms();
    *parse=t1-t0; *decode=t2-t1; *pack=t3-t2; *sum=checksum; *ow=w;*oh=h;*oc=c;
    opj_image_destroy(img);opj_stream_destroy(s);opj_destroy_codec(codec); return 1;
}

int main(int argc,char **argv){
    if(argc!=5){fprintf(stderr,"usage: %s file.jp2 threads reduce runs\n",argv[0]);return 2;}
    const char *file=argv[1]; int threads=atoi(argv[2]),reduce=atoi(argv[3]),runs=atoi(argv[4]);
    if(threads<1||reduce<0||runs<1||runs>99)return 2;
    double *pa=calloc(runs,sizeof(double)),*de=calloc(runs,sizeof(double)),*pk=calloc(runs,sizeof(double)),*to=calloc(runs,sizeof(double));
    uint64_t sum=0; unsigned w=0,h=0,c=0;
    for(int i=0;i<runs;i++){ uint64_t s=0; unsigned wi=0,hi=0,ci=0; if(!bench_once(file,threads,reduce,&pa[i],&de[i],&pk[i],&s,&wi,&hi,&ci)){fprintf(stderr,"decode failed at run %d\n",i);return 1;} to[i]=pa[i]+de[i]+pk[i]; sum=s;w=wi;h=hi;c=ci; }
    qsort(pa,runs,sizeof(double),cmp_d);qsort(de,runs,sizeof(double),cmp_d);qsort(pk,runs,sizeof(double),cmp_d);qsort(to,runs,sizeof(double),cmp_d);
    int m=runs/2;
    printf("file=%s threads=%d reduce=%d dims=%ux%u comps=%u runs=%d median_ms parse=%.3f decode=%.3f pack=%.3f total=%.3f checksum=%llu\n",file,threads,reduce,w,h,c,runs,pa[m],de[m],pk[m],to[m],(unsigned long long)sum);
    free(pa);free(de);free(pk);free(to);return 0;
}
