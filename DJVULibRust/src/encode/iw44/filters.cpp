static void 
filter_fv(short *p, int w, int h, int rowsize, int scale)
{
  // Disable MMX fast path so scalar path emits tracing
#ifdef MMX
  MMXControl::mmxflag = 0;
#endif
  int y = 0;
  int s = scale*rowsize;
  int s3 = s+s+s;
  h = (h>0) ? ((h-1)/scale)+1 : 0;
  y += 1;
  p += s;
  while (y-3 < h)
    {
      // 1-Delta
      {
        short *q = p;
        short *e = q+w;
        if (y>=3 && y+3<h)
          {
            // Generic case
#ifdef MMX
            if (scale==1 && MMXControl::mmxflag>0)
              mmx_fv_1(q, e, s, s3);
#endif
            while (q<e)
              {
                int a = (int)q[-s] + (int)q[s];
                int b = (int)q[-s3] + (int)q[s3];
                *q -= (((a<<3)+a-b+8)>>4);
                if (y<=s3 && q<p+8*scale)
                  fprintf(stderr, "TRANSFORM_TRACE c44 vertical prediction: y=%d, q=%d, a=%d, b=%d\n",
                      y, (int)(q-p), a, b);
                q += scale;
              }
          }
        else if (y<h)
          {
            // Special cases
            short *q1 = (y+1<h ? q+s : q-s);
            while (q<e)
              {
                int val_qs = (int)q[-s];
                int val_q1 = (int)(*q1);
                int a = val_qs + val_q1;
                *q -= ((a+1)>>1);
                if (y==1 && (q-p)<4)
                  fprintf(stderr, "TRANSFORM_TRACE c44 vertical prediction DETAIL: y=%d, q=%d, q-s=%d, q1=%d, buf[q-s]=%d, buf[q1]=%d, a=%d\n",
                      y, (int)(q-p), (int)(q-s-p), (int)(q1-p), val_qs, val_q1, a);
                q += scale;
                q1 += scale;
              }
          }
      }
      // 2-Update
      {
        short *q = p-s3;
        short *e = q+w;
        if (y>=6 && y<h)
          {
            // Generic case
#ifdef MMX
            if (scale==1 && MMXControl::mmxflag>0)
              mmx_fv_2(q, e, s, s3);
#endif
            while (q<e)
              {
                int a = (int)q[-s] + (int)q[s];
                int b = (int)q[-s3] + (int)q[s3];
                *q += (((a<<3)+a-b+16)>>5);
                if (y<=s3 && q<p+8*scale)
                  fprintf(stderr, "TRANSFORM_TRACE c44 vertical update: y=%d, q=%d, a=%d, b=%d\n",
                      y, (int)(q-p), a, b);
                q += scale;
              }
          }
        else if (y>=3)
          {
            // Special cases
            short *q1 = (y-2<h ? q+s : 0);
            short *q3 = (y<h ? q+s3 : 0);
            if (y>=6)
              {
                while (q<e)
                  {
                    int a = (int)q[-s] + (q1 ? (int)(*q1) : 0);
                    int b = (int)q[-s3] + (q3 ? (int)(*q3) : 0);
                    *q += (((a<<3)+a-b+16)>>5);
                    if (y<=s3 && q<p+8*scale)
                      fprintf(stderr, "TRANSFORM_TRACE c44 vertical update: y=%d, q=%d, a=%d, b=%d\n",
                          y, (int)(q-p), a, b);
                    q += scale;
                    if (q1) q1 += scale;
                    if (q3) q3 += scale;
                  }
              }
            else if (y>=4)
              {
                while (q<e)
                  {
                    int a = (int)q[-s] + (q1 ? (int)(*q1) : 0);
                    int b = (q3 ? (int)(*q3) : 0);
                    *q += (((a<<3)+a-b+16)>>5);
                    q += scale;
                    if (q1) q1 += scale;
                    if (q3) q3 += scale;
                  }
              }
            else
              {
                while (q<e)
                  {
                    int a = (q1 ? (int)(*q1) : 0);
                    int b = (q3 ? (int)(*q3) : 0);
                    *q += (((a<<3)+a-b+16)>>5);
                    q += scale;
                    if (q1) q1 += scale;
                    if (q3) q3 += scale;
                  }
              }
          }
      }
      y += 2;
      p += s+s;
    }
}

static void 
filter_fh(short *p, int w, int h, int rowsize, int scale)
{
  int y = 0;
  int s = scale;
  int s3 = s+s+s;
  rowsize *= scale;
  while (y<h)
    {
      short *q = p+s;
      short *e = p+w;
      int a0=0, a1=0, a2=0, a3=0;
      int b0=0, b1=0, b2=0, b3=0;
      if (q < e)
        {
          // Special case: x=1
          a1 = a2 = a3 = q[-s];
          if (q+s<e)
            a2 = q[s];
          if (q+s3<e)
            a3 = q[s3];
          if (y == 0 && q-p == s) {
            fprintf(stderr, "TRANSFORM_TRACE c44 horizontal a3 source: q+s3=%d, buf[%d]=%d\n", 
                (int)(q+s3-p), (int)(q+s3-p), a3);
          }
          b3 = q[0] - ((a1+a2+1)>>1);
          q[0] = b3;
          if (y == 0 && q-p == s) {
            fprintf(stderr, "TRANSFORM_TRACE c44 horizontal special case: y=%d, q=%d, old=%d, a1=%d, a2=%d, a3=%d, b3=%d\n", 
                y, (int)(q-p), q[0] + ((a1+a2+1)>>1), a1, a2, a3, b3);
          }
          q += s+s;
        }
      while (q+s3 < e)
        {
          // Generic case
          a0=a1; 
          a1=a2; 
          a2=a3;
          a3=q[s3];
          b0=b1; 
          b1=b2; 
          b2=b3;
          int old_val = q[0];
          b3 = q[0] - ((((a1+a2)<<3)+(a1+a2)-a0-a3+8) >> 4);
          q[0] = b3;
          
          // Trace first few prediction operations
          if (q <= p + s + 10) {
            fprintf(stderr, "TRANSFORM_TRACE c44 horizontal prediction: q=%d, old=%d, a0=%d, a1=%d, a2=%d, a3=%d, b3=%d\n", 
                (int)(q-p), old_val, a0, a1, a2, a3, b3);
          }
          q[-s3] = q[-s3] + ((((b1+b2)<<3)+(b1+b2)-b0-b3+16) >> 5);
          q += s+s;
        }
        
        // Debug: check if generic case loop executed
        fprintf(stderr, "TRANSFORM_TRACE c44 horizontal generic loop: q_start=%d, p=%d, s=%d, e=%d, s3=%d\n", 
            (int)((p+s)-p), (int)(p-0), s, (int)((p+w)-p), s3);
      while (q < e)
        {
          // Special case: w-3 <= x < w
          a1=a2; 
          a2=a3;
          b0=b1; 
          b1=b2; 
          b2=b3;
          b3 = q[0] - ((a1+a2+1)>>1);
          q[0] = b3;
          q[-s3] = q[-s3] + ((((b1+b2)<<3)+(b1+b2)-b0-b3+16) >> 5);
          q += s+s;
        }
      while (q-s3 < e)
        {
          // Special case  w <= x < w+3
          b0=b1; 
          b1=b2; 
          b2=b3;
          b3=0;
          if (q-s3 >= p)
            q[-s3] = q[-s3] + ((((b1+b2)<<3)+(b1+b2)-b0-b3+16) >> 5);
          q += s+s;
        }
      y += scale;
      p += rowsize;
    }
}


//////////////////////////////////////////////////////
// WAVELET TRANSFORM 
//////////////////////////////////////////////////////


//----------------------------------------------------
// Function for applying bidimensional IW44 between 
// scale intervals begin(inclusive) and end(exclusive)

void
IW44Image::Transform::Encode::forward(short *p, int w, int h, int rowsize, int begin, int end)
{ 

  // PREPARATION
  filter_begin(w,h);
  // LOOP ON SCALES
  for (int scale=begin; scale<end; scale<<=1)
    {
#ifdef IWTRANSFORM_TIMER
      int tv,th;
      th = tv = GOS::ticks();
#endif
      if (scale == 1) {
        fprintf(stderr, "TRANSFORM_TRACE c44 BEFORE filter_fh scale=%d: buf[0..8]=[%d, %d, %d, %d, %d, %d, %d, %d]\n",
            scale, p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
        fprintf(stderr, "TRANSFORM_TRACE c44 BEFORE filter_fh scale=%d: buf[32..40]=[%d, %d, %d, %d, %d, %d, %d, %d]\n",
            scale, p[32], p[33], p[34], p[35], p[36], p[37], p[38], p[39]);
      }
      filter_fh(p, w, h, rowsize, scale);
      if (scale == 1) {
        fprintf(stderr, "TRANSFORM_TRACE c44 AFTER filter_fh scale=%d: buf[0..8]=[%d, %d, %d, %d, %d, %d, %d, %d]\n",
            scale, p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
        fprintf(stderr, "TRANSFORM_TRACE c44 AFTER filter_fh scale=%d: buf[32..40]=[%d, %d, %d, %d, %d, %d, %d, %d]\n",
            scale, p[32], p[33], p[34], p[35], p[36], p[37], p[38], p[39]);
      }
#ifdef IWTRANSFORM_TIMER
      th = GOS::ticks();
      tv = th - tv;
#endif
      filter_fv(p, w, h, rowsize, scale);
#ifdef IWTRANSFORM_TIMER
      th = GOS::ticks()-th;
      DjVuPrintErrorUTF8("forw%d\tv=%dms h=%dms\n", scale,th,tv);
#endif
    }
  // TERMINATE
  filter_end();
}