<!-- Converted from T-REC-T.88-200002-S!!PDF-E.pdf — 164 pages -->

## Page 1

INTERNATIONAL TELECOMMUNICATION UNION
## ITU-T
TELECOMMUNICATION STANDARDIZATION SECTOR OF ITU
of bi-level images
(Previously CCITT Recommendation)
### T.88
(02/2000)
SERIES T: TERMINALS FOR TELEMATIC SERVICES
Information technology ñ Lossy/lossless coding

---

## Page 3

INTERNATIONAL STANDARD ISO/CEI 14492
ITU-T RECOMMENDATION T.88
# INFORMATION TECHNOLOGY ñ LOSSY/LOSSLESS CODING
# OF BI-LEVEL IMAGES
# Summary
This Recommendation | International Standard, commonly known as JBIG2, defines a coding method for bi-level images (e.g. black and white printed matter). These are images consisting of a single rectangular bit plane, with each pixel taking on one of just two possible colours. This Recommendation | International Standard has been explicitly prepared for a lossy, lossless, and lossy-to-lossless image compression.
# Source
ITU-T Recommendation T.88 was approved on 10 February 2000. An identical text is also published as ISO/IEC 14492.

---

## Page 4

### FOREWORD
ITU (International Telecommunication Union) is the United Nations Specialized Agency in the field of telecommuni-cations. The ITU Telecommunication Standardization Sector (ITU-T) is a permanent organ of the ITU. The ITU-T is responsible for studying technical, operating and tariff questions and issuing Recommendations on them with a view to standardizing telecommunications on a worldwide basis.
The World Telecommunication Standardization Conference (WTSC), which meets every four years, establishes the topics for study by the ITU-T Study Groups which, in their turn, produce Recommendations on these topics.
The approval of Recommendations by the Members of the ITU-T is covered by the procedure laid down in WTSC Resolution No. 1.
In some areas of information technology which fall within ITU-Tís purview, the necessary standards are prepared on a collaborative basis with ISO and IEC.
NOTE
In this Recommendation, the expression "Administration" is used for conciseness to indicate both a telecommunication administration and a recognized operating agency.
INTELLECTUAL PROPERTY RIGHTS
The ITU draws attention to the possibility that the practice or implementation of this Recommendation may involve the use of a claimed Intellectual Property Right. The ITU takes no position concerning the evidence, validity or applicability of claimed Intellectual Property Rights, whether asserted by ITU members or others outside of the Recommendation development process.
As of the date of approval of this Recommendation, the ITU had not received notice of intellectual property, protected by patents, which may be required to implement this Recommendation. However, implementors are cautioned that this may not represent the latest information and are therefore strongly urged to consult the TSB patent database.
ITU 2002
All rights reserved. No part of this publication may be reproduced or utilized in any form or by any means, electronic or mechanical, including photocopying and microfilm, without permission in writing from the ITU.

---

## Page 5

### CONTENTS
0
0.1
0.1.1 0.1.2 0.1.3 0.1.4 0.1.5 0.1.6
0.2.1 0.2.2 0.2.3 0.2.4
0.2
4
4.1 4.2 4.3
5.1 5.2 5.3 5.4
5
5.4.1 5.4.2 5.4.3 5.4.4
5.5 5.6
6.1 6.2
6
6.2.1 6.2.2 6.2.3 6.2.4 6.2.5 6.2.6
6.3.1 6.3.2 6.3.3 6.3.4 6.3.5
6.3
12 12 13 13 13 13 14 14 18 19 19 19 19 20
Decoding Procedures ...................................................................................................................................... Introduction to decoding procedures ................................................................................................. Generic region decoding procedure................................................................................................... General description............................................................................................................ Input parameters ................................................................................................................ Return value ...................................................................................................................... Variables used in decoding................................................................................................ Decoding using a template and arithmetic coding............................................................. Decoding using MMR coding ........................................................................................... Generic Refinement Region Decoding Procedure............................................................................. General description............................................................................................................ Input parameters ................................................................................................................ Return value ...................................................................................................................... Variables used in decoding................................................................................................ Decoding using a template and arithmetic coding.............................................................

---

## Page 6

6.4
6.4.1 6.4.2 6.4.3 6.4.4 6.4.5 6.4.6 6.4.7 6.4.8 6.4.9 6.4.10 6.4.11
6.5.1 6.5.2 6.5.3 6.5.4 6.5.5 6.5.6 6.5.7 6.5.8 6.5.9 6.5.10
6.6.1 6.6.2 6.6.3 6.6.4 6.6.5
6.7.1 6.7.2 6.7.3 6.7.4 6.7.5
6.5
6.6
6.7
7
7.1 7.2
7.2.1 7.2.2 7.2.3 7.2.4 7.2.5 7.2.6 7.2.7 7.2.8
7.3.1 7.3.2
7.4.1 7.4.2 7.4.3 7.4.4 7.4.5 7.4.6 7.4.7 7.4.8
7.3
7.4
44 44 45 45 45 45 45 47 47 47 47 48 49 50 50 50 51 56 66 67 70 72
Control Decoding Procedure .......................................................................................................................... General description ........................................................................................................................... Segment header syntax...................................................................................................................... Segment header fields........................................................................................................ Segment number................................................................................................................ Segment header flags......................................................................................................... Referred-to segment count and retention flags .................................................................. Referred-to segment numbers............................................................................................ Segment page association.................................................................................................. Segment data length .......................................................................................................... Segment header example ................................................................................................... Segment types ................................................................................................................................... Rules for segment references............................................................................................. Rules for page associations................................................................................................ Segment syntaxes .............................................................................................................................. Region segment information field ..................................................................................... Symbol dictionary segment syntax.................................................................................... Text region segment syntax............................................................................................... Pattern dictionary segment syntax..................................................................................... Halftone region segment syntax ........................................................................................ Generic region segment syntax.......................................................................................... Generic refinement region syntax...................................................................................... Page information segment syntax......................................................................................

---

## Page 7

7.4.9 7.4.10 7.4.11 7.4.12 7.4.13 7.4.14 7.4.15
8
8.1 8.2
A.1 A.2 A.3
B.1 B.2
B.2.1 B.2.2 B.2.3
B.3 B.4 B.5
C.1 C.2 C.3 C.4 C.5
D.1 D.2 D.3 D.4
Annex
D.4.1 D.4.2 D.4.3
E.1
E.1.1 E.1.2
E.2.1 E.2.2 E.2.3 E.2.4 E.2.5 E.2.6 E.2.7 E.2.8 E.2.9 E.2.10
E.2
101 101 101 101 102 103 103 103 104 105 105 106 107 107
Annex E ñ Arithmetic Coding................................................................................................................................. Binary encoding ................................................................................................................................ Recursive interval subdivision........................................................................................... Coding conventions and approximations........................................................................... Description of the arithmetic encoder ............................................................................................... Encoder code register conventions .................................................................................... Encoding a decision (ENCODE)....................................................................................... Encoding a 1 or 0 (CODE1 and CODE0).......................................................................... Encoding an MPS or LPS (CODEMPS and CODELPS) .................................................. Probability estimation........................................................................................................ Renormalisation in the encoder (RENORME) .................................................................. Compressed data output (BYTEOUT) .............................................................................. Initialisation of the encoder (INITENC)............................................................................ Termination of encoding (FLUSH) ................................................................................... Minimisation of the compressed data ...............................................................................

---

## Page 8

E.3
E.3.1 E.3.2 E.3.3 E.3.4 E.3.5 E.3.6 E.3.7 E.3.8
H.1 H.2
147
Bibliography ..............................................................................................................................................................

---

## Page 9

### Introduction
This Recommendation | International Standard, informally called JBIG2, defines a coding method for bi-level images (e.g. black and white printed matter). These are images consisting of a single rectangular bit plane, with each pixel taking on one of just two possible colours. Multiple colours are to be handled using an appropriate higher level standard such as ITU-T Recommendation T.44. It is being drafted by the Joint Bi-level Image Experts Group (JBIG), a "Collaborative Team", established in 1988, that reports both to ISO/IEC JTC 1/SC29/WG1 and to ITU-T.
Compression of this type of image is also addressed by existing facsimile standards, for example by the compression algorithms in ITU-T Recommendations T.4 (MH, MR), T.6 (MMR), T.82 (JBIG1), and T.85 (Application profile of JBIG1 for facsimile). Besides the obvious facsimile application, JBIG2 will be useful for document storage and archiving, coding images on the World Wide Web, wireless data transmission, print spooling, and even teleconferencing.
As the result of a process that ended in 1993, JBIG produced a first coding standard formally designated ITU-T Recommendation T.82 | International Standard ISO/IEC 11544, which is informally known as JBIG or JBIG1. JBIG1 is intended to behave as lossless and progressive (lossy-to-lossless) coding. Though it has the capability of lossy coding, the lossy images produced by JBIG1 have significantly lower quality than the original images because the number of pixels in the lossy image cannot exceed one quarter of those in the original image.
On the contrary, JBIG2 was explicitly prepared for lossy, lossless, and lossy-to-lossless image compression. The design goal for JBIG2 was to allow for lossless compression performance better than that of the existing standards, and to allow for lossy compression at much higher compression ratios than the lossless ratios of the existing standards, with almost no visible degradation of quality. In addition, JBIG2 allows both quality-progressive coding, with the progression going from lower to higher (or lossless) quality, and content-progressive coding, successively adding different types of image data (for example, first text, then halftones). A typical JBIG2 encoder decomposes the input bi-level image into several regions and codes each of the regions separately using a different coding method. Such content-based decomposition is very desirable especially in interactive multimedia applications. JBIG2 can also handle a set of images (multiple page document) in an explicit manner.
As is typical with image compression standards, JBIG2 explicitly defines the requirements of a compliant bitstream, and thus defines decoder behaviour. JBIG2 does not explicitly define a standard encoder, but instead is flexible enough to allow sophisticated encoder design. In fact, encoder design will be a major differentiator among competing JBIG2 implementations.
Although this Recommendation | International Standard is phrased in terms of actions to be taken by decoders to interpret a bitstream, any decoder that produces the correct result (as defined by those actions) is compliant, regardless of the actions it actually takes.
Annexes A, B, C, D, E, and F are normative, and thus form an integral part of this Recommendation | International Standard. Annexes G and H are informative, and thus do not form an integral part of this Recommendation | International Standard.
0.1 Interpretation and use of the requirements
This section is informative and designed to aid in interpreting the requirements of this Recommendation | International Standard. The requirements are written to be as general as possible to allow a large amount of implementation flexibility. Hence the language of the requirements is not specific about applications or implementations. In this section a correspondence is drawn between the general wording of the requirements and the intended use of this Recom-mendation | International Standard in typical applications.
0.1.1 Subject matter for JBIG2 coding
JBIG2 is used to code bi-level documents. A bi-level document contains one or more pages. A typical page contains some text data, that is, some characters of a small size arranged in horizontal or vertical rows. The characters in the text part of a page are called symbols in JBIG2. A page may also contain "halftone data", that is, gray-scale or colour multi-level images (e.g. photographs) that have been dithered to produce bi-level images. The periodic bitmap cells in the halftone part of the page are called patterns in JBIG2. In addition, a page may contain other data, such as line art and noise. Such non-text, non-halftone data is called generic data in JBIG2.
The JBIG2 image model treats text data and halftone data as special cases. It is expected that a JBIG2 encoder will divide the content of a page into a text region containing digitised text, a halftone region containing digitised halftones, and a generic region containing the remaining digitised image data, such as line-art. In some circumstances, it is better (in image quality or compressed data size) to consider text or halftones as generic data; conversely, in some circumstances it is better to consider generic data using one of the special cases.

---

## Page 10

may be present, and the page may consist of fewer than three regions.
are recombined to form the final page image.
symbol bitmap may be referred to by an index number.
coding rather than halftone coding.
0.1.2 Relationship between segments and documents
page. The end-of-page segment marks the end of the page.
0.1.3 Structure and use of segments
other information.
the document, or it may be associated with the document as a whole.
described by the earlier segment with the current representation of the page.
segment or in earlier dictionary segments.
segment dependencies without reading the entire file.
embedded organisation
locations within its own structure.
0.1.4 Internal representations
segments. A
earlier, segment. A region segment
An encoder is permitted to divide a single page into any number of regions, but often three regions will be sufficient, one for textual symbols, one for halftone patterns, and the third for the generic remainder. In some cases, not all types of data
The various regions may overlap on the physical page. JBIG2 provides the means to specify how the overlapping regions
A text region consists of a number of symbols placed at specified locations on a background. The symbols usually correspond to individual text characters. JBIG2 obtains much of its effectiveness by using individual symbols more than once. To reuse a symbol, an encoder or decoder must have a succinct way of referring to it. In JBIG2, the symbols are collected into one or more symbol dictionaries. A symbol dictionary is a set of bitmaps of text symbols, indexed so that a
A halftone region consists of a number of patterns placed along a regular grid. The patterns usually correspond to gray-scale values. Indeed, the coding method of the pattern indices is designed as a gray-scale coder. Compression can be realised by representing the binary pixels of one grid cell by a single integer, the halftone index (which is usually a rendered gray-scale value). This many-to-one mapping (the pattern in a cell into a gray-scale value) may have the effect that edge information present in the original bitmap is lost by halftone coding. For this reason, lossless or near-lossless coding of halftones will often be better in image quality (though larger in size) if the halftone is coded with generic
A JBIG2 file contains the information needed to decode a bi-level document. A JBIG2 file is composed of typical page is coded using several segments. In a simple case, there will be a page information segment, a symbol dictionary segment, a text region segment, a pattern dictionary segment, a halftone region segment, and an end-of-page segment. The page information segment provides general information about the page, such as its size and resolution. The dictionary segments collect bitmaps referred to in the region segments. The region segments describe the appearance of the text and halftone regions by referencing bitmaps from a dictionary and specifying where they should appear on the
Each segment contains a segment header, a data header, and data. The segment header is used to convey segment reference information and, in the case of multi-page documents, page association information. A data header gives information used for decoding the data in the segment. The data describes an image region or a dictionary, or provides
Segments are numbered sequentially. A segment may refer to a lower-numbered, or is always associated with one specific page of the document. A dictionary segment may be associated with one page of
A region segment may refer to one or more earlier dictionary segments. The purpose of such a reference is to allow the decoder to identify symbols in a dictionary segment that are present into the image.
A region segment may refer to an earlier region segment. The purpose of such a reference is to combine the image
A dictionary segment may refer to earlier dictionary segments. The symbols added to a dictionary segment may be described directly, or may be described as refinements of symbols described previously, either in the same dictionary
A JBIG2 file may be organised in two ways, sequential or random access. In the sequential organisation, each segment's segment header immediately precedes that segment's data header and data, all in sequential order. In the random access organisation, all the segment headers are collected together at the beginning of the file, followed by the data (including data headers) for all the segments, in the same order. This second organisation permits a decoder to determine all
A third way of encapsulating of JBIG2-encoded data is to embed it in a non-JBIG2 file ñ this is sometimes called the . In this case a different file format carries JBIG2 segments. The segment header, data header, and data of each segment are stored together, but the embedding file format may store the segments in any order, at any set of
Decoded data must be stored before printing or display. While this Recommendation | International Standard does not specify how to store it, its decoding model presumes certain data structures, specifically buffers and dictionaries.

---

## Page 11

Figure 1 illustrates major decoder components and associated buffers. In this figure, decoding procedures are outlined in bold lines, and memory components are outlined in non-bold lines. Also, bold arrows indicate that one decoding procedure invokes another decoding procedure; for example, the symbol dictionary decoding procedure invokes the generic region decoding procedure to decode the bitmaps for the symbols that it defines. Non-bold arrows indicate flow of data: the text region decoding procedure reads symbols from the symbol memory and draws them into the page buffer or an auxiliary buffer. Although it is not shown in Figure 1, the encoded data stream flows to the decoding procedures, and the block labeled "Page and auxiliary buffers" produces the final decoded page images.
Text Symbol region dictionary
| decoding | decoding |
|---|---|
| procedure | procedure |
Generic refinement region decoding procedure
Page and auxiliary buffers
Generic region decoding procedure
| Halftone | Pattern |
|---|---|
| region | dictionary |
| decoding | decoding |
| procedure | procedure |
Figure 1 ñ Block diagram of major decoder components
FIGURE 1/T.88...[D01]
arithmetic coding context memory, to decode most bitstreams.
of a buffer.
Symbol memory
Context memory
Pattern memory
T0828730-99
The resources required to decode any given JBIG2 bitstream depend on the complexity of that bitstream. Some techniques such as striping can be used to reduce decoder memory requirements. It is estimated that a full-featured decoder may need two full-page buffers, plus about the same amount of dictionary memory, plus about 100 kilobytes of
A buffer is a representation of a bitmap. A buffer is intended to hold a large amount of data, typically the size of a page. A buffer may contain the description of a region or of an entire page. Even if the buffer describes only a region, it has information associated with it that specifies its placement on the page. Decoding a region segment modifies the contents

---

## Page 12

There is one special buffer, the page buffer. It is intended that the decoder accumulate region data directly in the page buffer until the page has been completely decoded; then the data can be sent to an output device or file. Decoding an immediate region segment modifies the contents of the page buffer. The usual way of preparing a page is to decode one or more immediate region segments, each one modifying the page buffer. The decoder may output an incomplete page buffer, either as part of progressive transmission or in response to user input. Such output is optional, and its content is not specified by this Recommendation | International Standard.
All other buffers are auxiliary buffers. It is intended that the decoder fill an auxiliary buffer, then later use it to refine the page buffer. In an application, it will often be unnecessary to have any auxiliary buffers. Decoding an intermediate region segment modifies the contents of an auxiliary buffer. The decoder may use auxiliary buffers to output pages other than those found in a complete page buffer, either as part of progressive transmission or in response to user input. Such output is optional, and its content is not specified by this Recommendation | International Standard.
A symbol dictionary consists of an indexed set of bitmaps. The bitmaps in a dictionary are typically small, approximately the size of text characters. Unlike a buffer, a bitmap in a dictionary does not have page location information associated with it.
0.1.5 Decoding results
Decoding a segment involves invocation of one or more decoding procedures. The decoding procedures to be invoked are determined by the segment type.
The result of decoding a region segment is a bitmap stored in a buffer, possibly the page buffer. Decoding a region segment may fill a new buffer, or may modify an existing buffer. In typical applications, placing the data into a buffer involves changing pixels from the background colour to the foreground colour, but this Recommendation | International Standard specifies other permissible ways of changing a buffer's pixels.
A typical page will be described by one or more immediate region segments, each one resulting in modification of the page buffer.
Just as it is possible to specify a new symbol in a dictionary by refining a previously specified symbol, it is also possible to specify a new buffer by refining an existing buffer. However, a region may be refined only by the generic refinement decoding procedure. Such a refinement does not make use of the internal structure of the region in the buffer being refined. After a buffer has been refined, the original buffer is no longer available.
The result of decoding a dictionary segment is a new dictionary. The symbols in the dictionary may later be placed into a buffer by the text region decoding procedure.
0.1.6 Decoding procedures
The generic region decoding procedure fills or modifies a buffer directly, pixel-by-pixel if arithmetic coding is being used, or by runs of foreground and background pixels if MMR and Huffman coding are being used. In the arithmetic coding case, the prediction context contains only pixels determined by data already decoded within the current segment.
The generic refinement region decoding procedure modifies a buffer pixel-by-pixel using arithmetic coding. The prediction context uses pixels determined by data already decoded within the current segment as well as pixels already present either in the page buffer or in an auxiliary buffer.
The text region decoding procedure takes symbols from one or more symbol dictionaries and places them in a buffer. This procedure is invoked during the decoding of a text region segment. The text region segment contains the position and index information for each symbol to be placed in the buffer; the bitmaps of the symbols are taken from the symbol dictionaries.
The symbol dictionary decoding procedure creates a symbol dictionary, that is, an indexed set of symbol bitmaps. A bitmap in the dictionary may be coded directly; it may be coded as a refinement of a symbol already in a dictionary; or it may be coded as an aggregation of two or more symbols already in dictionaries. This decoding procedure is invoked during the decoding of a symbol dictionary segment.
The halftone region decoding procedure takes patterns from a pattern dictionary and places them in a buffer. This procedure is invoked during the decoding of a halftone region segment. The halftone region segment contains the position information for all the patterns to be placed in the buffer, as well as index information for the patterns themselves. The patterns, the fixed-size bitmaps of the halftone, are taken from the halftone dictionaries.
The pattern dictionary decoding procedure creates a dictionary, that is, an indexed set of fixed-size bitmaps (patterns). The bitmaps in the dictionary are coded directly and jointly. This decoding procedure is invoked during the decoding of a pattern dictionary segment.

---

## Page 13

The control decoding procedure decodes segment headers, which include segment type information. The segment type determines which decoding procedure must be invoked to decode the segment. The segment type also determines where the decoded output from the segment will be placed. The segment reference information, also present in the segment header and decoded by the control decoding procedure, determines which other segments must be used to decode the current segment. The control decoding procedure affects everything shown in Figure 1, and so is not shown there as a separate block.
Table 1 summarises the types of data being decoded, which decoding procedure is responsible for decoding them, and what the final representations of the decoded data are.
Table 1 ñ Entities in the decoding process
JBIG2 JBIG2 Physical Concept bitstream entity decoding entity representation
Document JBIG2 file JBIG2 decoder Output medium or device
| Page | Collection of segments | Implicit in control decoding |
|---|---|---|
|  |  |  |
|  |  | procedure |
| Region   | Region segment   | Region decoding procedure   |
|  |  |  |
| Dictionary   |  |  |
|  | Dictionary segment   |  |
|  |  | Dictionary decoding procedure   |
|  |  |  |
|  | Field within a symbol dictionary |  |
|  |  | Symbol dictionary decoding |
| Character   |  |  |
|  | segment |  |
|  |  | procedure |
| Gray-scale |  |  |
|  | Field within a halftone dictionary |  |
|  |  | Pattern dictionary decoding |
|  |  |  |
| value |  |  |
|  | segment |  |
|  |  | procedure |
0.2 Lossy coding
This Recommendation | International Standard does not define how to control lossy coding of bi-level images. Rather it defines how to perform perfect reconstruction of a bitmap that the encoder has chosen to encode. If the encoder chooses to encode a bitmap that is different than the original, the entire process becomes one of lossy coding. The different coding methods allow for different methods of introducing loss in a profitable way.
0.2.1 Symbol coding
Lossy symbol coding provides a natural way of doing lossy coding of text regions. The idea is to allow small differences between the original symbol bitmap and the one indexed in the symbol dictionary. Compression gain is effected by not having to code a large dictionary and, afterwards, by having a cheap symbol index coding as a consequence of the smaller dictionary. It is up to the encoder to decide when two bitmaps are essentially the same or essentially different. This technique was first described in [1].
The hazard of lossy symbol coding is to have substitution errors, that is, to have the encoder replace a bitmap corresponding to one character by a bitmap depicting a different character, so that a human reader misreads the character. The risk of substitution errors can be reduced by using intricate measures of difference between bitmaps and/or by making sure that the critical pixels of the indexed bitmap are correct. One way to control this, described in [5], is to index the possibly wrong symbol and then to apply refinement coding to that symbol bitmap. The idea is to encode the basic character shape at little cost, then correct pixels that the encoder believes alter the meaning of the character.
The process of beneficially introducing loss in textual regions may also take simpler forms such as removing flyspecks from documents or regularizing edges of letters. Most likely such changes will lower the code length of the region without affecting the general appearance of the region ñ possibly even improving the appearance.
NOTE ñ Although the term "text region" is used for regions of the page coded using symbol coding, other possible uses of symbol coding include coding line-art and other non-textual data.
0.2.2 Generic coding
To effect near-lossless coding using generic coding, the encoder applies a preprocess to an original image and encodes the changed image losslessly. The difficulties are to ensure that the changes result in a lower code length and that the quality of the changed image does not suffer badly from the changes. Two possible preprocesses are given in [11]. These

---

## Page 14

preprocesses flip pixels that, when flipped, significantly lower the total code length of the region, but can be flipped without seriously impairing the visual quality. The preprocesses provide for effective near-lossless coding of periodic halftones and for a moderate gain in compression for other data types. The preprocesses are not well-suited for error diffused images and images dithered with blue noise as perceptually lossless compression will not be achieved at a significantly lower rate than the lossless rate.
0.2.3 Halftone coding
Halftone coding is the natural way to obtain very high compression for periodic halftones, such as clustered-dot ordered dithered images. In contrast to lossy generic coding as described above, halftone coding does not intend to preserve the original bitmap, although this is possible in special cases. Loss can also be introduced for additional compression by not putting all the patterns of the original image into the dictionary, thereby reducing both the number of halftone patterns and the number of bits required to specify which pattern is used in which location.
For lossy coding of error diffused images and images dithered with blue noise, it is advisable to use halftone coding with a small grid size. A reconstructed image will lack fine details and may display blockiness but will be clearly recognizable. The blockiness may be reduced on the decoder side in a postprocess; for instance, by using other reconstruction patterns than those that appear in the dictionary. Error diffused images may also be coded losslessly, or with controlled loss as described above, using generic coding.
More details on performing this halftone coding can be found in [12].
0.2.4 Consequences of inadequate segmentation
In order to obtain optimum coding, both in terms of quality and file size, the correct form of encoding should be used for the appropriate regions of the document pages. This subclause briefly describes the consequences of errors in this segmentation.
Using lossy symbol coding for a document containing both text and halftone data will result in poor compression. Depending on the encoder, the quality of the halftone data may be good or bad. Using the form of lossy symbol coding described in [5], the visual quality will probably not suffer.
Using lossy generic coding (using the preprocesses given in [11]) for a document containing both symbol and halftone data usually results in good quality and moderate compression.
Line art and regions of handwritten text may be coded efficiently using generic coding, but depending on the encoder, these types of regions can also be very effectively coded with symbol coding.

---

## Page 15

ISO/IEC 14492:2001 (E)
ITU-T RECOMMENDATION
# INFORMATION TECHNOLOGY ñ LOSSY/LOSSLESS CODING
# OF BI-LEVEL IMAGES
# 1
# Scope
This Recommendation | International Standard defines methods for coding bi-level images and sets of images (documents consisting of multiple pages). It is particularly suitable for bi-level images consisting of text and dithered (halftone) data.
The methods defined permit lossless (bit-preserving) coding, lossy coding, and progressive coding. In progressive coding, the first image is lossy; subsequent images may be lossy or lossless.
This Recommendation | International Standard also defines file formats to enclose the coded bi-level image data.
# 2
# Normative References
ñ ISO/IEC 10646-1:2000, Information technology ñ Universal Multiple-Octet Coded Character Set (UCS) ñ Part 1: Architecture and Basic Multilingual Plane.
# 3
# Terms and Definitions
For the purposes of this Recommendation | International Standard, the following terms and definitions apply.
3.1 adaptive template pixel(s): Special pixel(s), in a template, whose location is not fixed.
3.2 aggregation: Joining or merging of several individual symbols into a new symbol.
3.3 bi-level image: Rectangular array of bits.
3.4 bit: Binary digit, representing the value 0 or 1.
3.5 bitmap: Bi-level image.
3.6 buffer: Storage area used to hold a bitmap.
3.7 byte: Eight bits of data.
3.8 combination operator: Operator used to combine the prior contents of a bitmap with new values being drawn into that bitmap.
3.9 coordinate system: Numbering system for two-dimensional locations where locations are labelled by two numbers, the first one increasing from left to right and the second one increasing from top to bottom.

---

## Page 16

ISO/IEC 14492:2001 (E)
3.10 delta S: Difference in the S coordinates between two successive symbol instances in a non-empty strip.
3.11 delta T: Difference in the T coordinates between two successive non-empty strips.
3.12 decoding procedure: Component of a decoder that decodes a certain type of data.
3.12.1 integer decoding procedure: Decoding procedure whose output on each invocation is a single value.
3.12.2 arithmetic integer decoding procedure: Integer decoding procedure that uses arithmetic entropy decoding.
3.12.3 region decoding procedure: Decoding procedure whose output is a bitmap.
3.12.4 generic region decoding procedure: Region decoding procedure that operates by decoding pixels individually or in runs.
3.12.5 generic refinement region decoding procedure: Region decoding procedure that operates by modifying a reference bitmap to produce an output bitmap.
3.12.6 gray-scale decoding procedure: Decoding procedure whose output is a gray-scale image.
3.12.7 pattern dictionary decoding procedure: Decoding procedure whose output is a list of patterns.
3.12.8 halftone region decoding procedure: Region decoding procedure that operates by drawing a set of patterns into a bitmap, placing the patterns according to a halftone grid.
3.12.9 Huffman table decoding procedure: Decoding procedure whose output is a Huffman table.
3.12.10 text region decoding procedure: Region decoding procedure that operates by drawing a set of symbol instances into a bitmap.
3.12.11 symbol dictionary decoding procedure: Decoding procedure whose output is a list of symbols.
3.13 decoder: Entity capable of decoding a bitstream in conformance with this Recommendation | International Standard.
3.14 dictionary: List of bitmaps.
| 3.14.1 | pattern dictionary: List of patterns. |
|---|---|
| 3.14.2   | symbol dictionary : List of symbols. |
| 3.15   |  |
|  | export flag |
| 3.16   |  |
|  | export list |
| 3.17   |  |
|  | gray-scale image : Rectangular array of non-negative integer indices. |
| 3.18   |  |
|  | gray-scale pixel : Integer-valued element in a gray-scale image. |
| 3.19   |  |
|  | halftone grid |
| 3.20   |  |
|  | height class |
| 3.21   |  |
|  | height class delta height |
| 3.22   |  |
|  | height class delta width |
| 3.23   |  |
|  | Huffman table |
| 3.24   |  |
|  | lossless coding |
| 3.25   |  |
|  | lossy coding |
| the original data. |  |
| 3.26   |  |
| ordinal | : Value used as a counter. |
| 3.27   |  |
|  | out-of-band value |
| 3.28   |  |
| pattern | : Bitmap produced by a pattern dictionary decoding procedure. |
| 3.29   |  |
| pixel | : Element with   0   or   1   as its value in a bitmap. |
| 3.30   |  |
|  | prefix length : Length of the Huffman code prefix in a table line. |
| 3.31   |  |
|  | range length : Number of additional code bits in a table line. |

---

## Page 17

ISO/IEC 14492:2001 (E)
3.32 reference bitmap: Bitmap used as the reference plane during the refinement region decoding procedure.
3.33 referred-to segment: Other segment required in order to decode the current segment.
3.34 region: Bitmap produced by a region decoding procedure.
3.35 segment: Segment header and its segment data.
3.36 strip: Full-width or full-height portion of the coordinate system of a text region.
3.36.1 empty strip: Strip containing the reference corners of no symbol instances.
3.36.2 non-empty strip: Strip containing the reference corner of at least one symbol instance.
3.37 strip size: Extent in pixels of the non-full dimension of a strip.
3.38 symbol: Bitmap produced by a symbol dictionary decoding procedure.
3.39 symbol ID: Integer used to identify a symbol or to index into an array of symbols to retrieve the symbol.
3.40 symbol instance: Symbol drawn, possibly with refinement, at a particular location in a text region.
3.41 symbol instance refinement delta height: Difference in height between a symbol instance's reference bitmap and the bitmap produced by the generic refinement region decoding procedure.
3.42 symbol instance refinement delta width: Difference in width between a symbol instance's reference bitmap and the bitmap produced by the generic refinement region decoding procedure.
3.43 symbol instance refinement delta X: Difference between the X coordinates of the top left corners of a symbol instance's reference bitmap and the bitmap produced by the generic refinement region decoding procedure.
3.44 symbol instance refinement delta Y: Difference between the Y coordinates of the top left corners of a symbol instance's reference bitmap and the bitmap produced by the generic refinement region decoding procedure.
3.45 table line: Specification of the encoding of a single value or a range of values as a Huffman code prefix followed by a fixed number of additional code bits.
3.46 typical prediction: Method of signalling that an entire row of a generic region is identical to the preceding row.
3.47 value: Integer or out-of-band indicator that is decoded.
### 4
### Symbols and Abbreviations
NOTE ñ Due to ISO nomenclature requirements, within the context of clause 4, the term "symbol" is locally used to mean a variable name.
4.1 Abbreviations
OOB Out-of-Band
READ Relative Element Address Designate
NOTE ñ The term "symbol" in the abbreviations LPS and MPS does not refer to the symbols (bitmaps) in this Recommendation | International Standard. The LPS and MPS abbreviations are used despite this because they are the generally-accepted terminology in arithmetic coding.

---

## Page 18

4.2
CONTEXT
CURT
DW
GBATY2
Low-order 16 bits of C
The current S coordinate in a text region decoding procedure
The first code assigned to a particular prefix length in a Huffman table
ISO/IEC 14492:2001 (E)
Symbol definitions
The following symbols used in this Recommendation | International Standard are listed below. A convention is used that parameters to any of the decoding procedures that are used in this Recommendation | International Standard are indicated
The values of the pixels in a template used in the generic or generic refinement decoding
The current symbol instance's T coordinate relative to the current strip's T coordinate in a text
The Y location of adaptive template pixel 2 in a generic region decoding procedure

---

## Page 19

ISO/IEC 14492:2001 (E)
HCHEIGHT The height of the current height class in a symbol dictionary decoding procedure

---

## Page 20

ISO/IEC 14492:2001 (E)
The difference in height between two height classes in a symbol dictionary decoding procedure
HD
HTEMPLATE
HTHIGH
IAAI
IADH
IADS
The combination operator used in a halftone region decoding procedure
The width of the patterns in a pattern dictionary
The number of patterns that may be used in a halftone region decoding procedure
A mask indicating gray-scale values to be skipped
halftone region decoding procedure
An array index
aggregation
height classes
subsequent symbol instances in a strip
The prefix used for many of the variables associated with a pattern dictionary region decoding
Whether unneeded gray-scale values are skipped in a halftone region decoding procedure
The height of the original bitmap of a symbol instance containing refinement information
A parameter indicating the number and arrangement of the pixels in a template used in a
One greater than the largest value that is represented by any normal table line in a Huffman
An arithmetic integer decoding procedure used to decode the number of symbol instances in an
An arithmetic integer decoding procedure used to decode the difference in height between two
An arithmetic integer decoding procedure used to decode the S coordinate of the second and

---

## Page 21

IADT
IADW
IAFS
IARDH
IARDW
IARDX
IARDY
IAIT
LTP
RANGELEN
ISO/IEC 14492:2001 (E)
subsequent symbol instances in a strip
An arithmetic integer decoding procedure used to decode export flags
instance in a strip
refinements
refinements
refinements
An arithmetic integer decoding procedure used to decode the RI bit of symbol instances
The prefix length of the lower range table line in a Huffman table
The next index for an MPS renormalisation
An array of the lengths of the ranges of the table lines in a Huffman table
An arithmetic integer decoding procedure used to decode the T coordinate of the second and
An arithmetic integer decoding procedure used to decode the difference in width between two
An arithmetic integer decoding procedure used to decode the S coordinate of the first symbol
An arithmetic integer decoding procedure used to decode the delta height of symbol instance
An arithmetic integer decoding procedure used to decode the delta width of symbol instance
An arithmetic integer decoding procedure used to decode the delta X values of symbol instance
An arithmetic integer decoding procedure used to decode the delta Y values of symbol instance
An arithmetic integer decoding procedure used to decode the T coordinate of the symbol
Whether the current line is coded explicitly in a generic region decoding procedure or a generic
The number of symbols decoded so far in a symbol dictionary decoding procedure

---

## Page 22

ISO/IEC 14492:2001 (E)
SBW The width of a text region

---

## Page 23

ISO/IEC 14492:2001 (E)
T
| I | TheT coordinate of a symbol instance |
|---|---|
| TOTWIDTH |  |
|  | The total width of the bitmaps in a height class |
| TPGDON |  |
|  | Whether typical prediction is used in a generic region decoding procedure |
| TPGRON |  |
|  | Whether typical prediction is used in a generic region refinement decoding procedure |

---

## Page 24

ISO/IEC 14492:2001 (E)
Y The vertical coordinate of a pixel in a bitmap
4.3 Operator definitions
>> A If V1 and V2 are two integers, then V1 >> A V2 is the value obtained by shifting the value of V1 rightward by V2 bits, filling the leftmost V2 bits of the new value with 0 if V1 is non-negative and 1 if V1 is negative.
### 5
### Conventions
5.1 Typographic conventions
All parameter names are given in bold face.
5.2 Binary notation
The two binary values are denoted as 0 and 1.

---

## Page 25

ISO/IEC 14492:2001 (E)
5.3 Hexadecimal notation
The prefix 0x indicates that the following value is to be interpreted as a hexadecimal number (radix 16).
EXAMPLE ñ The value 0x6A is equal to the decimal value 106.
5.4 Integer value syntax
5.4.1 Bit packing
Bits are packed into bytes starting at the most significant bit. If a decoder is reading a sequence of bits out of a bitstream, it shall first read the most significant bit of the first byte, then the next most significant bit, and so on, then proceed to the next byte.
EXAMPLE ñ The sequence of bytes 0x2F 0x05 0xC1, if interpreted as a sequence of bits, is the sequence
0 0 1 0 1 1 1 1 0 0 0 0 0 1 0 1 1 1 0 0 0 0 0 1
5.4.2 Multi-byte values
All multi-byte values shall be interpreted in a most-significant-first manner: the first byte of each value is the most significant, and the last byte is the least significant.
EXAMPLE ñ The sequence of bytes 0x01 0x5C 0x99 0xFA, if interpreted as a four-byte value, represents the value 0x015C99FA.
5.4.3 Bit numbering
The least significant bit of any value is numbered bit 0. For a one-byte value, the most significant bit is numbered bit 7; for a two-byte value, the most significant bit is numbered bit 15; for a four-byte value, the most significant bit is numbered bit 31.
5.4.4 Signedness
Unless otherwise specified, all multi-bit values shall be treated as unsigned values. When a value is to be treated as a signed number, it shall be interpreted in two's-complement form.
5.5 Array notation and conventions
Arrays are numbered starting from zero.
EXAMPLE ñ A one-dimensional array ARR containing twelve elements has elements:
ARR[0], ARR[1], . . . , ARR[11]
5.6 Image and bitmap conventions
NOTE 1 ñ Throughout this Recommendation | International Standard, pixels in bitmaps are treated as having the values 0 or 1. In most applications of this Recommendation | International Standard, the application will select some interpretation of these two values. A typical interpretation of these pixels is that 0 represents white, or background, and 1 represents black, or foreground. However, this is not a requirement of this Recommendation | International Standard and applications are free to make other interpretations of these values.
The terms "left", "right", "top", "bottom", "width" and "height" are often applied to bitmaps. These terms do not refer to any physical aspect of the bitmap: if a bitmap is printed on paper, it may be printed with its "left" edge along any edge of the paper. They are used within this Recommendation | International Standard to refer to the four edges of the bitmap as shown in Figure 2.
A pixel in a bitmap is referred to by a pair of coordinates X and Y, sometimes written as a pair (X, Y). The location (0, 0) represents the pixel in the top left corner. The X coordinate increases rightwards and the Y coordinate increases downwards.
NOTE 2 ñ These conventions are intended to make it easier to describe operations involving bitmaps, and are not intended to imply any physical characteristics of the image represented by the bitmap.

---

## Page 26

ISO/IEC 14492:2001 (E)
(0, 0) Top edge
Left edge Right edge
Bitmap
T0828740-99/d02
Bottom edge
Figure 2 ñ The four edges of a bitmap
FIGURE 2/T.88...[D02]
### 6
### Decoding Procedures
6.1 Introduction to decoding procedures
This Recommendation | International Standard makes use of a number of different decoding procedures for different types of data. Each of these decoding procedures produces a certain kind of data as output. The generic region decoding procedure, generic refinement region decoding procedure, halftone region decoding procedure, and text region decoding procedures all produce regions as their output. The symbol dictionary decoding procedure produces an array of symbols as its output. The pattern dictionary decoding procedure similarly produces an array of halftone cell bitmaps as its output.
The various region decoding procedures operate in different ways:
• The generic region decoding procedure decodes a bitmap, treating it simply as an array of binary pixels.
• The generic region refinement decoding procedure decodes a bitmap by treating it as an array of binary pixels, but coding each pixel with respect to some reference bitmap.
• The text region decoding procedure decodes a bitmap by drawing a collection of symbols into it, possibly applying the generic refinement region decoding procedure to each one.
• The halftone region decoding procedure decodes a bitmap by placing a collection of patterns into it, at locations specified by a halftone grid.
Each decoding procedure is specified in terms of a number of parameters and a sequence of operations, which are affected by the values of the parameters. These parameters are supplied to the decoding procedure for each invocation, and the same decoding procedure may be invoked multiple times during the course of decoding a bitstream, with different parameters each time.
Some of the decoding procedure parameters are unused in certain circumstances, usually depending on the values of other parameters. In these circumstances, no value needs to be specified for those unused parameters.
In this clause, subsequent clauses, and normative annexes, restrictions are placed on the bitstream being decoded.
EXAMPLE 1 ñ In 7.3, some segment types are described as "Reserved; must not be used."
EXAMPLE 2 ñ In 7.4.2.1.1, if the SDHUFF field is 0 then the SDHUFFDH field must contain the value 0.
NOTE ñ This means that if a decoder encounters a bitstream that does not satisfy the restrictions, it may take any action: it may give up and abort decoding; it may ignore the error and attempt to continue; it may interpret the error and change its behaviour (e.g. use the error to attempt to aid recovery from further errors); and so on.

---

## Page 27

# ISO/IEC 14492:2001 (E)
# 6.2
# Generic region decoding procedure
# 6.2.1
# General description
# This decoding procedure is used to decode a rectangular array of 0 or 1 values, which are coded one pixel at a time (i.e. it
# is used to decode a bitmap using simple, generic, coding). The decoding procedure also modifies an array of probability
# information which may be used by other invocations of this generic region decoding procedure.
# The generic region decoding procedure may be based on sequential coding of the image pixels using arithmetic coding as
# specified in Annex E and a template to determine the coding state. This technique was used in ITU-T Rec. T.82 |
# ISO/IEC 11544 (JBIG). This type of decoding is described in 6.2.5.
# Alternatively, for improved speed but reduced compression the generic region decoding procedure may be based on
# Huffman coding of runs of pixels. This technique was used in the MMR (Modified Modified READ) algorithm
# described in ITU-T Recommendation T.6 (G4). This type of decoding is described in 6.2.6.
# 6.2.2
# Input parameters
# The parameters to this decoding procedure are shown in Table 2.
# Table 2 ñ Parameters for the generic region decoding procedure
Size Name Type Signed? Description and restrictions (bits)
GBH Integer 32 N The height of the region.
GBTEMPLATE Integer 2 N The template identifier.a)
USESKIP Integer 1 N Whether some pixels should be skipped in the decoding.a)
GBATY4 Integer 8 Y The Y location of the adaptive template pixel A4.b)
a) Unused if MMR = 1. b) Unused if MMR = 1 or GBTEMPLATE ≠ 0. c) Unused if USESKIP = 0 or MMR = 1.
# 6.2.3
# Return value
# The variable whose value is the result of this decoding procedure is shown in Table 3.
# Table 3 ñ Return value from the generic region decoding procedure
Size Name Type Signed? Description and restrictions (bits)
GBREG Bitmap The decoded region bitmap.

---

## Page 28

ISO/IEC 14492:2001 (E)
6.2.4 Variables used in decoding
The variables used by this decoding procedure are shown in Table 4.
Table 4 ñ Variables used in the generic region decoding procedure
LTP Integer 1 N Whether the current image line is coded explicitly.a)
a) Unused if MMR = 1.
6.2.5 Decoding using a template and arithmetic coding
6.2.5.1 General description
If MMR is 0 the generic region decoding procedure is based on arithmetic coding with a template to determine the coding state. The remainder of 6.2.5 describes this form of decoding, and only applies when MMR is 0.
6.2.5.2 Coding order and edge conventions
The coding algorithm iterates through the bitmap in raster scan order, that is, by rows from top to bottom, and within each row from left to right. The processing for a current target pixel will reference some pixels in fixed spatial relationship to the target pixel.
Near the edges of the bitmap, these neighbour references might not lie in the actual bitmap. The rule to satisfy out-of-bounds references shall be:
• All pixels lying outside the bounds of the actual bitmap have the value 0.
6.2.5.3 Fixed templates
A template defines a neighbourhood around a pixel to be coded. The values of the pixels in this neighbourhood define a context. Each context has its own adaptive probability estimate used by the arithmetic coder (see Annex E). Although a template is a geometric pattern of pixels, the pixels in a template are said to take on values when the template is aligned with a particular part of the image.
A 2 X X X X X A1
T0828750-99/d03
Figure 3 ñ Template when GBTEMPLATE = 0, showing the AT pixels at their nominal locations
FIGURE 3/T.88...[D03]
T0828760-99/d04
Figure 4 ñ Template when GBTEMPLATE = 1, showing the AT pixel at its nominal location
FIGURE 4/T.88...[D04]

---

## Page 29

ISO/IEC 14492:2001 (E)
Figure 3 shows the template which shall be used when GBTEMPLATE is 0. Figure 4 shows the template which shall be used when GBTEMPLATE is 1. Figure 5 shows the template which shall be used when GBTEMPLATE is 2. Figure 6 shows the template which shall be used when GBTEMPLATE is 3. In each of these figures, the pixel denoted by a circle corresponds to the pixel to be coded and is not part of the template. The pixels denoted by 'X' correspond to ordinary pixels in the template. The pixels denoted A1-A4 are special pixels in the template. They are denoted "adaptive" or AT pixels. These pixels are special in that their locations are not fixed, but can be placed at different locations. See 6.2.5.4 for a description of AT pixels. The legends A1-A4 indicate the AT pixels 1 to 4. The pixels' actual locations are specified as parameters to this decoding procedure; Figures 3 to 6 show the nominal locations of these AT pixels for each template.
The values of the pixels in the template shall be combined to form a context. Each pixel in the template (including the adaptive pixels) shall correspond to a specific bit in the context, although the pixels in the template may be assigned to bits in the context in any order. Because there are up to 16 pixels in the template, contexts can take on up to 65536 different values. This context shall be used to identify which adaptive probability estimate shall be used by the arithmetic coder for encoding the pixel to be coded (see Annex E).
T0828770-99/d05
Figure 5 ñ Template when GBTEMPLATE = 2, showing the AT pixel at its nominal location
FIGURE 5/T.88...[D05]
T0828780-99/d06
Figure 6 ñ Template when GBTEMPLATE = 3, showing the AT pixel at its nominal location
FIGURE 6/T.88...[D06]
NOTE 1 ñ A rule of thumb is to use large templates for large bitmaps. Thus a full-size periodic halftone should be coded with the 16-pixel template and tiny bitmaps such as usual symbol bitmaps should be coded with one of the 10-pixel templates. In some cases an intermediate template is desired, for performance or decoder memory requirements; in this case the 13-pixel template should be used. It is also possible to generate further templates by placing one or more of the AT pixels on top of a regular template pixel, thus fixing its value. NOTE 2 ñ The 10-pixel templates are those used in ITU-T Rec. T.82 | ISO/IEC 11544 (JBIG). Software execution speed is somewhat higher with the two-line template than any of the three-line templates. For most images the 10-pixel, three-line template gives higher compression than the 10-pixel, two-line template.
6.2.5.4 Adaptive template pixels
In coding the image, the template shall be allowed to change in the restricted way described in this clause.
NOTE 1 ñ Some profiles may restrict AT pixel locations to a smaller range than that shown in Figure 7.

---

## Page 30

# ISO/IEC 14492:2001 (E)
NOTE 4 ñ If an AT pixel's location overlaps any regular template pixel's location, then the AT pixel's value can be ignored (since it duplicates another value). This can reduce the memory requirements of the decoder, since not all CX values can occur. However, when TPGD is enabled (TPGDON = 1), the context used to code the SLTP value is used, regardless of whether AT pixels overlap regular template pixels. This means that contexts where the AT pixel's value differs from the regular template pixel's value can still occur, but only for SLTP when TPGD is enabled.
(ñ128, ñ128) . . . (ñ1, ñ128) (0, ñ128) (1, ñ128)
. . . (127, ñ128)
. . . . . . . . .
(ñ128, ñ1) . . . (ñ1, ñ1) (0, ñ1) (1, ñ1)
. . . (127, ñ1)
T0828790-99/d07
(ñ128, 0) . . . (ñ1, 0)
# Figure 7 ñ Field to which AT pixel locations are restricted
# FIGURE 7?t.88...[D07]
# Table 5 ñ The nominal values of the AT pixel locations
GBATX GBATX GBATX
| GBTEMPLATE | 1 | 2 | 3 | GBATX4 |
|---|---|---|---|---|
|  | GBATY |  |  |  |
|  |  | GBATY |  |  |
|  |  |  | GBATY |  |
|  | 1 | 2 | 3 | GBATY 4 |
|  | 3 |  |  |  |
|  |  | ñ3 |  |  |
|  |  |  | 2 |  |
|  |  |  |  | ñ2 |
| 0   |  |  |  |  |
|  | ñ1 |  |  |  |
|  |  | ñ1 |  |  |
|  |  |  | ñ2 |  |
|  |  |  |  | ñ2 |
|  | 3 |  |  |  |
|  |  | NA |  |  |
|  |  |  | NA |  |
|  |  |  |  | NA |
| 1   |  |  |  |  |
|  | ñ1 |  |  |  |
|  |  | NA |  |  |
|  |  |  | NA |  |
|  |  |  |  | NA |
|  | 2 |  |  |  |
|  |  | NA |  |  |
|  |  |  | NA |  |
|  |  |  |  | NA |
| 2   |  |  |  |  |
|  | ñ1 |  |  |  |
|  |  | NA |  |  |
|  |  |  | NA |  |
|  |  |  |  | NA |
|  | 2 |  |  |  |
|  |  | NA |  |  |
|  |  |  | NA |  |
|  |  |  |  | NA |
| 3   |  |  |  |  |
|  | ñ1 |  |  |  |
|  |  | NA |  |  |
|  |  |  | NA |  |
|  |  |  |  | NA |
|  | NOTE ñ NA means that the parameter has no nominal value. |  |  |  |
# 6.2.5.5 Typical prediction for generic direct coding (TPGD)
# Typical prediction for generic direct coding can be enabled or disabled with the TPGDON parameter. If typical
# prediction for generic direct coding is enabled (TPGDON is 1), then before the first pixel of each row is decoded, a
# value indicating that a row is typical shall be decoded. If the row is typical then each pixel of this row is identical to the
# corresponding pixel in the row immediately above, and so no other pixels of this row need to be decoded. If the row is
# not typical, then each pixel of this row needs to be decoded.
NOTE ñ The value decoded before the first pixel of each row is not used in any pixel's template.
# 6.2.5.6 Skipped pixels
# If the parameter USESKIP is 1, then the parameter SKIP contains a GBW-by-GBH bitmap. Each pixel in SKIP
# corresponds to a pixel in the bitmap being decoded; if the pixel in SKIP is 1 then the corresponding pixel in the bitmap
# being decoded is 0 and is not actually decoded.

---

## Page 31

ISO/IEC 14492:2001 (E)
6.2.5.7 Decoding the bitmap
The decoding of the bitmap proceeds as follows: 1) Set:
LTP = 0
• If GBTEMPLATE is 3, use the context shown in Figure 11. Let SLTP be the value of this bit. Set:
LTP = LTP XOR SLTP
• Decode the current pixel by invoking the arithmetic entropy decoding procedure, with CX set to the value formed by concatenating the label "GB" and the 10-16 pixel values gathered in CONTEXT. The result of this invocation is the value of the current pixel. EXAMPLE ñ If GBTEMPLATE is 2, the image pixels overlaid by the template are as shown in Figure 10, and the pixels are gathered in reading order (in rows from top to bottom, and within each row from left to right), then CX is set to "GB0011100101". 4) After all the rows have been decoded, the current contents of the bitmap GBREG are the results that shall be obtained by every decoder, whether it performs this exact sequence of steps or not.
1 0 0 1 1
0 1 1 0 0 1 0
| 0 1 | 0 | 1 |
|---|---|---|
|  |  |  |
Figure 8 ñ Reused context for coding the SLTP value when GBTEMPLATE is 0
FIGURE 8/T.88...[D08]

---

## Page 32

ISO/IEC 14492:2001 (E)
T0828810-99/d09
Figure 9 ñ Reused context for coding the SLTP value when GBTEMPLATE is 1
FIGURE 9/T.88...[D09]
T0828820-99/d10
Figure 10 ñ Reused context for coding the SLTP value when GBTEMPLATE is 2
FIGURE 10/T.88...[D10]
T0828830-99/d11
Figure 11 ñ Reused context for coding the SLTP value when GBTEMPLATE is 3
FIGURE 11/T.88...[D11]
6.2.6 Decoding using MMR coding
NOTE 1 ñ The sources of the byte count, in the cases where it is known, are: ñ Within the pattern dictionary decoding procedure, the byte count is known because all the segment data, beyond the fixed-length data header, is a single MMR-encoded data block, so the MMR data length can be computed from the segment data length. ñ Within the symbol dictionary decoding procedure, the byte count is known from BMSIZE. ñ Within the generic region decoding procedure, the byte count is again known from the segment data length. The reason for allowing EOFB to be optional is that an EOFB is three bytes long, while the byte count is often known beforehand, or can be encoded in fewer than three bytes. Thus, omitting an EOFB reduces the bitmap's coded data size; in symbol dictionaries, where there are often many small bitmaps encoded separately, this savings can be significant.

---

## Page 33

# ISO/IEC 14492:2001 (E)
ñ Invoke the MMR decoding procedure, and never check for EOFB after the bitmap has been decoded, in the cases where EOFB is optional. If the MMR decoding procedure consumes fewer bytes than are known to part of the MMR-compressed data block, this is not an error, but a normal condition indicating that EOFB was present. Skip over such unconsumed bytes.
# • The extension codes of T.6, including uncompressed mode, must not be present in the MMR-encoded data.
NOTE 3 ñ MMR provides less compression than image bitmap compression based on arithmetic coding. Image bitmap decoding using MMR is faster than image bitmap decoding based on arithmetic coding.
# 6.3
# Generic Refinement Region Decoding Procedure
# 6.3.1
# General description
# This decoding procedure is used to decode a rectangular array of 0 or 1 values, which are coded one pixel at a time.
# There is a reference bitmap known to the decoding procedure, and this is used as part of the decoding process. The
# reference bitmap is intended to resemble the bitmap being decoded, and this similarity is used to increase compression.
# Each pixel is decoded using a context comprising pixels drawn from the reference bitmap as well as previously-decoded
# pixels from the bitmap being decoded.
# 6.3.2
# Input parameters
# The parameters to this decoding procedure are shown in Table 6.
# Table 6 ñ Parameters for the generic refinement region decoding procedure
Size Name Type Signed? Description and restrictions (bits)
GRATY2 Integer 8 Y The Y location of the adaptive template pixel RA2.a)
a) Unused if GRTEMPLATE ≠ 0.
# 6.3.3
# Return value
# The variable whose value is the result of this decoding procedure is shown in Table 7.
# Table 7 ñ Return value from the generic refinement region decoding procedure
Size Name Type Signed? Description and restrictions (bits)
| GRREG | Bitmap |
|---|---|
|  |  |

---

## Page 34

# ISO/IEC 14492:2001 (E)
# 6.3.4
# Variables used in decoding
# The variables used by this decoding procedure are shown in Table 8.
# Table 8 ñ Variables used in the generic refinement region decoding procedure
Size Name Type Signed? Description and restrictions (bits)
LTP Integer 1 N Whether the current image line is decoded explicitly.a)
previous line's LTP value.a)
TPGRVAL Integer 1 N Value of the TPGR-predicted current pixel.a)
a) Unused if TPGRON = 0.
# 6.3.5
# Decoding using a template and arithmetic coding
# 6.3.5.1 General description
# The generic refinement region decoding procedure is based on arithmetic coding with a template to determine the coding
# state. The remainder of 6.3.5 describes this form of decoding.
# 6.3.5.2 Coding order and edge conventions
# The coding algorithm iterates through the refined bitmap being decoded, along with a reference bitmap, in raster scan
# order. That is, it iterates by rows from top to bottom, and within each row from left to right. The processing for a current
# target pixel will reference some pixels in fixed spatial relationship to the target pixel. Some of these pixels are drawn
# from the reference version of the bitmap, and some of these pixels are drawn from the already-coded pixels of the refined
# bitmap.
# Near the edges of the bitmap, these neighbour references might not lie in the actual bitmap. The rule to satisfy out-of-
# bounds references shall be:
# • All pixels lying outside the bounds of the actual bitmap or the reference bitmap have the value 0.
# 6.3.5.3 Fixed templates and adaptive templates
# A template defines a neighbourhood around a pixel to be coded. The values of the pixels in this neighbourhood define a
# context. Each context has its own adaptive probability estimate used by the arithmetic coder (see Annex E). Although a
# template is a geometric pattern of pixels, the pixels in a template are said to take on values when the template is aligned
# with a particular part of the image.
RA 1 X X RA2 X X
X X X X
T0828840-99/d12
# Figure 12 ñ 13-pixel refinement template showing
# the AT pixels at their nominal locations
# FIGURE 12/T.88...[D12]
# Figure 12 shows the template which shall be used when GRTEMPLATE is 0. Figure 13 shows the template which shall
# be used when GRTEMPLATE is 1. In each of these figures, the left-hand group indicates the pixels from the already-
# coded pixels of the refined bitmap that are in the template, and the right-hand group indicates the pixels from the
# reference version of the template that are in the template. Each group in each figure includes a pixel denoted by a circle;

---

## Page 35

ISO/IEC 14492:2001 (E)
these pixels all correspond to the pixel to be coded. The pixels marked with an 'X' correspond to ordinary pixels in the template. The pixels denoted RA1-RA2 are special pixels in the template. They are denoted "adaptive" or AT pixels. These pixels are special in that their locations are allowed to change during the process of encoding the image. See 6.3.5.4 for a description of AT pixels. The legends RA1-RA2 indicate the nominal locations of AT pixels 1 to 2.
The AT pixel RA1 can be located anywhere in the field shown in Figure 7, not including the current pixel. The AT pixel RA2 can be located anywhere in the range (ñ128, ñ128) to (127, 127) in the reference bitmap.
The pixels in the left hand group of each template shall be aligned with the already-decoded pixels of the bitmap being decoded, with the pixel denoted by a circle lying on the pixel to be decoded. Let (X, Y) be the location of this pixel. The pixels of the right hand group of each template shall be aligned with the reference bitmap GRREFERENCE, with the pixel denoted by a circle placed at the location (X ñ GRREFERENCEDX, Y ñ GRREFERENCEDY). The values of the pixels in the template shall be combined to form a context. Each pixel in the template (including the adaptive pixels) shall correspond to a specific bit in the context, although the pixels in the template may be assigned to bits in the context in any order. Because there are up to 13 pixels in the template, contexts can take on up to 8192 different values. This context shall be used to identify which adaptive probability estimate shall be used by the arithmetic coder for encoding the pixel to be coded (see Annex E).
T0828850-99/d13
Figure 13 ñ 10-pixel refinement template
FIGURE 13/T.88...[D13]
6.3.5.4 Adaptive template pixels
In coding the image, the template shall be allowed to change in the restricted way described in this clause.
The pixels that are allowed to change shall be called AT pixels. Their nominal locations are indicated by 'RA1' and 'RA2' in Figure 12. Note that only one template has AT pixels.
6.3.5.5 Typical prediction for generic refinement (TPGR)
NOTE ñ The value decoded before the first pixel of each row is not used in any pixel's template.
6.3.5.6 Decoding the refinement bitmap
Let SLTP be the value of this decoded bit. Set:
LTP = LTP XOR SLTP

---

## Page 36

ISO/IEC 14492:2001 (E)
iii) Otherwise, explicitly decode the current pixel using the methodology of steps 3 c) i) through 3 c) iii) above. 4) After all the rows have been decoded, the current contents of the bitmap GRREG are the results that shall be obtained by every decoder, whether it performs this exact sequence of steps or not.
T0828860-99/d14
Figure 14 ñ Reused context for coding the SLTP value when GRTEMPLATE is 0
FIGURE 14/T.88...[D14]
0 0 0 0
| 0 | 0 1 | 0 |
|---|---|---|
|  | 0 0 |  |
|  | T0828870-99/d15 |  |
Figure 15 ñ Reused context for coding the SLTP value when GRTEMPLATE is 1
FIGURE 15/T.88...[D15]

---

## Page 37

# ISO/IEC 14492:2001 (E)
X X X
X X X
T0828880-99/d16
# Figure 16 ñ TPGR template
# FIGURE 16/T.88...[D16]
# 6.4
# Text Region Decoding Procedure
# 6.4.1
# General description
# This decoding procedure is used to decode a bitmap by decoding a number of symbol instances. A symbol instance
# contains a location and a symbol ID, and possibly a refinement bitmap. These symbol instances are combined to form the
# decoded bitmap.
NOTE ñ This decoding procedure will normally be used to decode the text part of a page. The symbols are normally single text characters from some font or alphabet.
# 6.4.2
# Input parameters
# The parameters to this decoding procedure are shown in Table 9.
NOTE ñ The values of some of these parameters in a typical situation, where a bitmap containing text characters in standard English reading order is being decoded, and 1 is the foreground pixel value, are:
# • SBDEFPIXEL is 0
• SBCOMBOP is OR
# • TRANSPOSED is 0
| • | REFCORNER is BOTTOMLEFT |
|---|---|
|  |  |
|  | Size |
|  | Name   |
|  | Type   |
|  |  |
|  |  |
|  | (bits) |
|  | SBHUFF   |
|  | Integer   |
|  | 1 |
|  |  |
|  |  |
|  | SBREFINE   |
|  | Integer   |
|  | 1 |
|  |  |
|  |  |
| SBW |   |
|  | Integer   |
|  | 32 |
|  |  |
|  |  |
| SBH |   |
|  | Integer   |
|  | 32 |
|  |  |
|  |  |
|  | SBNUMINSTANCES   |
|  | Integer   |
|  | 32 |
|  |  |
|  |  |
|  | SBSTRIPS   |
|  | Integer   |
|  | 4 |
|  |  |
|  |  |
|  |  |
|  | SBNUMSYMS   |
|  | Integer   |
|  | 32 |
|  |  |
|  |  |
|  | SBSYMCODES   |
|  | Array of Huffman codes |
|  |  |
|  |  |
SBSYMCODELEN Integer 6 N The length of the symbol codes used in IAID.d)
TRANSPOSED Integer 1 N Whether the strips run vertically.

---

## Page 38

## ISO/IEC 14492:2001 (E)
## Table 9 (concluded)
Size Name Type Signed? Description and restrictions (bits)
first symbol instance in each strip.a)
subsequent symbol instances in each strip. a)
coordinates between non-empty strips.a)
bitmap.b)
bitmap.b)
a refinement coded bitmap.b)
a refinement coded bitmap.b)
instance's refinement bitmap data.b)
SBRATY2 Integer 8 Y The Y location of the adaptive template pixel RA2.c)
a) Unused if SBHUFF = 0. b) Unused if SBHUFF = 0 or SBREFINE = 0. c) Unused if SBREFINE = 0. d) Unused if SBHUFF = 1.
## 6.4.3
## Return value
## The variable whose value is the result of this decoding procedure is shown in Table 10.
## Table 10 ñ Return value from the text region decoding procedure
Size Signed?
| Name | Type | Description and restrictions |
|---|---|---|
|  | (bits) |  |
| SBREG   |  |  |
|  | Bitmap   |  |
|  |  | The decoded region bitmap. |
## 6.4.4
## Variables used in decoding
## The variables used by this decoding procedure are shown in Table 11.

---

## Page 39

## ISO/IEC 14492:2001 (E)
## Table 11 ñ Variables used in the text region decoding procedure
Size Name Type Signed? Description and restrictions (bits)
S Integer 32 Y
| I | A symbol instance's S coordinate. |
|---|---|
| T |  |
|  |  |
|  |  |
|  | Y   |
| I   | A symbol instance's T coordinate. |
| ID |  |
|  |  |
|  |  |
|  | N   |
| I   | A symbol instance's symbol ID. |
| IB   |  |
|  |  |
| I | A symbol instance's symbol bitmap. |
| W   |  |
|  |  |
|  |  |
|  | N   |
HOI Integer 32 N The height of IBOI.a)
a) Unused if SBREFINE = 0.
## 6.4.5
## Decoding the text region
## A symbol-coded bitmap is represented by a set of symbol instances. Each symbol instance encodes a location, a
## symbol ID, and possibly refinement information. The location of each symbol instance comprises an S coordinate and a
## T coordinate. If TRANSPOSED is 0, then the S coordinate axis corresponds to the X axis of the bitmap, and the T axis
## corresponds to the Y axis of the bitmap. If TRANSPOSED is 1, then the S coordinate axis corresponds to the Y axis of
## the bitmap, and the T axis corresponds to the X axis of the bitmap.
NOTE 1 ñ Transposing the coordinate axes allows efficient coding of text running vertically. The reference corner is variable because the most efficient coding is usually obtained when the reference corner of each symbol instance lies on a text baseline, and the text baselines may run in any direction.
## In order to improve compression, symbol instances are grouped into strips according to their TI values. This is done
## according to the value of SBSTRIPS. Symbol instances having TI values between 0 and SBSTRIPS ñ 1 are grouped into
## one strip, symbol instances having TI values between SBSTRIPS and 2 ◊ SBSTRIPS ñ 1 into the next, and so on.
## Within each strip, the symbol instances are coded in the order of increasing S coordinate.
NOTE 2 ñ Normally the strips occur in the order of strictly increasing T coordinates, and the symbol instances within each strip occur in the order of nondecreasing S coordinates. However, it is possible for negative delta S or delta T values to occur during the decoding, meaning that the strips and symbol instances might occur in any order.

---

## Page 40

# ISO/IEC 14492:2001 (E)
# The overall structure of the data to be decoded in order to reconstruct the text region is shown in Figure 17. The format
# of each strip is as shown in Figure 18. When SBREFINE is 0, the format of each symbol instance is as shown in
# Figure 19. When SBREFINE is 1, the format of each symbol instance is as shown in Figure 20.
NOTE 3 ñ There may be some symbol instances whose reference corner lies off the top of the region. If these are to be coded, there must be some way to have a strip that also lies above the top of the region. The initial value of STRIPT is the coordinate with respect to which the first strip is located.
Initial STRIPT value
First strip
Second strip
. . .
Last strip
# Figure 17 ñ Coded structure of a text region
Delta T
First symbol instance
Second symbol instance
. . .
Last symbol instance
OOB
# Figure 18 ñ Structure of a strip
Symbol instance S coordinate
Symbol instance T coordinate
Symbol instance symbol ID
# Figure 19 ñ Structure of a symbol instance when SBREFINE is 0
Symbol instance S coordinate
Symbol instance T coordinate
Symbol instance symbol ID
Symbol instance refinement information
# Figure 20 ñ Structure of a symbol instance when SBREFINE is 1
# The result of decoding a text region shall be the bitmap that is produced by the following steps:
# 1) Fill a bitmap SBREG, of the size given by SBW and SBH, with the SBDEFPIXEL value.
# 2) Decode the initial STRIPT value as described in 6.4.6. Negate the decoded value and assign this negated value to
# the variable STRIPT. Assign the value 0 to FIRSTS. Assign the value 0 to NINSTANCES.

---

## Page 41

ISO/IEC 14492:2001 (E)
b) Decode the strip's delta T value as described in 6.4.6. Let DT be the decoded value. Set:
STRIPT = STRIPT + DT
i) If the current symbol instance is the first symbol instance in the strip, then decode the first symbol instance's S coordinate as described in 6.4.7. Let DFS be the decoded value. Set:
FIRSTS = FIRSTS + DFS CURS = FIRSTS
ii) Otherwise, if the current symbol instance is not the first symbol instance in the strip, decode the symbol instance's S coordinate as described in 6.4.8. If the result of this decoding is OOB then the last symbol instance of the strip has been decoded; proceed to step 3 d). Otherwise, let IDS be the decoded value. Set:
CURS = CURS + IDS + SBDSOFFSET
iii) Decode the symbol instance's T coordinate as described in 6.4.9. Let CURT be the decoded value. Set:
TI = STRIPT + CURT
• If TRANSPOSED is 0, and REFCORNER is TOPRIGHT or BOTTOMRIGHT, set:
CURS = CURS + WI ñ 1
• If TRANSPOSED is 1, and REFCORNER is BOTTOMLEFT or BOTTOMRIGHT, set:
CURS = CURS + HI ñ 1
vii) Set:
SI = CURS
ñ If REFCORNER is BOTTOMRIGHT then the bottom right pixel of the symbol instance bitmap IBI shall be placed at SBREG[SI, TI].

---

## Page 42

ISO/IEC 14492:2001 (E)
x) Update CURS as follows:
• If TRANSPOSED is 0, and REFCORNER is TOPLEFT or BOTTOMLEFT, set:
CURS = CURS + WI ñ 1
• If TRANSPOSED is 1, and REFCORNER is TOPLEFT or TOPRIGHT, set:
CURS = CURS + HI ñ 1
xi) Set:
NINSTANCES = NINSTANCES + 1
4) After all the strips have been decoded, the current contents of SBREG are the results that shall be obtained by every decoder, whether it performs this exact sequence of steps or not.
6.4.6 Strip delta T
If SBHUFF is 1, decode a value using the Huffman table specified by SBHUFFDT and multiply the resulting value by SBSTRIPS.
If SBHUFF is 0, decode a value using the IADT integer arithmetic decoding procedure (see Annex A) and multiply the resulting value by SBSTRIPS.
NOTE ñ The symbol instance S coordinate value for the first symbol instance of each strip is coded differently from the subsequent symbol instances in each strip. This takes advantage of the beginnings of lines being aligned.
If SBHUFF is 1, decode a value using the Huffman table specified by SBHUFFFS.
If SBHUFF is 0, decode a value using the IAFS integer arithmetic decoding procedure (see Annex A).
6.4.8 Subsequent symbol instance S coordinate
If SBHUFF is 1, decode a value using the Huffman table specified by SBHUFFDS.
If SBHUFF is 0, decode a value using the IADS integer arithmetic decoding procedure (see Annex A).
In either case it is possible that the result of this decoding is the out-of-band value OOB.

---

## Page 43

ISO/IEC 14492:2001 (E)
6.4.9 Symbol instance T coordinate
If SBSTRIPS = 1, then the value decoded is always zero. Otherwise:
• If SBHUFF is 1, decode a value by reading log2SBSTRIPS bits directly from the bitstream.
NOTE ñ If SBSTRIPS = 1, then no bits are consumed, and the IAIT integer arithmetic decoding procedure is never invoked.
| 6.4.10 | Symbol instance symbol ID |
|---|---|
| If   SBHUFF   is |   1 |
| SBSYMCODES | . The resulting value, which is |
| If   SBHUFF   | is   0 |
| resulting value. |  |
| 6.4.11   | Symbol instance bitmap |
|  | In some cases, the symbol instance bitmap   |
|  |  |
|  | which of the options is true for a symbol instance is called |
| If   SBREFINE |   is   0 , then set   R I   to   0 . |
| If   SBREFINE |   is   1 , then decode   R I   as follows: |
• If SBHUFF is 1, then read one bit and set RI to the value of that bit.
• If SBHUFF is 0, then decode one bit using the IARI integer arithmetic decoding procedure and set RI to the value of that bit.
If RI is 0 then set the symbol instance bitmap IBI to SBSYMS[IDI].
If RI is 1 then determine the symbol instance bitmap as follows:
1) Decode the symbol instance refinement delta width as described in 6.4.11.1. Let RDWI be the value decoded.
2) Decode the symbol instance refinement delta height as described in 6.4.11.2. Let RDHI be the value decoded.
3) Decode the symbol instance refinement X offset as described in 6.4.11.3. Let RDXI be the value decoded.
4) Decode the symbol instance refinement Y offset as described in 6.4.11.4. Let RDYI be the value decoded.
5) If SBHUFF is 1, then:
a) Decode the symbol instance refinement bitmap data size as described in 6.4.11.5.
b) Skip over any bits remaining in the last byte read.
6) Let IBOI be SBSYMS[IDI]. Let WOI be the width of IBOI and HOI be the height of IBOI. The symbol instance bitmap IBI is the result of applying the generic refinement region decoding procedure described in 6.3. Set the parameters to this decoding procedure as shown in Table 12.
7) If SBHUFF is 1, then skip over any bits remaining in the last byte read. The total number of bytes processed by the generic refinement bitmap decoding procedure must be equal to the value read in step 5 a).
6.4.11.1 Symbol instance refinement delta width
This field, and the following fields, indicate the size, location and contents of the refined symbol bitmap, as the size might not be the same as the size of the bitmap of the symbol whose ID is given in this symbol instance; also, the change in the size of the bitmap might extend to the left and top, not just to the right and bottom, so we need to supply an offset as well as a size. Note that the offsets are given in terms of X and Y, not S and T.
If SBHUFF is 1, decode a value using the Huffman table specified by SBHUFFRDW.
If SBHUFF is 0, decode a value using the IARDW integer arithmetic decoding procedure (see Annex A).

---

## Page 44

ISO/IEC 14492:2001 (E)
Table 12 ñ Parameters used to decode a symbol instance's bitmap using refinement
Name Value
GRW WOI + RDWI GRH HOI + RDHI GRTEMPLATE SBRTEMPLATE GRREFERENCE IBOI GRREFERENCEDX RDWI/2 + RDXI GRREFERENCEDY RDHI/2 + RDYI TPGRON 0 GRATX
| 1 | SBRATX1 |
|---|---|
| GRATY |  |
| 1   | SBRATY 1 |
| GRATX   |  |
| 2 | SBRATX   2 |
| GRATY |  |
| 2   | SBRATY 2 |
| Symbol instance refinement delta height |  |
| , decode a value using the Huffman table specified by |   SBHUFFRDH |
|  | , decode a value using the IARDH integer arithmetic decoding procedure (see Annex A). |
| Symbol instance refinement X offset |  |
| , decode a value using the Huffman table specified by |   SBHUFFRDX |
|  | , decode a value using the IARDX integer arithmetic decoding procedure (see Annex A). |
| Symbol instance refinement Y offset |  |
| , decode a value using the Huffman table specified by |   SBHUFFRDY |
|  | , decode a value using the IARDY integer arithmetic decoding procedure (see Annex A). |
| Symbol instance refinement bitmap data size |  |
|  | SBHUFFRSIZE . |
|  |  |
| Symbol Dictionary Decoding Procedure |  |
|  |  |
|  |  |
|  | This decoding procedure is used to decode a set of symbols; these symbols can then be used by text region decoding |
| procedures, or in some cases by other symbol dictionary decoding procedures. |  |
|  |  |
|  |  |
| The parameters to this decoding procedure are shown in Table 13. |  |
|  | parameter determines how the symbols in this symbol dictionary are coded. If |
| each symbol bitmap is coded via direct bitmap coding. If |   SDREFAGG   is   |
|  | refining or aggregating previously-defined symbol bitmaps. These previously-defined symbol bitmaps may be drawn |
| from other dictionaries and provided as input to this decoding procedure in |   |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
| The variables used by this decoding procedure are shown in Table 15. |  |

---

## Page 45

ISO/IEC 14492:2001 (E)
Table 13 ñ Parameters for the symbol dictionary decoding procedure
Size Name Type Signed? Description and restrictions (bits)
SDNUMINSYMS
| Integer | 32 |
|---|---|
|  | N |
|  |  |
|  |  |
|  |  |
| Array of symbols |   |
|  |  |
|  |  |
|   Integer   32   |  |
|  | N |
|  |  |
|   |  |
| Integer   32   |  |
|  | N |
|  |  |
|  |  |
| Huffman table   |  |
|  |  |
|  |  |
|  |  |
| Huffman table   |  |
|  |  |
|  |  |
|   |  |
| Huffman table   |  |
|  |  |
|  |  |
|   Huffman table   |  |
|  |  |
|  |  |
SDRATY2 Integer 8 Y The Y location of the adaptive template pixel RA2.d)
a) Unused if SDHUFF = 0. b) Unused if SDHUFF = 0 or SDREFAGG = 0. c) Unused if SDHUFF = 1. d) Unused if SDREFAGG = 0.
Table 14 ñ Return value from the symbol dictionary decoding procedure
Size Name Type Signed? Description and restrictions (bits)
SDEXSYMS Array of symbols The symbols exported by this symbol dictionary. Contains SDNUMEXSYMS symbols.

---

## Page 46

## ISO/IEC 14492:2001 (E)
## Table 15 ñ Variables used in the symbol dictionary decoding procedure.
Size Name Type Signed? Description and restrictions (bits)
SDNEWSYMS Array of symbols The symbols defined in this symbol dictionary. Contains SDNUMNEWSYMS symbols.
SDNEWSYMWIDTHS Array of integers The widths of the symbols in SDNEWSYMS. Contains SDNUMNEWSYMS integers. Each integer is a 32-bit unsigned value.
HCHEIGHT Integer 32 N Height of the current height class.
NSYMSDECODED Integer 32 N How many symbols have been decoded so far.
HCDH Integer 32 Y The difference in height between two height classes.
SYMWIDTH Integer 32 N The width of the current symbol.
TOTWIDTH Integer 32 N The width of the current height class.
HCFIRSTSYM Integer 32 N The index of the first symbol in the current height class.
DW Integer 32 Y The difference in width between two symbols.
B Bitmap
| S | The current symbol's bitmap. |
|---|---|
| B   |  |
|  |  |
HC The current height class collective bitmap.
I Integer 32 N An array index.
J Integer 32 N An array index.
REFAGGNINST Integer 32 N The number of symbol instances in an aggregation.
Contains SDNUMINSYMS + SDNUMNEWSYMS values. Each value is one bit.
EXINDEX Integer 32 N An array index.
CUREXFLAG Integer 1 N The current export flag.
EXRUNLENGTH Integer 32 N The length of a run of identical export flag values.
## 6.5.5
## Decoding the symbol dictionary
## The internal structure of a symbol dictionary is shown in Figure 21. The symbols defined in the dictionary are ordered
## into height classes: a height class contains a number of symbols whose bitmaps are the same height.
NOTE 1 ñ In most cases, the height classes occur in the order of strictly increasing height, shortest through tallest. If SDREFAGG is 1, though, a symbol may be coded as a refinement of a larger symbol defined in the same dictionary. In this case, the height class for that base symbol must be decoded (and therefore must occur) before the shorter height class of the symbol that is coded by refining it. For this reason, height class delta heights (and symbol delta widths) may be zero or negative, as well as positive.
List of exported symbols
## Figure 21 ñ The structure of a symbol dictionary

---

## Page 47

ISO/IEC 14492:2001 (E)
If SDHUFF is 1 and SDREFAGG is 0 then the format of a height class is as shown in Figure 22. Otherwise, the format of a height class is as shown in Figure 23. The fields mentioned in those figures are described fully below.
Height class delta height Delta width for first symbol Delta width for second symbol . . . OOB Height class collective bitmap
Figure 22 ñ Height class coding when SDHUFF is 1 and SDREFAGG is 0
Height class delta height Delta width for first symbol Bitmap for first symbol Delta width for second symbol Bitmap for second symbol . . . OOB
Figure 23 ñ Height class coding when SDHUFF is 0 or SDREFAGG is 1
HCHEIGHT = 0 NSYMSDECODED = 0
HCHEIGHT = HCEIGHT + HCDH SYMWIDTH = 0 TOTWIDTH = 0 HCFIRSTSYM = NSYMSDECODED
SYMWIDTH = SYMWIDTH + DW TOTWIDTH = TOTWIDTH + SYMWIDTH
ii) If SDHUFF is 0 or SDREFAGG is 1, then decode the symbol's bitmap as described in 6.5.8. Let BS be the decoded bitmap (this bitmap has width SYMWIDTH and height HCHEIGHT). Set:
SDNEWSYMS[NSYMSDECODED] = BS

---

## Page 48

ISO/IEC 14492:2001 (E)
iii) If SDHUFF is 1 and SDREFAGG is 0, then set:
SDNEWSYMWIDTHS[NSYMSDECODED] = SYMWIDTH
iv) Set:
NSYMSDECODED = NSYMSDECODED + 1
BHC contains the NSYMSDECODED ñ HCFIRSTSYM symbols concatenated left-to-right, with no intervening gaps. For each I between HCFIRSTSYM and NSYMSDECODED ñ 1:
• the width of SDNEWSYMS[I] is the value of SDNEWSYMWIDTHS[I];
• the height of SDNEWSYMS[I] is HCHEIGHT; and
• the bitmap SDNEWSYMS[I] can be obtained by extracting the columns of BHC from:
J=HCFIRSTSYM
through
J=HCFIRSTSYM
NOTE 2 ñ Not all the new symbols need to be exported; this allows the dictionary to define a symbol, use it via refinement/aggregate coding to build other symbols, and not actually export the original symbol. Also, since input symbols can be exported, this dictionary can, in effect, copy symbols from other dictionaries.
6.5.6 Height class delta height
If SDHUFF is 1, decode a value using the Huffman table specified by SDHUFFDH.
If SDHUFF is 0, decode a value using the IADH integer arithmetic decoding procedure (see Annex A).
6.5.7 Delta width
If SDHUFF is 1, decode a value using the Huffman table specified by SDHUFFDW.
If SDHUFF is 0, decode a value using the IADW integer arithmetic decoding procedure (see Annex A).
In either case it is possible that the result of this decoding is the out-of-band value OOB.
6.5.8 Symbol bitmap
This field is only present if SDHUFF = 0 or SDREFAGG = 1. This field takes one of two forms; SDREFAGG determines which form is used.
6.5.8.1 Direct-coded symbol bitmap
If SDREFAGG is 0, then decode the symbol's bitmap using a generic region decoding procedure as described in 6.2. Set the parameters to this decoding procedure as shown in Table 16.

---

## Page 49

ISO/IEC 14492:2001 (E)
Table 16 ñ Parameters used to decode a symbol's bitmap using generic bitmap decoding
MMR 0 GBW SYMWIDTH GBH HCHEIGHT GBTEMPLATE SDTEMPLATE TPGDON 0 USESKIP 0 GBATX
| 1 | SDATX1 |
|---|---|
| GBATY |  |
| 1   | SDATY 1 |
| GBATX   |  |
| 2 | SDATX   2 |
| GBATY |  |
| 2   | SDATY 2 |
| GBATX   |  |
| 3 | SDATX   3 |
| GBATY |  |
| 3   | SDATY 3 |
| GBATX   |  |
| 4 | SDATX   4 |
| GBATY |  |
| 4   | SDATY 4 |
| Refinement/aggregate-coded symbol bitmap |  |
|  | , then the symbol's bitmap is coded by refinement and aggregation of other, previously-defined, |
|  |  |
|  | Decode the number of symbol instances contained in the aggregation, as specified in 6.5.8.2.1. Let REFAGGNINST |
|  |  |
|  | If REFAGGNINST is greater than one, then decode the bitmap itself using a text region decoding procedure as |
|  |  |
|  |  |
| Number of symbol instances in aggregation |  |
|  |   |
|  | , decode a value using the IAAI integer arithmetic decoding procedure (see Annex A). |
|   |   =   1 |
|  | If a symbol's bitmap is coded by refinement/aggregate coding, and there is only one symbol in the aggregation, then the |
|  | bitmap is decoded as follows. This is essentially the procedure followed by the symbol region decoding procedure, |
|  |  |
| . |  |
| Decode a symbol ID as described in 6.4.10, using the values of |   |
|   ID I   |  |
| refinement   X   | described   in 6.4.11.3.   |
| RDX I   be the value decoded. |  |
| refinement   Y   | described   in 6.4.11.4.   |
| RDY   I be the value decoded. |  |
|  |  |
|  | Decode the symbol instance refinement bitmap data size as described in 6.4.11.5, using Table B.1 for |
| . |  |
| Skip over any bits remaining in the last byte read. |  |
| [ ID   I ], where   |  |
|  | the generic refinement region decoding procedure described in 6.3. Set the parameters to this decoding procedure as |
|  |  |
|  | , then skip over any bits remaining in the last byte read. The total number of bytes processed by the |
|  |  |

---

## Page 50

ISO/IEC 14492:2001 (E)
Table 17 ñ Parameters used to decode a symbol's bitmap using refinement/aggregate decoding
Name Value
SBHUFF SDHUFF SBREFINE 1 SBW SYMWIDTH SBH HCHEIGHT SBNUMINSTANCES REFAGGNINST SBSTRIPS 1 SBNUMSYMS SDNUMINSYMS + NSYMSDECODED
| SBSYMCODES | See 6.5.8.2.3.a) |
|---|---|
| SBSYMCODELEN   | See 6.5.8.2.3. b) |
| SBSYMS   |  |
|  | See 6.5.8.2.4. |
| SBDEFPIXEL   |  |
|  | 0 |
| SBCOMBOP   |  |
|  | OR |
| TRANSPOSED   |  |
|  | 0 |
| REFCORNER   |  |
|  | TOPLEFT |
| SBDSOFFSET   |  |
|  | 0 |
| SBHUFFFS   | Table B.6 a) |
| SBHUFFDS   | Table B.8 a) |
| SBHUFFDT   | Table B.11 a) |
| SBHUFFRDW   | Table B.15 a) |
| SBHUFFRDH   | Table B.15 a) |
| SBHUFFRDX   | Table B.15 a) |
| SBHUFFRDY   | Table B.15 a) |
| SBHUFFRSIZE   | Table B.1 a) |
| SBRTEMPLATE   |  |
|  | SDRTEMPLATE |
| SBRATX   |  |
| 1 | SDRATX   1 |
| SBRATY |  |
| 1   | SDRATY 1 |
| SBRATX   |  |
| 2 | SDRATX   2 |
| SBRATY |  |
| 2   | SDRATY 2 |
| a)   If   SDHUFF   =   0   | then this parameter has no value. |
| b)   If   SDHUFF   =   1   | then this parameter has no value. |
Table 18 ñ Parameters used to decode a symbol's bitmap when REFAGGNINST = 1
Name Value
GRW SYMWIDTH GRH HCHEIGHT GRTEMPLATE SDRTEMPLATE GRREFERENCE IBOI GRREFERENCEDX RDXI GRREFERENCEDY RDYI TPGRON 0 GRATX
| 1 | SDRATX1 |
|---|---|
| GRATY |  |
| 1   | SDRATY 1 |
| GRATX   |  |
| 2 | SDRATX   2 |
| GRATY |  |
| 2   | SDRATY 2 |

---

## Page 51

ISO/IEC 14492:2001 (E)
6.5.8.2.3 Setting SBSYMCODES and SBSYMCODELEN
When SDHUFF = 1, set SBSYMCODES to an array of SBNUMSYMS codes, where the length of each code is:
max( log2 (SDNUMINSYMS + SDNUMNEWSYMS) , 1)
and the code SBSYMCODES[I] is I (for I between 0 and SBNUMSYMS ñ 1).
NOTE ñ This sets the codes as equal-length codes, assigned starting from zero. The code lengths are computed from the maximum number of symbols available in this symbol dictionary: all the imported symbols and all the symbols defined here. There is some wastage in choosing this code length and assigning these codes. However, doing it this way means that neither the code lengths nor the actual codes assigned to each symbol changes during the process of decoding this symbol dictionary.
Similarly, when SDHUFF is 0, SBSYMCODELEN should be set to:
log2(SDNUMINSYMS + SDNUMNEWSYMS)
so that the length of the bit strings decoded using IAID will not change during the decoding of this symbol dictionary.
6.5.8.2.4 Setting SBSYMS
Set SBSYMS to an array of SDNUMINSYMS + NSYMSDECODED symbols, formed by concatenating the array SDINSYMS and the first NSYMSDECODED entries of the array SDNEWSYMS.
6.5.9 Height class collective bitmap
This field is only present if SDHUFF = 1 and SDREFAGG = 0.
This field contains the bitmaps of all the symbols in the height class, concatenated left to right, and MMR encoded. It is preceded by a count of its size in bytes.
3) If BMSIZE is zero, then the bitmap is stored uncompressed, and the actual size in bytes is:
TOTWIDTH HCHEIGHT × 8
Decode the bitmap by reading this many bytes and treating it as HCHEIGHT rows of TOTWIDTH pixels, each row padded out to a byte boundary with 0-7 0 bits.
4) Otherwise, decode the bitmap using a generic bitmap decoding procedure as described in 6.2. Set the parameters to this decoding procedure as shown in Table 19.
Table 19 ñ Parameters used to decode a height class collective bitmap
Name Value
MMR 1 GBW TOTWIDTH GBH HCHEIGHT
NOTE ñ BMSIZE is used to determine the number of bytes of MMR-encoded data, and thus allow the encoder to omit EOFB (see 6.2.6); this usually results in a reduction in encoded data size. It also allows the encoder to transmit the bitmap uncompressed in the cases where MMR coding would result in an expansion.
| 6.5.10 | Exported symbols |
|---|---|
|  |  |
|  | plus any of the symbols defined in the dictionary. |

---

## Page 52

ISO/IEC 14492:2001 (E)
The array of symbols exported from the dictionary is produced by decoding a bit for each of those symbols. These bits form an array EXFLAGS of SDNUMINSYMS + SDNUMNEWSYMS binary values, each one corresponding to an input symbol or a newly-defined symbol. A 1 bit for a symbol indicates that the symbol is exported. Exactly SDNUMEXSYMS symbols must be exported from the dictionary. The order of exported symbols is the order produced by concatenating the array SDINSYMS and the array SDNEWSYMS.
1) Set:
EXINDEX = 0 CUREXFLAG = 0
4) Set:
EXINDEX = EXINDEX + EXRUNLENGTH CUREXFLAG = NOT(CUREXFLAG)
a) If I < SDNUMINSYMS then set:
J] SDEXSYMS[ = SDINSYMS[I] J = J + 1
b) If I ≥ SDNUMINSYMS then set:
J] SDEXSYMS[ = SDNEWSYMS[I ñ SDNUMINSYMS] J = J + 1
NOTE ñ Most dictionaries will export exactly the new symbols that they define; they will not export any of the symbols in SDINSYMS. In this case, the first SDNUMINSYMS values in EXFLAGS are 0, and the remaining SDNUMNEWSYMS values are 1.
6.6 Halftone Region Decoding Procedure
6.6.1 General description
NOTE ñ This form of coding is suitable to efficiently transmitting a bitmap containing periodic halftone image data, such as clustered-dot ordered dithered data. Other forms of halftone image data, such as error-diffused data, may be converted into this form via descreening, or may be coded in a form more closely resembling the original using generic bitmap coding.
6.6.2 Input parameters
The parameters to this decoding procedure are shown in Table 20.

---

## Page 53

# ISO/IEC 14492:2001 (E)
# Table 20 ñ Parameters for the halftone region decoding procedure
Size Name Type Signed? Description and restrictions (bits)
HMMR Integer 1 N Whether MMR coding is used.
| HTEMPLATE | Integer | 2 | N | The template identifier.a) |
|---|---|---|---|---|
| HNUMPATS   |  |  |  |  |
|  | Integer   |  |  |  |
|  | 32 |   |  |  |
|  |  |  | N   |  |
|  |  |  |  | The number of patterns that may be used in this region. |
| HPATS   |  |  |  |  |
|  | Array of patterns |   |  |  |
|  |  |  |  | An array containing the patterns used in this region. Contains |
|  |  |  |  | HNUMPATS   patterns. |
| HDEFPIXEL   |  |  |  |  |
|  | Integer   |  |  |  |
|  | 1 |   |  |  |
|  |  |  | N   |  |
|  |  |  |  | The default pixel for this bitmap. |
| HCOMBOP   |  |  |  |  |
|  | Operator   |  |  |  |
|  |  |  |  | The combination operator used in this halftone region. May take |
|  |  |  |  | on the values OR, AND, XOR, XNOR and REPLACE. |
HENABLESKIP Integer 1 N Whether unneeded gray-scale values are skipped.a)
a) Unused if HMMR = 1
# 6.6.3
# Return value
# The variable whose value is the result of this decoding procedure is shown in Table 21.
# Table 21 ñ Return value from the halftone region decoding procedure
Size Name Type Signed? Description and restrictions (bits)
HTREG Bitmap The decoded region bitmap
# 6.6.4
# Variables used in decoding
# The variables used by this decoding procedure are shown in Table 22.
# 6.6.5
# Decoding the halftone region
# A halftone-coded bitmap is represented by a set of pattern instances. Each instance encodes a pattern. The location of
# each pattern is not coded explicitly but given by a grid global to the entire halftone bitmap. The halftone grid origin is
# specified by parameters HGX and HGY. The grid period is defined by parameters HRX and HRY (see Figure 24).
# HGX, HGY, HRX and HRY are scaled by 256, which means that the grid origin and grid period have a fractional part
# of 8 bits.
NOTE 1 ñ Note that HRX and HRY are unsigned values; that is, their values are always greater than or equal to zero. This means that the grid vector is restricted to lie in a single quadrant. Despite this restriction, any halftone grid can be encoded by a suitable adjustment of HGX and HGY: HGX and HGY must be set so that the grid's origin is the leftmost corner. This is the top left corner in the case where the grid is axis-aligned, or is a slight counter-clockwise rotation of an axis-aligned grid (as shown in Figure 24) and is the bottom left corner in the case where the grid is a slight clockwise rotation of an axis aligned grid.
# The possible patterns are given in a dictionary. The identity of a pattern is specified by an index which will usually
# represent the gray-scale value of the pattern.

---

## Page 54

### ISO/IEC 14492:2001 (E)
### Table 22 ñ Variables used in the halftone region decoding procedure
Size Signed?
| Name | Type | Description and restrictions |
|---|---|---|
|  |  |  |
|  |  |  |
|  | Integer   |  |
|  |  |  |
|  |  |  |
HSKIP Bitmap Skip mask. HSKIP is HGW by HGH pixels.a)
a) Unused if HENABLESKIP = 0.
NOTE 2 ñ We use the term gray-scale value for the index to illustrate the compression idea. There is no requirement in this Recommendation | International Standard that the index does indeed correspond to the gray-scale value.
### The result of decoding a halftone bitmap is the bitmap that is produced by the following steps:
### 1) Fill a bitmap HTREG, of the size given by HBW and HBH, with the HDEFPIXEL value.
### 2) If HENABLESKIP equals 1, compute a bitmap HSKIP as shown in 6.6.5.1.
### 3) Set HBPP to log2 (HNUMPATS) .
### 4) Decode an image GI of size HGW by HGH with HBPP bits per pixel using the gray-scale image decoding
### procedure as described in Annex C. Set the parameters to this decoding procedure as shown in Table 23.
### Let GI be the results of invoking this decoding procedure.
### 5) Place sequentially the patterns corresponding to the values in GI into HTREG by the procedure described in 6.6.5.2.
### The rendering procedure is illustrated in Figure 24. The outline of two patterns are marked by dotted boxes.
### 6) After all the patterns have been placed on the bitmap, the current contents of the halftone-coded bitmap are the
### results that shall be obtained by every decoder, whether it performs this exact sequence of steps or not.
NOTE 3 ñ If HGX is 0, HGY is 0, HRX is equal to HPW ◊ 256 and HRY is 0, then the grid is simple: it is axis-aligned, the primary direction is horizontal, and the grid step is equal to the size of the patterns. In this case, it is possible to optimise the drawing process, as none of the patterns can overlap.
### 6.6.5.1 Computing HSKIP
### The bitmap HSKIP contains 1 at a pixel if drawing a pattern at the corresponding location on the halftone grid does not
### affect any pixels of HTREG. It is computed as follows:
### 1) For each value of mg between 0 and HGH ñ 1, beginning from 0, perform the following steps:
### a) For each value of ng between 0 and HGW ñ 1, beginning from 0, perform the following steps:
| i) | Set: |
|---|---|
|  |  |
|  |  |
| ii)   If (( | x   +   HPW |
|  |  |
| Otherwise, set: |  |
|  |  |

---

## Page 55

## ISO/IEC 14492:2001 (E)
Bounding box for page
ng
Halftone grid
( HGX + HRX , HGY ñ HRY ) 256 256 Bounding box for halftone region
### (0, 0)
( HGX , HGY ) 256 256
x
HGX + HRY
| ( | , HGY + HRX ) |
|---|---|
| 256 |  |
|  | 256 |
y
mg
T0828890-99/d17
## Figure 24 ñ Specification of coordinate systems and grid parameters
## Table 23 ñ Parameters used to decode a halftone region's gray-scale value array
### Name
### Value
### GSMMR
### HMMR
### GSW
### HGW
### GSH
### HGH
### GSBPP
### HBPP
### GSUSESKIP
### HENABLESKIP
| GSKIP | HSKIP a) |
|---|---|
| GSTEMPLATE   | HTEMPLATE   b) |
| a)   If   HENABLESKIP | then this parameter has no value. |
| b)   If   HMMR   =   1   | then this parameter has no value. |

---

## Page 56

### ISO/IEC 14492:2001 (E)
### 6.6.5.2 Rendering the patterns
### Draw the patterns into HTREG using the following procedure:
### 1) For each value of mg between 0 and HGH ñ 1, beginning from 0, perform the following steps.
### a) For each value of ng between 0 and HGW ñ 1, beginning from 0, perform the following steps.
| i) | Set: |
|---|---|
|  |  |
|  |  |
| ii)   | Draw the pattern |
|  | HTREG. |
|  |  |
|  |  |
| by   | HCOMBOP |
| bitmap. |  |
|  |  |
|  |  |
| bitmap. |  |
|  |  |
|  |  |
|  | Recommendation | International Standard. |
|  |  |
|  |  |
|  |  |
|   |  |
|  |  |
| 6.7.1   |  |
|  | General description |
|  |  |
| decoding procedures. |  |
| 6.7.2   |  |
|  | Input parameters |
|  |  |
|  |  |
|  |  |
| Name |   |
|  |  |
|  |  |
|  |  |
|  |  |
| HDMMR   |  |
|  |  |
|  |  |
|  |  |
|  |  |
| HDPW   |  |
|  |  |
|  |  |
|  |  |
|  |  |
| HDPH   |  |
|  |  |
|  |  |
|  |  |
|  |  |
| GRAYMAX   |  |
|  |  |
|  |  |
|  |  |
|  |  |
HDTEMPLATE Integer 2 N The template used to code the patterns.a)
a) Unused if HDMMR = 1.
### 6.7.3
### Return value
### The variable whose value is the result of this decoding procedure is shown in Table 25.
### Table 25 ñ Return value from the pattern dictionary decoding procedure
Size Name Type Signed? Description and restrictions (bits)
HDPATS Array of patterns The patterns exported by this pattern dictionary. Contains GRAYMAX + 1 patterns.

---

## Page 57

# ISO/IEC 14492:2001 (E)
# 6.7.4
# Variables used in decoding
# The variables used by this decoding procedure are shown in Table 26.
# Table 26 ñ Variables used in the pattern dictionary decoding procedure
Size Name Type Signed? Description and restrictions (bits)
B Bitmap
| HDC | The dictionary collective bitmap. |
|---|---|
| B   |  |
|  |  |
P A bitmap of size HDPW by HDPH.
# 6.7.5
# Decoding the pattern dictionary
# The result of decoding a pattern dictionary is a set of patterns: HDPATS[0] ∑ ∑ ∑ HDPATS[GRAYMAX]. These patterns
# shall be the patterns produced by the following steps:
# 1) Create a bitmap BHDC. The height of this bitmap is HDPH. The width of the bitmap is (GRAYMAX + 1) ◊
# HDPW. This bitmap contains all the patterns concatenated left to right.
# 2) Decode the collective bitmap using a generic region decoding procedure as described in 6.2. Set the parameters to
# this decoding procedure as shown in Table 27.
# 3) Set:
# GRAY = 0
# 4) While GRAY ≤ GRAYMAX:
# a) Let the subimage of BHDC consisting of HPH rows and columns HDPW ◊ GRAY through HDPW ◊ (GRAY
# + 1) ñ 1 be denoted BP . Set:
# HDPATS[GRAY] = BP
# b) Set:
# GRAY = GRAY + 1
# Table 27 ñ Parameters used to decode a pattern dictionary's collective bitmap
Name Value
MMR HDMMR GBW (GRAYMAX + 1) ◊ HDPW GBH HDPH
| GBTEMPLATE | HDTEMPLATEa) |
|---|---|
| TPGDON   | 0 a) |
| USESKIP   |  |
|  | 0 |
| GBATX   1 | ñHDPW a) |
| GBATY 1   | 0 a) |
| GBATX   2 | ñ3 b) |
| GBATY 2   | ñ1 b) |
| GBATX   3 | 2 b) |
| GBATY 3   | ñ2 b) |
| GBATX   4 | ñ2 b) |
| GBATY 4   | ñ2 b) |
| a)   If   HDMMR   =   1   |  |
| b)   If   HDMMR   =   1   or   |   ≠   0 then this parameter has no value. |

---

## Page 58

ISO/IEC 14492:2001 (E)
### Control Decoding Procedure
7.1 General description
This decoding procedure controls the invocation of all the other decoding procedure. The encoded bitstream consists of a collection of segments, each containing a part of the data necessary for decoding. There are several different types of segments.
A segment has two parts: a segment header part and a segment data part. All types of segments use a common format for the segment header, but different formats for segment data.
Some segments give information about the structure of the document: start of page, end of page, and so on. Some segments code regions, used in turn to produce the decoded image of a certain page. Some segments ("dictionary segments") do neither, but instead define resources that can be used by segments that code regions.
A segment can be associated with some page, or not associated with any page. A segment can refer to other, preceding, segments. A segment also includes retention bits for the segment that it refers to, and for itself; these indicate when the decoder may discard the data created by decoding a segment.
EXAMPLE ñ A text region segment may make use of symbols defined in preceding symbol dictionary segments. This is indicated by the text region's segment header referring to those symbol dictionary segments.
The format of segment headers is described in 7.2. The types of segments are defined in 7.3. The syntax of each type of segment is defined in 7.4.
In the following, some references are made to "preceding" and "following" segments (and other indications implying an order of segments). These terms are defined with reference to the order imposed on the segments by their segment numbers: a segment precedes all segments whose segment numbers are larger than its segment number. In the sequential and random-access organisations (see D.1 and D.2), the segments must appear in the file in increasing order of their segment numbers. However, in the embedded organisation (see D.3), this is not the case because the JBIG2 segments are encapsulated in another file format.
NOTE ñ It is possible for there to be gaps in the segment numbering. A JBIG2 file might contain segments numbered 2, 3, 4, 8 and 10. This can occur due to editing: the segment numbers might originally have been contiguous, but at some point in the life of the file some pages were deleted and the remaining segments not renumbered.
A segment's header part always begins and ends on a byte boundary.
A segment's data part always begins and ends on a byte boundary. Any unused bits in the final byte of a segment must contain 0, and shall not be examined by the decoder.
The segment header part and the segment data part of a segment need not occur contiguously in the bitstream being decoded. See Annex D for an organisation where the segment header part of a segment may be stored at some distance from the segment data part of that segment.
This clause contains figures that describe various parts of the encoded data, such as Figures 25 and 31. These conventions used in these figures are:
• The first byte encountered in the bitstream is at the left end.
• Fields whose sizes are fixed, and that are always present, are outlined with narrow lines.
• Fields whose sizes are not fixed, or that are not present in all cases, or whose structures are fully described elsewhere, are outlined with heavy lines.
• Some figures (such as Figure 25) are divided into fields, each of which is an integral number of bytes long. In these figures, hash marks extending down from the top of the figure denote byte boundaries, and fields are separated by lines running the full height of the figure.
• The remaining figures are divided into fields, each of which is an integral number of bits long, making up an integral number of bytes. In these figures, short hash marks extending up from the bottom of the figure show bit boundaries. Fields are separated by longer hash marks extending up from the bottom of the figure. Each bit's number is shown below the figure.

---

## Page 59

ISO/IEC 14492:2001 (E)
7.2 Segment header syntax
7.2.1 Segment header fields
A segment header contains the fields shown in Figure 25 and described below.
Segment Referred-to segment
Referred-to segment Segment page header
| Segment number | count and |
|---|---|
|  |  |
|  |  |
|  | retention flags |
|  | Figure 25   ñ   |
Segment number ñ see 7.2.2.
Segment header flags ñ see 7.2.3.
Referred-to segment count and retention flags ñ see 7.2.4.
Referred-to segment number fields ñ see 7.2.5.
Segment page association ñ see 7.2.6.
Segment data length ñ see 7.2.7.
7.2.2 Segment number
This four-byte field contains the segment's segment number. The valid range of segment numbers is 0 through 4294967295 (0xFFFFFFFF) inclusive. As mentioned before, it is possible for there to be gaps in the segment numbering.
7.2.3 Segment header flags
This is a 1-byte field. The bits that are defined are shown in Figure 26 and are described below.
size
7 6 5 4 3 2 1 0
Figure 26 ñ Segment header flags
Bits 0-5 Segment type. See 7.3.
Bit 6 Page association field size. See 7.2.6.
NOTE ñ The intention of this bit is to indicate to the decoder that the segment is only referred to by a small number of extension segments. The decoder may take some expensive actions when segments are flagged as retained, but if this retention is only for the benefit of the segment's attached extension segments, these actions may not be necessary. Knowing this in advance is helpful.
7.2.4 Referred-to segment count and retention flags
NOTE ñ The decoder's memory requirements can be reduced by letting it know when it is allowed to forget about the data represented by some previous segment.
Deferred non-retain

---

## Page 60

ISO/IEC 14492:2001 (E)
The number of bytes in this field depends on the number of segments referred to by this segment. If this segment refers to four or fewer segments, then this field is one byte long. If this segment refers to more than four segments, then this field is 4 + (R + 1)/8 bytes long where R is the number of segments that this segment refers to.
EXAMPLE ñ If this segment refers to between five and seven other segments, then the field is five bytes long; if it refers to between eight and fifteen other segments, then the field is six bytes long.
The three most significant bits of the first byte in this field determine the length of the field. If the value of this three-bit subfield is between 0 and 4, then the field is one byte long. If the value of this three-bit subfield is 7, then the field is at least five bytes long. This three-bit subfield must not contain values of 5 and 6.
In the case where the field is one byte long, that byte is formatted as shown in Figure 27 and as described below.
Retain bit Retain bit Retain bit Retain bit Retain bit for 4th for 3rd for 2nd for 1st for this segment segment segment segment segment
7 6 5 4 3 2 1 0
Figure 27 ñ Referred-to segment count and retention flags ñ short form
Count of referred-to segments
Bit 0 Retain bit for this segment.
Bit 1 Retain bit for the first referred-to segment. If this segment refers to no other segments, this field must contain 0.
Bit 2 Retain bit for the second referred-to segment. If this segment refers to fewer than two other segments, this field must contain 0.
Bit 3 Retain bit for the third referred-to segment. If this segment refers to fewer than three other segments, this field must contain 0.
Bit 4 Retain bit for the fourth referred-to segment. If this segment refers to fewer than four other segments, this field must contain 0.
Bits 5-7 Count of referred-to segments. This field may take on values between zero and four. This specifies the number of segments that this segment refers to.
In the case where the field is in the long format (at least five bytes long), it is composed of an initial four-byte field, followed by a succession of one-byte fields. The initial four-byte field is formatted as follows.
Bits 0-28 Count of referred-to segments. This specifies the number of segments that this segment refers to.
Bits 29-31 Indication of long-form format. This field must contain the value 7.
The first one-byte field following the initial four-byte field is formatted as follows.
Bit 0 Retain bit for this segment.
Bit 1 Retain bit for the first referred-to segment.
Bit 2 Retain bit for the second referred-to segment.
Bit 3 Retain bit for the third referred-to segment.
Bit 4 Retain bit for the fourth referred-to segment.
Bit 5 Retain bit for the fifth referred-to segment. If this segment refers to fewer than five other segments, this field must contain 0.
Bit 6 Retain bit for the sixth referred-to segment. If this segment refers to fewer than six other segments, this field must contain 0.
Bit 7 Retain bit for the seventh referred-to segment. If this segment refers to fewer than seven other segments, this field must contain 0.

---

## Page 61

ISO/IEC 14492:2001 (E)
The second one-byte field, if present, contains retain bits for the eighth through fifteenth referred-to segments; the bits corresponding to any segments beyond the count of segments actually referred to must be 0. Succeeding one-byte fields are formatted similarly.
If the retain bit for this segment value is 0, then no segment may refer to this segment.
If the retain bit for the first referred-to segment value is 0, then no segment after this one may refer to the first segment that this segment refers to (i.e. this segment is the last segment that refers to that other segment). Further retain bit values have similar meanings: if the retain bit for the Kth referred-to segment value is 0, then no segment after this one may refer to the Kth segment that this segment refers to.
7.2.5 Referred-to segment numbers
This field contains the segment numbers of the segments that this segment refers to, if any. The number of values in this field is determined by the referred-to segment count and retention flags field. Each value is the segment number of a segment that this segment refers to. If a segment refers to other segments, it must refer to only segments with lower segment numbers. When the current segment's number is 256 or less, then each referred-to segment number is one byte long. Otherwise, when the current segment's number is 65536 or less, each referred-to segment number is two bytes long. Otherwise, each referred-to segment number is four bytes long.
7.2.6 Segment page association
This field encodes the number of the page to which this segment belongs. The first page must be numbered "1". This field may contain a value of zero; this value indicates that this segment is not associated with any page.
A segment that has a non-zero segment page association may only be referred to by segments having the same segment page association value as it.
NOTE ñ Most documents have fewer than 256 pages, so this field has a short form that can hold values from 0 to 255 in a single byte. The page association field for unassociated segments can also be only a single byte long.
7.2.7 Segment data length
This 4-byte field contains the length of the segment's segment data part, in bytes.
NOTE ñ Given a list of segment headers in the random-access organisation (see Figure D.2), a decoder can build a map of the rest of the file by knowing the length of the data associated with each segment. This allows it to perform random access.
7.2.8 Segment header example
EXAMPLE 1 ñ A segment header consisting of the sequence of bytes:
0x00 0x00 0x00 0x20 0x86 0x6B 0x02 0x1E 0x05 0x04
is parsed as follows:
0x00 0x00 0x00 0x20 This segment's number is 0x00000020, or 32 decimal.
0x86 This segment's type is 6. Its page association field is one byte long. It is retained by only its attached extension segments.
0x6B This segment refers to three other segments. It is referred to by some other segment. This is the last reference to the second of the three segments that it refers to.
0x02 0x1E 0x05 The three segments that it refers to are numbers 2, 30, and 5.
0x04 This segment is associated with page number 4.

---

## Page 62

ISO/IEC 14492:2001 (E)
EXAMPLE 2 ñ A segment header consisting of the sequence of bytes, in hexadecimal:
00 00 02 34 40 E0 00 00 09 02 FD 01 00 00 02 00 1E 00 05 02 00 02 01 02 02 02 03 02 04 00 00 04 01
00 00 04 01 This segment is associated with page number 1025.
7.3 Segment types
Each segment has a certain type. This type specifies the type of the data associated with the segment. This type restricts which other segments it may refer to, and which other segments may refer to it. These restrictions are detailed in 7.3.1.
The segment type is a number between 0 and 63, inclusive. Not all values are allowed. The allowed list of segment types, their full names, and where their formats are defined, are:
| 0 | Symbol dictionary ñ see 7.4.2. |
|---|---|
| 4   | Intermediate text region ñ see 7.4.3. |
| 6   | Immediate text region ñ see 7.4.3. |
62 Extension ñ see 7.4.14.
NOTE ñ These segment type numbers are allocated according to the following rules. The two high-order bits (bits 4-5) of this number specify the primary type of the segment, and the four low-order (bits 0-3) bits specify the secondary type of the segment.

---

## Page 63

ISO/IEC 14492:2001 (E)
The primary types are:
| 0 | Symbol bitmap data |
|---|---|
| 1   | Halftone bitmap data |
| 2   | Generic bitmap data |
| 3   | Metadata |
|  | Primary types 0-2 are collectively referred to as region types. |
|  |  |
| Bit 0   |  |
|  | If this bit is   1 |
| Bit 1   |  |
|  | If this bit is   1 |
|  |  |
| Bits 2-3   |  |
|  |  |
|  | 0   Dictionary |
|  | 1   Direct Region |
|  | 2   Refinement Region |
|  | For metadata, the interpretations of the four low-order bits are: |
| 0   |  |
|  | Page information |
| 1   |  |
|  | End of page |
| 2   |  |
|  | End of stripe |
| 3   |  |
|  | End of file |
| 4   |  |
|  | Profiles |
| 5   |  |
|  | Tables |
| 6-13   | Reserved |
| 14   |  |
|  | Extension |
| 15   |  |
|  | Reserved |
| The   | segments   of   types   |
|  |  |
|  |  |
|  |  |
|  | "region segments". |
| The   | segments   of   types   |
|  |  |
|  |  |
| region segments". |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
| region segments". |  |
|  |  |
|  |  |
| 7.3.1   |  |
|  | Rules for segment references |
|  | The rules for segment references are as follows: |
• A segment of type "symbol dictionary" (type 0) may refer to any number of segments of type "symbol dictionary" and to up to four segments of type "tables".

---

## Page 64

ISO/IEC 14492:2001 (E)
•
•
•
• segment's type imposes some restriction.
7.3.2 Rules for page associations
7.4 Segment syntaxes
decoded.
7.4.1 Region segment information field
Region segment Region segment Region segment Region segment bitmap width bitmap height bitmap X location bitmap Y location
Figure 28 ñ Region segment data header structure
Region segment bitmap width ñ see 7.4.1.1.
Region segment bitmap height ñ see 7.4.1.2.
Region segment bitmap X location ñ see 7.4.1.3.
Region segment bitmap Y location ñ see 7.4.1.4.
Region segment flags ñ see 7.4.1.5.
Region segment flags
A segment of type "intermediate generic region", "immediate generic region" or "immediate lossless generic region"
A segment of type "intermediate generic refinement region" (type 40) must refer to exactly one other segment. This
A segment of type "immediate generic refinement region" or "immediate lossless generic refinement region" (type 42 or 43) may refer to either zero other segments or exactly one other segment. If it refers to one other segment then
A segment of type "page information" (type 48) must not refer to any other segments.
A segment of type "extension" (type 62) may refer to any number of segments of any type, unless the extension
Every region segment must be associated with some page (i.e. have a non-zero page association field). "Page information", "end of page" and "end of stripe" segments must be associated with some page. "End of file" segments must not be associated with any page. Segments of other types may be associated with a page or not.
If a segment is not associated with any page, then it must not refer to any segment that is associated with any page.
If a segment is associated with a page, then it may refer to segments that are not associated with any page, and to segments that are associated with the same page. It must not refer to any segment that is associated with a different page.
This subclause describes in detail the syntax of the segment data part of each type of segment, and how it is to be
Every region segment's data part begins with a region segment information field; its format is specified here. A region segment information field contains the following subfields, as shown in Figure 28 and as described below:

---

## Page 65

ISO/IEC 14492:2001 (E)
7.4.1.1 Region segment bitmap width
This four-byte field gives the width in pixels of the bitmap encoded in this segment.
7.4.1.2 Region segment bitmap height
This four-byte field gives the height in pixels of the bitmap encoded in this segment.
7.4.1.3 Region segment bitmap X location
This four-byte field gives the horizontal offset in pixels of the bitmap encoded in this segment relative to the page bitmap.
7.4.1.4 Region segment bitmap Y location
This four-byte field gives the vertical offset in pixels of the bitmap encoded in this segment relative to the page bitmap.
7.4.1.5 Region segment flags
This one-byte field is formatted as shown in Figure 29 and as described below.
Reserved External combination Must be 0 operator
7 6 5 4 3 2 1 0
Figure 29 ñ Region segment flags field structure
Bits 0-2 External combination operator. This three-bit field can take on the following values, representing one of five possible combination operators:
Bits 3-7 Reserved; must be 0.
In other words, this region segment information field describes the size and location of the bitmap encoded in this segment.
EXAMPLE ñ If the size and location values are (in order) 100, 200, 50 and 75, then this segment describes a bitmap 100 pixels wide, 200 pixels high, whose top left corner is 50 pixels to the right of, and 75 pixels below, the page's top left corner.
7.4.2 Symbol dictionary segment syntax
7.4.2.1 Symbol dictionary segment data header
A symbol dictionary segment's data part begins with a symbol dictionary segment data header, containing the fields shown in Figure 30 and described below:
Symbol
Symbol dictionary Symbol dictionary dictionary SDNUMEXSYMS SDNUMNEWSYMS AT flags refinement AT flags flags
Figure 30 ñ Symbol dictionary segment data header structure

---

## Page 66

ISO/IEC 14492:2001 (E)
Symbol dictionary flags ñ see 7.4.2.1.1.
Symbol dictionary AT flags ñ see 7.4.2.1.2.
Symbol dictionary refinement AT flags ñ see 7.4.2.1.3.
SDNUMEXSYMS ñ see 7.4.2.1.4.
SDNUMNEWSYMS ñ see 7.4.2.1.5.
7.4.2.1.1 Symbol dictionary flags
This two-byte field is formatted as shown in Figure 31 and as described below:
Bitmap Bitmap
| Must be 0 | LATE | LATE | context context |
|---|---|---|---|
|  |  |  |  |
|  |  |  |  |
|  |  |  |  |
SDRTEMP coding SDHUFFDW SDHUFFDH
| coding | SDREF- |
|---|---|
| AGGINST |  |
|  | SDHUFF |
| selection |  |
| retained used |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
| 9   |  |
| 8   |  |
| 7   |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  | 2   |
|  | 1   |
|  | 0 |
| Figure 31 ñ Symbol dictionary flags field structure |  |
Bit 0 SDHUFF
If this bit is 1, then the segment uses the Huffman encoding variant. If this bit is 0, then the segment uses the arithmetic encoding variant. The setting of this flag determines how the data in this segment are encoded, and may also modify the order in which some of the data are encoded.
Bit 1 SDREFAGG
If this bit is 0, then no refinement or aggregate coding is used in this segment. If this bit is 1, then every symbol bitmap is refinement/aggregate coded.
Bit 2-3 SDHUFFDH selection. This two-bit field can take on one of three values, indicating which table is to be used for SDHUFFDH
0 Table B.4 1 Table B.5 3 User-supplied table
The value 2 is not permitted.
If SDHUFF is 0 then this field must contain the value 0.
Bits 4-5 SDHUFFDW selection. This two-bit field can take on one of three values, indicating which table is to be used for SDHUFFDW.
0 Table B.2 1 Table B.3 3 User-supplied table
The value 2 is not permitted.
If SDHUFF is 0 then this field must contain the value 0.
Bit 6 SDHUFFBMSIZE selection.
If this field is 0 then Table B.1 is used for SDHUFFBMSIZE. If this field is 1 then a user-supplied table is used for SDHUFFBMSIZE.
If SDHUFF is 0 then this field must contain the value 0.
Bit 7 SDHUFFAGGINST selection.
If this field is 0 then Table B.1 is used for SDHUFFAGGINST. If this field is 1 then a user-supplied table is used for SDHUFFAGGINST.
If SDHUFF is 0 or SDREFAGG is 0 then this field must contain the value 0.
Bit 8 Bitmap coding context used.
If SDHUFF is 1 and SDREFAGG is 0 then this field must contain the value 0.

---

## Page 67

ISO/IEC 14492:2001 (E)
Bit 9 Bitmap coding context retained.
If SDHUFF is 1 and SDREFAGG is 0 then this field must contain the value 0.
Bits 10-11 SDTEMPLATE
This field controls the template used to decode symbol bitmaps if SDHUFF is 0. If SDHUFF is 1, this field must contain the value 0.
Bit 12 SDRTEMPLATE
This field controls the template used to decode symbol bitmaps if SDREFAGG is 1. If SDREFAGG is 0, this field must contain the value 0.
Bits 13-15 Reserved; must be 0.
7.4.2.1.2 Symbol dictionary AT flags
This field is only present if SDHUFF is 0. If SDTEMPLATE is 0, it is an eight-byte field, formatted as shown in Figure 32 and as described below.
SDATX SDATY SDATX SDATY SDATX SDATY SDATX
1 1 2 2 3 3 4 SDATY4
Figure 32 ñ Symbol dictionary AT flags field structure when SDTEMPLATE is 0
Byte 0 SDATX1
Byte 1 SDATY1
Byte 2 SDATX2
Byte 3 SDATY2
Byte 4 SDATX3
Byte 5 SDATY3
Byte 6 SDATX4
Byte 7 SDATY4
If SDTEMPLATE is 1, 2 or 3, it is a two-byte field formatted as shown in Figure 33 and as described below.
SDATX
1 SDATY1
Figure 33 ñ Symbol dictionary AT flags field structure when SDTEMPLATE is not 0
Byte 0 SDATX1
Byte 1 SDATY1
If SDTEMPLATE is 1, 2 or 3 then the values of SDATX2 through SDATX4 and SDATY2 through SDATY4 are all zero.
The AT coordinate X and Y fields are signed values, and may take on values that are permitted according to Figure 7.
7.4.2.1.3 Symbol dictionary refinement AT flags
This field is only present if SDREFAGG is 1 and SDRTEMPLATE is 0. It is a four-byte field, formatted as shown in Figure 34 and as described below.

---

## Page 68

ISO/IEC 14492:2001 (E)
SDRATX1 SDRATY1 SDRATX2 SDRATY2
Figure 34 ñ Symbol dictionary refinement AT flags field structure
Byte 0 SDRATX1
Byte 1 SDRATY1
Byte 2 SDRATX2
Byte 3 SDRATY2
The AT coordinate X and Y fields are signed values, and may take on values that are permitted according to 6.3.5.3.
7.4.2.1.4 Number of exported symbols (SDNUMEXSYMS)
This four-byte field contains the number of symbols exported from this dictionary.
It is very useful for the decoder to be able to find out easily how many symbols are present ñ for example, it might want to allocate an array of structures before beginning to decode the dictionary.
7.4.2.1.5 Number of new symbols (SDNUMNEWSYMS)
NOTE ñ SDNUMEXSYMS and SDNUMNEWSYMS are often, but not always, the same value. For example, if a dictionary re-exports some of the symbols that it imported from dictionaries that it refers to, then the dictionary effectively copies those symbols. Those symbols are reflected in SDNUMEXSYMS but not in SDNUMNEWSYMS. Another possible source of difference comes from the possibility that a dictionary defines some symbols that it does not export.
7.4.2.1.6 Symbol dictionary segment Huffman table selection
4) SDHUFFAGGINST
If a user-specified table is used for SDHUFFDW, then this table must be capable of coding the out-of-band value OOB. If a user-specified table is used for SDHUFFDH, SDHUFFBMSIZE or SDHUFFAGGINST, then this table must not be capable of coding the out-of-band value OOB.
EXAMPLE ñ If SDHUFFDH and SDHUFFAGGINST are specified to use user-supplied tables, and SDHUFFDW and SDHUFFBMSIZE are specified to use standard tables (Tables B.2 and B.1 respectively), then this segment must refer to exactly two tables segments; the tables segment that is referred to first is used for SDHUFFDH and the tables segment that is referred to second is used for SDHUFFAGGINST.
7.4.2.2 Decoding a symbol dictionary segment
3) If the "bitmap coding context used" bit in the header was 1, then, as described in E.3.8, set the arithmetic coding statistics for the generic region and generic refinement region decoding procedures to the values that they contained at the end of decoding the last-referred-to symbol dictionary segment. That symbol dictionary segment's symbol

---

## Page 69

# ISO/IEC 14492:2001 (E)
# dictionary segment data header must have had the "bitmap coding context retained" bit equal to 1. The values of
# SDHUFF, SDREFAGG, SDTEMPLATE, SDRTEMPLATE, and all of the AT locations (both direct and
# refinement) for this symbol dictionary must match the corresponding values from the symbol dictionary whose
# context values are being used.
# 4) If the "bitmap coding context used" bit in the header was 0, then, as described in E.3.7, reset all the arithmetic
# coding statistics for the generic region and generic refinement region decoding procedures to zero.
# 5) Reset the arithmetic coding statistics for all the contexts of all the arithmetic integer coders to zero.
# 6) Invoke the symbol dictionary decoding procedure described in 6.5, with the parameters to the symbol dictionary
# decoding procedure set as shown in Table 28.
# 7) If the "bitmap coding context retained" bit in the header was 1, then, as described in E.3.8, preserve the current
# contents of the arithmetic coding statistics for the generic region and generic refinement region decoding
# procedures.
NOTE ñ Step 3) is intended to reduce the coding costs of symbol dictionaries. A side-effect of decoding a symbol dictionary is that the arithmetic coding statistics used for coding bitmaps "learn" the approximate statistics of the symbols in that symbol dictionary. These two steps (3 and 7) allow some limited reuse of these statistics: the statistics learned when decoding the symbol dictionary, that is the last symbol dictionary referred to, are used as a starting point for decoding this symbol dictionary.
Step 7) is explicitly present because not every symbol dictionary's arithmetic coding statistics will be used by another dictionary. Knowing that they will not be used allows the decoder to discard them, reducing memory usage.
# Table 28 ñ Parameters used to decode a symbol dictionary segment
Name Value
SDHUFF As shown in 7.4.2.1.1. SDREFAGG As shown in 7.4.2.1.1. SDNUMINSYMS The total number of exported symbols from all the symbol dictionary segments referred to by this segment. SDINSYMS Concatenate the exported symbol arrays from all the symbol dictionary segments referred to by this segment, in the order in which they are referred to. SDNUMNEWSYMS As shown in 7.4.2.1.5. SDNUMEXSYMS As shown in 7.4.2.1.4. SDHUFFDH See 7.4.2.1.6. SDHUFFDW See 7.4.2.1.6. SDHUFFBMSIZE See 7.4.2.1.6. SDHUFFAGGINST See 7.4.2.1.6. SDTEMPLATE See 7.4.2.1.1. SDATX
| 1 | See 7.4.2.1.2. |
|---|---|
| SDATY |  |
| 1   | See 7.4.2.1.2. |
| SDATX   |  |
| 2 | See 7.4.2.1.2. |
| SDATY |  |
| 2   | See 7.4.2.1.2. |
| SDATX   |  |
| 3 | See 7.4.2.1.2. |
| SDATY |  |
| 3   | See 7.4.2.1.2. |
| SDATX   |  |
| 4 | See 7.4.2.1.2. |
| SDATY |  |
| 4   | See 7.4.2.1.2. |
| SDRTEMPLATE |  |
|  | See 7.4.2.1.1. |
| SDRATX   |  |
| 1 | See 7.4.2.1.3. |
| SDRATY |  |
| 1   | See 7.4.2.1.3. |
| SDRATX   |  |
| 2 | See 7.4.2.1.3. |
| SDRATY |  |
| 2   | See 7.4.2.1.3. |

---

## Page 70

# ISO/IEC 14492:2001 (E)
# 7.4.3
# Text region segment syntax
# The data parts of all three of the text region segment types ("intermediate text region", "immediate text region" and
# "immediate lossless text region") are coded identically, but are acted upon differently, see 8.2. The syntax of these
# segment types' data parts is specified here.
# 7.4.3.1 Text region segment data header
# The data part of a text region segment begins with a text region segment data header. This header contains the fields
# shown in Figure 35 and described below.
# Region segment information field ñ see 7.4.1.
# Text region segment flags ñ see 7.4.3.1.1.
# Text region segment Huffman flags ñ see 7.4.3.1.2.
# Text region segment refinement AT flags ñ see 7.4.3.1.3.
# SBNUMINSTANCES ñ see 7.4.3.1.4.
# Text region segment symbol ID Huffman decoding table ñ see 7.4.3.1.5.
Region segment Text region Text region
| Text region | Text region |
|---|---|
|   |  |
|  |  |
|  |  |
| segment | segment symbol ID |
|  |  |
|  |  |
| flags | Huffman decoding table |
# Figure 35 ñ Text region segment data header
# 7.4.3.1.1 Text region segment flags
# This two-byte field is formatted as shown in Figure 36 and as described below.
SBR-
SBDEF-TRANS-SBREF-TEMP-SBDSOFFSET SBCOMBOP REFCORNER LOGSBSTRIPS SNHUFF PIXEL POSED INE LATE
12 11 10 9 8 7 6 5 4 3 2 1 0
# Figure 36 ñ Text region flags field structure
# Bit 0
# SBHUFF
# If this bit is 1, then the segment uses the Huffman encoding variant. If this bit is 0, then the segment uses
# the arithmetic encoding variant. The setting of this flag determines how the data in this segment are
# encoded.
# Bit 1
# SBREFINE
# If this bit is 0, then the segment contains no symbol instance refinements. If this bit is 1, then the segment
# may contain symbol instance refinements.
# Bits 2-3
# LOGSBSTRIPS
# This two-bit field codes the base-2 logarithm of the strip size used to encode the segment. Thus, strip sizes
# of 1, 2, 4, and 8 can be encoded.
# Bits 4-5
# REFCORNER. The four values that this two-bit field can take on are:
# 0 BOTTOMLEFT
# 1 TOPLEFT
# 2 BOTTOMRIGHT
# 3 TOPRIGHT

---

## Page 71

ISO/IEC 14492:2001 (E)
NOTE ñ The best compression is usually achieved when the reference point of each symbol is on the text baseline. Given that text can run in any of eight directions, there needs to be some flexibility in which corner of a given symbol is used as the reference point.
Bit 6 TRANSPOSED
If this bit is 1, then the primary direction of coding is top-to-bottom. If this bit is 0, then the primary direction of coding is left-to-right. This allows for text running up and down the page.
Bits 7-8 SBCOMBOP. This field has four possible values, representing one of four possible combination operators:
0 OR 1 AND 2 XOR 3 XNOR
Bit 9 SBDEFPIXEL
This bit contains the initial value for every pixel in the text region, before any symbols are drawn.
Bits 10-14 SBDSOFFSET
This signed five-bit field contains the value of SBDSOFFSET ñ see 6.4.8.
Bit 15 SBRTEMPLATE
This field controls the template used to decode symbol instance refinements if SBREFINE is 1. If SBREFINE is 0, this field must contain the value 0.
7.4.3.1.2 Text region segment Huffman flags
This field is only present if SBHUFF is 1.
This two-byte field is formatted as shown in Figure 37 and as described below.
Reserved SBHUFF-Must be RSIZE 0 selection
12 11 10 9 8 7 6 5 4 3 2 1 0
Figure 37 ñ Text region Huffman flags field structure
SBHUFFRDY SBHUFFRDX SBHUFFRDH SBHUFFRDW SBHUFFDT SBHUFFDS SBHUFFFS selection selection selection selection selection selection selection
Bits 0-1 SBHUFFFS selection. This two-bit field can take on one of three values, indicating which table is to be used for SBHUFFFS.
0 Table B.6 1 Table B.7 3 User-supplied table
The value 2 is not permitted.
Bits 2-3 SBHUFFDS selection. This two-bit field can take on one of four values, indicating which table is to be used for SBHUFFDS.
0 Table B.8 1 Table B.9 2 Table B.10 3 User-supplied table
Bits 4-5 SBHUFFDT selection. This two-bit field can take on one of four values, indicating which table is to be used for SBHUFFDT.
0 Table B.11 1 Table B.12 2 Table B.13 3 User-supplied table

---

## Page 72

ISO/IEC 14492:2001 (E)
Bits 6-7 SBHUFFRDW selection. This two-bit field can take on one of three values, indicating which table is to be used for SBHUFFRDW.
0 Table B.14 1 Table B.15 3 User-supplied table
The value 2 is not permitted. If SBREFINE is 0 then this field must contain the value 0.
Bits 8-9 SBHUFFRDH selection. This two-bit field can take on one of three values, indicating which table is to be used for SBHUFFRDH.
0 Table B.14 1 Table B.15 3 User-supplied table
The value 2 is not permitted. If SBREFINE is 0 then this field must contain the value 0.
Bits 10-11 SBHUFFRDX selection. This two-bit field can take on one of three values, indicating which table is to be used for SBHUFFRDX.
0 Table B.14 1 Table B.15 3 User-supplied table
The value 2 is not permitted. If SBREFINE is 0 then this field must contain the value 0.
Bits 12-13 SBHUFFRDY selection. This two-bit field can take on one of three values, indicating which table is to be used for SBHUFFRDY.
0 Table B.14 1 Table B.15 3 User-supplied table
The value 2 is not permitted. If SBREFINE is 0 then this field must contain the value 0.
Bit 14 SBHUFFRSIZE selection. If this field is 0 then Table B.1 is used for SBHUFFRSIZE. If this field is 1 then a user-supplied table is used for SBHUFFRSIZE. If SBREFINE is 0 then this field must contain the value 0.
Bit 15 Reserved; must be 0.
7.4.3.1.3 Text region refinement AT flags
This field is only present if SBREFINE is 1 and SBRTEMPLATE is 0. It is a four-byte field, formatted as shown in Figure 38 and as described below.
SBRATX1 SBRATY1 SBRATX2 SBRATY2
Figure 38 ñ Text region refinement AT flags field structure
Byte 0 SBRATX1
Byte 1 SBRATY1
Byte 2 SBRATX2
Byte 3 SBRATY2
The AT coordinate X and Y fields are signed values, and may take on values that are permitted according to 6.3.5.3.

---

## Page 73

ISO/IEC 14492:2001 (E)
7.4.3.1.4 Number of symbol instances (SBNUMINSTANCES)
This four-byte field contains the number of symbol instances coded in this segment.
7.4.3.1.5 Text region segment symbol ID Huffman decoding table
This field contains a coded version of the Huffman codes used to decode symbol instance IDs in the text region decoding procedure. It is decoded as specified in 7.4.3.1.7. It is only present if SBHUFF is 1.
7.4.3.1.6 Text region segment Huffman table selection
Set the values of the parameters SBHUFFFS, SBHUFFDS, SBHUFFDT, SBHUFFRDW, SBHUFFRDH, SBHUFFRDX, SBHUFFRDY and SBHUFFRSIZE according to the selection fields shown in 7.4.3.1.2, and the tables segments referred to by this segment. More precisely, of these eight Huffman tables, some may be specified to use some standard table, and some may be specified to use a user-supplied table. The number specified to use a user-supplied table must be equal to the number of tables segments referred to by this segment. These tables segments are matched up with the Huffman tables using user-supplied tables according to the order in which the tables segments are referred to, and the order:
1) SBHUFFFS
2) SBHUFFDS
3) SBHUFFDT
4) SBHUFFRDW
5) SBHUFFRDH
6) SBHUFFRDX
7) SBHUFFRDY
8) SBHUFFRSIZE
If a user-specified table is used for SBHUFFDS, then this table must be capable of coding the out-of-band value OOB. If a user-specified table is used for SBHUFFFS, SBHUFFDT, SBHUFFRDW, SBHUFFRDH, SBHUFFRDX, SBHUFFRDY or SBHUFFRSIZE then this table must not be capable of coding the out-of-band value OOB.
7.4.3.1.7 Symbol ID Huffman table decoding
This table is encoded as SBNUMSYMS symbol ID code lengths; the actual codes in SBSYMCODES are assigned from these symbol ID code lengths using the algorithm in B.3.
The symbol ID code lengths themselves are run-length coded and the runs Huffman coded. This is very similar to the "zlib" coded format documented in RFC 1951, though not identical. The encoding is based on the codes shown in Table 29.
Decoding a symbol ID Huffman table proceeds as follows:
1) Read the code lengths for RUNCODE0 through RUNCODE34; each is stored as a four-bit value.
2) Given the lengths, assign Huffman codes for RUNCODE0 through RUNCODE34 using the algorithm in B.3.
3) Read a Huffman code using this assignment. This decodes into one of RUNCODE0 through RUNCODE34. If it is RUNCODE32, read two additional bits. If it is RUNCODE33, read three additional bits. If it is RUNCODE34, read seven additional bits.
4) Interpret the RUNCODE code and the additional bits (if any) according to Table 29. This gives the symbol ID code lengths for one or more symbols.
5) Repeat steps 3) and 4) until the symbol ID code lengths for all SBNUMSYMS symbols have been determined.
6) Skip over the remaining bits in the last byte read, so that the actual text region decoding procedure begins on a byte boundary.
7) Assign a Huffman code to each symbol by applying the algorithm in B.3 to the symbol ID code lengths just decoded. The result is the symbol ID Huffman table SBSYMCODES.

---

## Page 74

ISO/IEC 14492:2001 (E)
RUNCODE0 RUNCODE1 RUNCODE2 RUNCODE3 RUNCODE4 RUNCODE5 RUNCODE6 RUNCODE7 RUNCODE8 RUNCODE9 RUNCODE10 RUNCODE11 RUNCODE12 RUNCODE13 RUNCODE14 RUNCODE15 RUNCODE16 RUNCODE17 RUNCODE18 RUNCODE19 RUNCODE20 RUNCODE21 RUNCODE22 RUNCODE23 RUNCODE24 RUNCODE25 RUNCODE26 RUNCODE27 RUNCODE28 RUNCODE29 RUNCODE30 RUNCODE31 RUNCODE32
RUNCODE33
RUNCODE34
Table 29 ñ Meaning of the run codes
Symbol ID code length is 0 Symbol ID code length is 1 Symbol ID code length is 2 Symbol ID code length is 3 Symbol ID code length is 4 Symbol ID code length is 5 Symbol ID code length is 6 Symbol ID code length is 7 Symbol ID code length is 8 Symbol ID code length is 9 Symbol ID code length is 10 Symbol ID code length is 11 Symbol ID code length is 12 Symbol ID code length is 13 Symbol ID code length is 14 Symbol ID code length is 15 Symbol ID code length is 16 Symbol ID code length is 17 Symbol ID code length is 18 Symbol ID code length is 19 Symbol ID code length is 20 Symbol ID code length is 21 Symbol ID code length is 22 Symbol ID code length is 23 Symbol ID code length is 24 Symbol ID code length is 25 Symbol ID code length is 26 Symbol ID code length is 27 Symbol ID code length is 28 Symbol ID code length is 29 Symbol ID code length is 30 Symbol ID code length is 31 Copy the previous symbol ID code length 3-6 times. The next two bits, plus 3, indicate this repeat length. Repeat a symbol ID code length of 0 for 3-10 times. The next three bits, plus 3, indicate this repeat length. Repeat a symbol ID code length of 0 for 11-138 times. The next seven bits, plus 11, indicate this repeat length.

---

## Page 75

## ISO/IEC 14492:2001 (E)
## EXAMPLE 1 ñ Suppose that SBNUMSYMS is 32 and the symbol ID code lengths for these 32 symbols are, in order:
0 0 0 9 6 6 6 6 3 4 4 4 4 4 4 0
7 9 8 7 5 5 5 5 5 5 3 6 7 4 7 7
## These symbol ID code lengths might be transmitted as the sequence of bytes, in hexadecimal:
## 0x50 0x03 0x35 0x32 0x53 0x00 0x00 0x00 0x00
## 0x00 0x00 0x00 0x00 0x00 0x00 0x00 0x35 0x0F
## 0x8B 0x30 0x9E 0xB8 0x5F 0x1D 0xD2 0x83 0x00
## Interpretation of this sequence of bytes can be separated into the following three steps:
## 1) The first 17 bytes plus the first four bits of the 18th byte assign code lengths to the 35 run codes as follows:
RUNCODE0 5 RUNCODE1 0 RUNCODE2 0 RUNCODE3 3 RUNCODE4 3 RUNCODE5 5 RUNCODE6 3 RUNCODE7 2 RUNCODE8 5 RUNCODE9 3 RUNCODE10 0 RUNCODE11 0 RUNCODE12 0 RUNCODE13 0 RUNCODE14 0 RUNCODE15 0 RUNCODE16 0 RUNCODE17 0 RUNCODE18 0 RUNCODE19 0 RUNCODE20 0 RUNCODE21 0 RUNCODE22 0 RUNCODE23 0 RUNCODE24 0 RUNCODE25 0 RUNCODE26 0 RUNCODE27 0 RUNCODE28 0 RUNCODE29 0 RUNCODE30 0 RUNCODE31 0 RUNCODE32 3 RUNCODE33 5 RUNCODE34 0
## Recall that codes that are not used are assigned a symbol ID code length of zero.
## 2) The algorithm of B.3 assigns the following Huffman codes to the run codes (run codes that are not assigned
## Huffman codes are omitted).
RUNCODE0 11100 RUNCODE3 010 RUNCODE4 011 RUNCODE5 11101 RUNCODE6 100 RUNCODE7 00 RUNCODE8 11110 RUNCODE9 101 RUNCODE32 110 RUNCODE33 11111

---

## Page 76

ISO/IEC 14492:2001 (E)
3) The remaining part of the byte sequence is:
0xF 0x8B 0x30 0x9E 0xB8 0x5F 0x1D 0xD2 0x83 0x00
the following results:
11111 000
run of three zero lengths
101 RUNCODE9
100 RUNCODE6
110 00
010 RUNCODE3
011 RUNCODE4
110 10 RUNCODE32(2)
11100 RUNCODE0
00 RUNCODE7
101 RUNCODE9
11110 RUNCODE8
00 RUNCODE7
11101 RUNCODE5
110 10 RUNCODE32(2)
010 RUNCODE3
100 RUNCODE6
00 RUNCODE7
011 RUNCODE4
00 RUNCODE7
00 RUNCODE7
0000 Four bits of padding to fill the last byte.
4)
symbol ID table is identical to that in the previous example.
0 0 0 1 8 8 8 8 64
| 32 | 32 | 32 | 32 | 32 | 32 |
|---|---|---|---|---|---|
|  |  |  |  |  |  |
|  |  |  |  |  |  |
|  |  |  |  |  |  |
|  |  |  |  |  |  |
|  |  |  |  |  |  |
|  |  |  |  |  |  |
|  |  |  |  |  |  |
|  |  |  |  |  |  |
| 16   |  |  |  |  |  |
| 16   64   |  |  |  |  |  |
|  | 8   |  |  |  |  |
|  |  | 4   |  |  |  |
|  |  |  | 32   |  |  |
|  |  |  |  | 4   |  |
once, the fifth symbol is used eight times, and so on.
Table 30 then shows, from right to left, the progression of the encoding.
0
4
where half of the first byte has already been consumed. Decoding this sequence using these Huffman codes provides
RUNCODE33(0) ñ that is, RUNCODE33 followed by three bits containing the value 0, indicating a
RUNCODE32(0) ñ that is, RUNCODE32 followed by two bits containing the value 0
After interpreting the run codes according to Table 29, the desired sequence of symbol ID code lengths is decoded.
EXAMPLE 2 ñ This example describes how an encoder might generate an encoded symbol ID Huffman table. The
Suppose that a text region refers to a dictionary containing 32 symbols, and that each symbol is used as follows:
For example, the first, second and third symbols in the symbol dictionary are not used at all, the fourth symbol is used

---

## Page 77

## ISO/IEC 14492:2001 (E)
## Table 30 ñ Example of symbol ID Huffman table encoding
Use Symbol ID Runs
| Symbol | RUNCODEs |
|---|---|
|  |  |
|  |  |
| Symbol #1   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE33(0) |
| Symbol #2   |  |
|  |  |
|  |  |
| Symbol #3   |  |
|  |  |
|  |  |
| Symbol #4   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE9 |
| Symbol #5   |  |
|  | RUNCODE6 |
| Symbol #6   |  |
|  |  |
|  |  |
|  | RUNCODE32(0) |
| Symbol #7   |  |
|  |  |
|  |  |
| Symbol #8   |  |
|  |  |
|  |  |
| Symbol #9   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE3 |
| Symbol #10   |  |
|  | RUNCODE4 |
| Symbol #11   | RUNCODE32(2) |
| Symbol #12   |  |
|  |  |
|  |  |
| Symbol #13   |  |
|  |  |
|  |  |
| Symbol #14   |  |
|  |  |
|  |  |
| Symbol #15   |  |
|  |  |
|  |  |
| Symbol #16   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE0 |
| Symbol #17   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE7 |
| Symbol #18   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE9 |
| Symbol #19   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE8 |
| Symbol #20   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE7 |
| Symbol #21   |  |
|  | RUNCODE5 |
| Symbol #22   |  |
|  |  |
|  |  |
|  | RUNCODE32(2) |
| Symbol #23   |  |
|  |  |
|  |  |
| Symbol #24   |  |
|  |  |
|  |  |
| Symbol #25   |  |
|  |  |
|  |  |
| Symbol #26   |  |
|  |  |
|  |  |
| Symbol #27   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE3 |
| Symbol #28   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE6 |
| Symbol #29   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE7 |
| Symbol #30   |  |
|  |  |
|  |  |
|  |  |
|  | RUNCODE4 |
| Symbol #31   |  |
|  |  |
|  |  |
|  | RUNCODE7 |
| Symbol #32   |  |
|  |  |
|  |  |
|  | RUNCODE7 |
|  |  |
|  |  |
|  |  |
|  |  |
| value "2", meaning "Copy the previous symbol ID code length 5 times". |  |
|  |  |
|  |  |
RUNCODE0 1 RUNCODE3 2 RUNCODE4 2 RUNCODE5 1 RUNCODE6 2 RUNCODE7 5 RUNCODE8 1 RUNCODE9 2 RUNCODE32 3 RUNCODE33 1

---

## Page 78

# ISO/IEC 14492:2001 (E)
# These counts are then converted into code lengths using a standard Huffman tree algorithm:
RUNCODE0 5 RUNCODE3 3 RUNCODE4 3 RUNCODE5 5 RUNCODE6 3 RUNCODE7 2 RUNCODE8 5 RUNCODE9 3 RUNCODE32 3 RUNCODE33 5
# The algorithm of B.3 assigns the following Huffman codes to the run codes:
RUNCODE0 11100 RUNCODE3 010 RUNCODE4 011 RUNCODE5 11101 RUNCODE6 100 RUNCODE7 00 RUNCODE8 11110 RUNCODE9 101 RUNCODE32 110 RUNCODE33 11111
# and these Huffman codes are then used to encode the "RUNCODEs" column of Table 30:
| 11111 000 | RUNCODE33(0) |
|---|---|
| 101   | RUNCODE9 |
| 100   | RUNCODE6 |
| 110 00   | RUNCODE32(0) |
| 010   | RUNCODE3 |
| 011   | RUNCODE4 |
| 110 10   | RUNCODE32(2) |
| 11100   | RUNCODE0 |
| 00   | RUNCODE7 |
| 101   | RUNCODE9 |
| 11110   | RUNCODE8 |
| 00   | RUNCODE7 |
| 11101   | RUNCODE5 |
| 110 10   | RUNCODE32(2) |
| 010   | RUNCODE3 |
| 100   | RUNCODE6 |
| 00   | RUNCODE7 |
| 011   | RUNCODE4 |
| 00   | RUNCODE7 |
| 00   | RUNCODE7 |
# The encoder now emits the encoded RUNCODE code lengths, followed by the sequence of RUNCODEs, plus four bits
# of padding to fill the last byte, yielding the sequence of bytes:
# 0x50 0x03 0x35 0x32 0x53 0x00 0x00 0x00 0x00
# 0x00 0x00 0x00 0x00 0x00 0x00 0x00 0x35 0x0F
# 0x8B 0x30 0x9E 0xB8 0x5F 0x1D 0xD2 0x83 0x00

---

## Page 79

## ISO/IEC 14492:2001 (E)
## 7.4.3.2 Decoding a text region segment
## A text region segment is decoded according to the following steps:
## 1) Interpret its header, as described in 7.4.3.1.
## 2) Decode (or retrieve the results of decoding) any referred-to symbol dictionary and tables segments.
## 3) As described in E.3.7, reset all the arithmetic coding statistics to zero.
## 4) Invoke the text region decoding procedure described in 6.4, with the parameters to the text region decoding
## procedure set as shown in Table 31.
## Table 31 ñ Parameters used to decode a text region segment
Name Value
SBNUMINSTANCES As shown in 7.4.3.1.4.
| SBSTRIPS | 2LOGSBSTRIPS |
|---|---|
| SBNUMSYMS   |  |
|  |  |
|  | segments referred to by this segment. |
| SBSYMCODES   |  |
|  | As specified in 7.4.3.1.7. |
| SBSYMCODELEN   |  |
|  | log 2   SBNUMSYMS |
| SBSYMS   |  |
|  |  |
|  |  |
|  | referred to. |
| SBHUFFFS   |  |
|  | See 7.4.3.1.6. |
| SBHUFFDS   |  |
|  | See 7.4.3.1.6. |
| SBHUFFDT   |  |
|  | See 7.4.3.1.6. |
| SBHUFFRDW   |  |
|  | See 7.4.3.1.6. |
| SBHUFFRDH   |  |
|  | See 7.4.3.1.6. |
| SBHUFFRDX   |  |
|  | See 7.4.3.1.6. |
| SBHUFFRDY   |  |
|  | See 7.4.3.1.6. |
| SBHUFFRSIZE   |  |
|  | See 7.4.3.1.6. |
| SBRTEMPLATE   |  |
|  | As shown in 7.4.3.1.1. |
| SBRATX   |  |
| 1 | See 7.4.3.1.3. |
| SBRATY |  |
| 1   | See 7.4.3.1.3. |
| SBRATX   |  |
| 2 | See 7.4.3.1.3. |
| SBRATY |  |
| 2   | See 7.4.3.1.3. |

---

## Page 80

ISO/IEC 14492:2001 (E)
7.4.4 Pattern dictionary segment syntax
7.4.4.1 Pattern dictionary segment data header
A pattern dictionary segment's data part begins with a pattern dictionary segment data header, formatted as shown in Figure 39 and as described below.
Halftone dictionary HDPW HDPH GRAYMAX flags
Figure 39 ñPattern dictionary header structure
Pattern dictionary flags ñ see 7.4.4.1.1.
HDPW ñ see 7.4.4.1.2.
HDPH ñ see 7.4.4.1.3.
GRAYMAX ñ see 7.4.4.1.4.
7.4.4.1.1 Pattern dictionary flags
This one-byte field is formatted as shown in Figure 40 and as described below.
Bit 0 HDMMR
If this bit is 1, then the segment uses the MMR encoding variant. If this bit is 0, then the segment uses the arithmetic encoding variant.
Bits 1-2 HDTEMPLATE
This field controls the template used to decode patterns if HDMMR is 0. If HDMMR is 1, this field must contain the value 0.
Bits 3-7 Reserved; must be 0.
Reserved Must be 0
HDTEMPLATE HDMMR
7 6 5 4 3 2 1 0
Figure 40 ñ Pattern dictionary flags field structure
7.4.4.1.2 Width of the patterns in the pattern dictionary (HDPW)
This one-byte field contains the width of the patterns defined in this pattern dictionary. Its value must be greater than zero.
7.4.4.1.3 Height of the patterns in the pattern dictionary (HDPH)
This one-byte field contains the height of the patterns defined in this pattern dictionary. Its value must be greater than zero.
7.4.4.1.4 Largest gray-scale value (GRAYMAX)
This four-byte field contains one less than the number of patterns defined in this pattern dictionary.

---

## Page 81

# ISO/IEC 14492:2001 (E)
# 7.4.4.2 Decoding a pattern dictionary segment
# A pattern dictionary segment is decoded according to the following steps:
# 1) Interpret its header, as described in 7.4.4.1.
# 2) As described in E.3.7, reset all the arithmetic coding statistics to zero.
# 3) Invoke the pattern dictionary decoding procedure described in 6.7, with the parameters to the pattern dictionary
# decoding procedure set as shown in Table 32.
# Table 32 ñ Parameters used to decode a pattern dictionary segment
Name Value
HDMMR As shown in 7.4.4.1.1. HDTEMPLATE As shown in 7.4.4.1.1. HDPW As shown in 7.4.4.1.2. HDPH As shown in 7.4.4.1.3. GRAYMAX As shown in 7.4.4.1.4.
# 7.4.5
# Halftone region segment syntax
# The data parts of all three of the halftone region segment types ("intermediate halftone region", "immediate halftone
# region" and "immediate lossless halftone region") are coded identically, but are acted upon differently, see 8.2. The
# syntax of these segment types' data parts is specified here.
# 7.4.5.1 Halftone region segment data header
# The data part of a halftone region segment begins with a halftone region segment data header. This header contains the
# fields shown in Figure 41 and described below.
Region segment Halftone region Halftone grid Halftone grid information field segment flags position and size step sizes
# Figure 41 ñ Halftone region segment data header structure
# Region segment information field ñ see 7.4.1.
# Halftone region segment flags ñ see 7.4.5.1.1.
# Halftone grid position and size ñ see 7.4.5.1.2.
# Halftone grid vector ñ see 7.4.5.1.3.
# 7.4.5.1.1 Halftone region segment flags
# This one-byte field is formatted as shown in Figure 42 and as described below.
HDEF-HENABLE-HCOMBOP HTEMPLATE HMMR PIXEL SKIP
7 6 5 4 3 2 1 0
# Figure 42 ñ Halftone region segment flags field structure

---

## Page 82

ISO/IEC 14492:2001 (E)
Bit 0 HMMR
If this bit is 1, then the segment uses the MMR encoding variant. If this bit is 0, then the segment uses the arithmetic encoding variant.
Bits 1-2 HTEMPLATE
This field controls the template used to decode halftone gray-scale value bitplanes if HMMR is 0. If HMMR is 1, this field must contain the value 0.
Bit 3 HENABLESKIP
This field controls whether gray-scale values that do not contribute to the region contents are skipped during decoding. If HMMR is 1, this field must contain the value 0.
Bits 4-6 HCOMBOP
This field has five possible values, representing one of five possible combination operators:
0 OR 1 AND 2 XOR 3 XNOR 4 REPLACE
Bit 7 HDEFPIXEL
This bit contains the initial value for every pixel in the halftone region, before any patterns are drawn.
7.4.5.1.2 Halftone grid position and size
This field describes the location and size of the grid of gray-scale values. See Figure 24 for an illustration of these values. It is formatted as shown in Figure 43 and as described below.
HGW HGH HGX HGY
Figure 43 ñ Halftone grid position and size field structure
HGW ñ see 7.4.5.1.2.1.
HGH ñ see 7.4.5.1.2.2.
HGX ñ see 7.4.5.1.2.3.
HGY ñ see 7.4.5.1.2.4.
7.4.5.1.2.1 Width of the gray-scale image (HGW)
This four-byte field contains the width of the array of gray-scale values.
7.4.5.1.2.2 Height of the gray-scale image (HGH)
This four-byte field contains the height of the array of gray-scale values.
7.4.5.1.2.3 Horizontal offset of the grid (HGX)
This signed four-byte field contains 256 times the horizontal offset of the origin of the halftone grid.
7.4.5.1.2.4 Vertical offset of the grid (HGY)
This signed four-byte field contains 256 times the vertical offset of the origin of the halftone grid.

---

## Page 83

# ISO/IEC 14492:2001 (E)
# 7.4.5.1.3 Halftone grid vector
# This field describes the vector used to draw the grid of gray-scale values. See Figure 24 for an illustration of these
# values. It is formatted as shown in Figure 44 and as described below.
HRX HRY
# Figure 44 ñ Halftone grid vector field structure
# HRX ñ See 7.4.5.1.3.1.
# HRY ñ See 7.4.5.1.3.2.
# 7.4.5.1.3.1
# Horizontal coordinate of the halftone grid vector (HRX)
# This unsigned two-byte field contains 256 times the horizontal coordinate of the halftone grid vector.
# 7.4.5.1.3.2
# Vertical coordinate of the halftone grid vector (HRY)
# This unsigned two-byte field contains 256 times the vertical coordinate of the halftone grid vector.
# 7.4.5.2 Decoding a halftone region segment
# A halftone region segment is decoded according to the following steps:
# 1) Interpret its header, as described in 7.4.5.1.
# 2) Decode (or retrieve the results of decoding) the referred-to pattern dictionary segment.
# 3) As described in E.3.7, reset all the arithmetic coding statistics to zero.
# 4) Invoke the halftone region decoding procedure described in 6.6, with the parameters to the halftone region decoding
# procedure set as shown in Table 33.
# Table 33 ñ Parameters used to decode a halftone region segment
Name Value
HBW As specified by the region segment bitmap width in this segment's region segment data header. HBH As specified by the region segment bitmap height in this segment's region segment data header. HMMR As shown in 7.4.5.1.1. HTEMPLATE As shown in 7.4.5.1.1. HENABLESKIP As shown in 7.4.5.1.1. HCOMBOP As shown in 7.4.5.1.1. HDEFPIXEL As shown in 7.4.5.1.1. HGW As shown in 7.4.5.1.2.1. HGH As shown in 7.4.5.1.2.2. HGX As shown in 7.4.5.1.2.3. HGY As shown in 7.4.5.1.2.4. HRX As shown in 7.4.5.1.3.1. HRY As shown in 7.4.5.1.3.2. HNUMPATS The number of patterns in the pattern dictionary segment referred to by this segment. HPATS The patterns in the pattern dictionary segment referred to by this segment. HPW The width, in pixels, of each of the patterns contained in HPATS. HPH The height, in pixels, of each of the patterns contained in HPATS.

---

## Page 84

ISO/IEC 14492:2001 (E)
7.4.6 Generic region segment syntax
The data parts of all three of the generic region segment types ("intermediate generic region", "immediate generic region" and "immediate lossless generic region") are coded identically, but are acted upon differently, see 8.2. The syntax of these segment types' data parts is specified here.
7.4.6.1 Generic region segment data header
The data part of a generic region segment begins with a generic region segment data header. This header contains the fields shown in Figure 45 and described below.
Region segment Generic region Generic region information field segment flags segment AT flags
Figure 45 ñ Generic region segment data header structure
Region segment information field ñ see 7.4.1.
Generic region segment flags ñ see 7.4.6.2.
Generic region segment AT flags ñ see 7.4.6.3.
7.4.6.2 Generic region segment flags
This one-byte field is formatted as shown in Figure 46 and as described below.
Reserved Must be 0
TPGDON GBTEMPLATE MMR
7 6 5 4 3 2 1 0
Figure 46 ñ Generic region segment flags field structure
Bit 0 MMR
Bits 1-2 GBTEMPLATE
This field specifies the template used for template-based arithmetic coding. If MMR is 1 then this field must contain the value zero.
Bit 3 TPGDON
This field specifies whether typical prediction for generic direct coding is used.
Bits 4-7 Reserved; must be zero.
7.4.6.3 Generic region segment AT flags
This field is only present if MMR is 0. If GBTEMPLATE is 0, it is an eight-byte field, formatted as shown in Figure 47 and as described below.
GBATX GBATY GBATX GBATY GBATX GBATY GBATX
1 1 2 2 3 3 4 GBATY4
Figure 47 ñ Generic region AT flags field structure when GBTEMPLATE is 0

---

## Page 85

ISO/IEC 14492:2001 (E)
Byte 0 GBATX1
Byte 1 GBATY1
Byte 2 GBATX2
Byte 3 GBATY2
Byte 4 GBATX3
Byte 5 GBATY3
Byte 6 GBATX4
Byte 7 GBATY4
If GBTEMPLATE is 1, 2 or 3, it is a two-byte field formatted as shown in Figure 48 and as described below. If GBTEMPLATE is 1, 2 or 3 then the values of GBATX2 through GBATX4 and GBATY2 through GBATY4 are all zero.
GBATX
1 GBATY1
Figure 48 ñ Generic region AT flags field structure when GBTEMPLATE is not 0
Byte 0 GBATX1
Byte 1 GBATY1
The AT coordinate X and Y fields are signed values, and may take on values that are permitted according to Figure 7.
7.4.6.4 Decoding a generic region segment
3) Invoke the generic region decoding procedure described in 6.2, with the parameters to the generic region decoding procedure set as shown in Table 34.
Table 34 ñ Parameters used to decode a generic region segment
MMR As shown in 7.4.6.2. GBTEMPLATE As shown in 7.4.6.2. TPGDON As shown in 7.4.6.2. USESKIP 0 GBW As specified by the region segment bitmap width in this segment's region segment data header. GBH As specified by the region segment bitmap height in this segment's region segment data header. GBATX
| 1 | See 7.4.6.3. |
|---|---|
| GBATY |  |
| 1   | See 7.4.6.3. |
| GBATX   |  |
| 2 | See 7.4.6.3. |
| GBATY |  |
| 2   | See 7.4.6.3. |
| GBATX   |  |
| 3 | See 7.4.6.3. |
| GBATY |  |
| 3   | See 7.4.6.3. |
| GBATX   |  |
| 4 | See 7.4.6.3. |
| GBATY |  |
| 4   | See 7.4.6.3. |

---

## Page 86

ISO/IEC 14492:2001 (E)
As a special case, as noted in 7.2.7, an immediate generic region segment may have an unknown length. In this case, it is also possible that the segment may contain fewer rows of bitmap data than are indicated in the segment's region segment information field.
NOTE ñ The sequence 0x00 0x00 cannot occur within MMR-encoded data; the sequence 0xFF 0xAC can occur only at the end of arithmetically-coded data. Thus, those sequences cannot occur by chance in the data that is decoded to generate the contents of the generic region.
7.4.7 Generic refinement region syntax
The data parts of all three of the generic refinement region segment types ("intermediate generic refinement region", "immediate generic refinement region" and "immediate lossless generic refinement region") are coded identically, but are acted upon differently, see 8.2. The syntax of these segment types' data parts is specified here.
7.4.7.1 Generic refinement region segment data header
The data part of a generic refinement region segment begins with a generic refinement region segment data header. This header contains the fields shown in Figure 49 and described below.
Generic refinement Generic refinement region region segment
| segment | AT flags |
|---|---|
| flags |  |
| Figure 49 ñ Generic refinement region segment data header structure |  |
Region segment information field
Region segment information field ñ see 7.4.1.
Generic refinement region segment flags ñ see 7.4.7.2.
Generic refinement region segment AT flags ñ see 7.4.7.3.
7.4.7.2 Generic refinement region segment flags
This one-byte field is formatted as shown in Figure 50 and as described below.
Reserved GRTEMP-TPGRON Must be 0 LATE
7 6 5 4 3 2 1 0
Figure 50 ñ Generic refinement region segment flags field structure
Bit 0 GRTEMPLATE
This field specifies the template used for template-based arithmetic coding.
Bit 1 TPGRON
This field specifies whether typical prediction for generic refinement is used.
Bits 2-7 Reserved; must be zero.

---

## Page 87

ISO/IEC 14492:2001 (E)
7.4.7.3 Generic refinement region segment AT flags
This field is only present if GRTEMPLATE is 0. It is a four-byte field, formatted as shown in Figure 51 and as described below.
GRATX GRATY GRATX
1 1 2 GRATY2
Figure 51 ñ Generic refinement region AT flags field structure
Byte 0 GRATX1
Byte 1 GRATY1
Byte 2 GRATX2
Byte 3 GRATY2
The AT coordinate X and Y fields are signed values, and may take on values that are permitted according to 6.3.5.3.
7.4.7.4 Reference bitmap selection
If this segment refers to another region segment, then set the reference bitmap GRREFERENCE to be the current contents of the auxiliary buffer associated with the region segment that this segment refers to.
If this segment does not refer to another region segment, set GRREFERENCE to be a bitmap containing the current contents of the page buffer (see clause 8), restricted to the area of the page buffer specified by this segment's region segment information field.
7.4.7.5 Decoding a generic refinement region segment
A generic refinement region segment is decoded according to the following steps:
NOTE ñ The requirement that the locations and external combination operators match is present to assist decoders that want to produce images of a page that is only partially decoded: it ensures that the final location and external combination operator is known for all intermediate segments. These partially-decoded page images are outside the scope of this Recommendation | International Standard.
2) As described in E.3.7, reset all the arithmetic coding statistics to zero.
3) Determine the buffer associated with the region segment that this segment refers to.
4) Invoke the generic refinement region decoding procedure described in 6.3, with the parameters to the generic refinement region decoding procedure set as shown in Table 35.
7.4.8 Page information segment syntax
A page information segment describes a page. It contains the fields shown in Figure 52 and described below.
Page bitmap width ñ see 7.4.8.1.
Page bitmap height ñ see 7.4.8.2.
Page X resolution ñ see 7.4.8.3.

---

## Page 88

# ISO/IEC 14492:2001 (E)
# Table 35 ñ Parameters used to decode a generic refinement region segment
Name Value
GRTEMPLATE As shown in 7.4.6.2. TPGRON As shown in 7.4.6.2. GRW As specified by the region segment bitmap width in this segment's region segment data header. GRH As specified by the region segment bitmap height in this segment's region segment data header. GRREFERENCE See 7.4.7.4. GRREFERENCEDX 0 GRREFERENCEDY 0 GRATX
| 1 | See 7.4.7.3. |
|---|---|
| GRATX   |  |
| 2 | See 7.4.7.3. |
| GRATY |  |
| 1   | See 7.4.7.3. |
| GRATY |  |
| 2   | See 7.4.7.3. |
Page Page segment
| Page bitmap width | Page bitmap height | Page X resolution | Page Y resolution | striping |
|---|---|---|---|---|
|  |  |  |  |  |
|  |  |  |  | information |
# Figure 52 ñ Page information segment structure
# Page Y resolution ñ see 7.4.8.4.
# Page segment flags ñ see 7.4.8.5.
# Page striping information ñ see 7.4.8.6.
# The first segment that is associated with any page must be a page information segment.
# 7.4.8.1 Page bitmap width
# This is a four-byte value containing the width in pixels of the page's bitmap.
# 7.4.8.2 Page bitmap height
# This is a four-byte value containing the height in pixels of the page's bitmap. In some cases, this value may not be known
# at the time that the page information segment is written. In this case, this field must contain 0xFFFFFFFF, and the
# actual page height may be communicated later, once it is known.
# 7.4.8.3 Page X resolution
# This is a four-byte value containing the resolution of the original page medium, measured in pixels/metre in the
# horizontal direction. If this value is unknown, then this field must contain 0x00000000.
# 7.4.8.4 Page Y resolution
# This is a four-byte value containing the resolution of the original page medium, measured in pixels/metre in the vertical
# direction. If this value is unknown, then this field must contain 0x00000000.

---

## Page 89

# ISO/IEC 14492:2001 (E)
# 7.4.8.5 Page segment flags
# This is a one-byte field. It is formatted as shown in Figure 53 and as described below.
Page Page Page Page Page default Page is Reserved combination requires default might combination eventually Must be 0 operator auxiliary pixel contain operator lossless overridden buffers value refinements
7 6 5 4 3 2 1 0
# Figure 53 ñ Page segment flags field structure
# Bit 0
# Page is eventually lossless. If this bit is 0, then the file does not contain a lossless representation of the
# original (pre-coding) page. If this bit is 1, then the file contains enough information to reconstruct the
# original page.
# Bit 1
# Page might contain refinements. If this bit is 0, then no refinement region segment may be associated with
# the page. If this bit is 1, then such segments may be associated with the page.
# Bit 2
# Page default pixel value. This bit contains the initial value for every pixel in the page, before any region
# segments are decoded or drawn.
# Bits 3-4
# Page default combination operator. This field has four possible values, representing one of four possible
# combination operators:
# 0 OR
# 1 AND
# 2 XOR
# 3 XNOR
# This operator is used to merge overlapping region segments, and also to combine region segments with
# the page default pixel value.
# Bit 5
# Page requires auxiliary buffers. If this bit is 0, then no region segment requiring an auxiliary buffer may
# be associated with the page. If this bit is 1, then such segments may be associated with the page.
# Bit 6
# Page combination operator overridden. If this bit is 0, then every direct region segment associated with
# this page must use the page's default combination operator. If this bit is 1, then direct region segments
# associated with this page may use any combination operators.
NOTE 2 ñ If all the direct region segments associated with a page use the same combination operator, then it is possible to reorder them to some extent (it is not possible to switch the relative order of any refinement segment). If some of them use different combination operators, then the decoder is unable do any such reordering. Furthermore, the decoder cannot tell from the segment headers whether any such non-default combination operators are used in the page, so this bit indicates that reordering may be possible, if the decoder wishes to perform it.
# Bit 7
# Reserved; must be 0.
# 7.4.8.6 Page striping information
# This is a two-byte field. It is formatted as shown in Figure 54 and as described below.
Page is
striped Maximum stripe size
15 14 13 12 11 10 9 8 7 6 5 4 3 2 1 0
# Figure 54 ñ Page striping information field structure

---

## Page 90

ISO/IEC 14492:2001 (E)
Bits 0-14 Maximum stripe size
Bit 15 Page is striped
If the "page is striped" bit is 1
size.
bit must be 1.
7.4.9 End of page segment syntax
of page segment associated with it.
the last end of stripe segment and the end of page segment.
7.4.10 End of stripe segment syntax
stripe.
stripe of the page.
| 7.4.11 | End of file segment syntax |
|---|---|
|  | If a file contains an end of file segment, it must be the last segment. |
7.4.12 Profiles segment syntax
compliance with each of the profiles listed.
0xFFFFFFFF) then the "page is striped"
It can then encode a
The first
, then the page may have end of stripe segments associated with it. In this case, the maximum size of each stripe (the distance between an end of stripe segment's end row and the end row of the previous end of stripe segment, or 0 in the case of the first end of stripe segment) must be no more than the page's maximum stripe
If the page's bitmap height is unknown (indicated by a page bitmap height of
An end of page segment has no associated data. Its segment data length field must be zero.
The last segment that is associated with any page must be an end of page segment. Each page must have exactly one end
If a page's height was originally unknown, then there must be at least one end of stripe segment associated with the page. In this case, the end row of that last stripe is the last row of the page bitmap and no region segment may occur between
An end of stripe segment states that the encoder has finished coding a portion of the page with which the segment is associated, and will not revisit it. It specifies the Y coordinate of a row of the page; no segment following the end of stripe may modify any portion of the page bitmap that lines on or above that row; furthermore, no segment preceding the end of stripe may modify any portion of the page bitmap that lies below that row. This row is called the "end row" of the
NOTE 1 ñ In some cases, the decoder may only have a limited amount of buffer memory for the page bitmap, smaller than the size of the page. The decoder needs to be told when it is able to output the current buffer contents and clear the buffer for the next
The end row specified by an end of stripe segment must lie below any previous end row for that page.
NOTE 2 ñ An end of stripe segment is used to communicate the size of the page in this case.
NOTE 3 ñ If a stripe boundary breaks a line of text into two parts, a top part and a bottom part, the characters straddling the stripe boundary are also broken. One way of handling these straddling characters is to code them with a generic region, or to add the top halves and bottom halves to a symbol dictionary and use those top and bottom halves as other symbols. However, if the encoder has more buffer space than the decoder, a more efficient encoding method is possible: the encoder can (temporarily) ignore the stripe boundary, and generate a list of symbol instances, and a symbol dictionary. text region in each stripe; the first text region contains all the symbol instances that affect the portion of the page above the stripe boundary; the second text region contains all the symbol instances that affect the portion below the stripe boundary. This means that some symbol instances appear twice, both times using the same symbol in the symbol dictionary. appearance encodes the top half of the character, where the bottom half is clipped off by the drawing rules used in the text region decoding procedure. The second appearance likewise encodes the bottom half of the character: the entire character is encoded, but the top half is clipped off. Thus, this encoding method reduces the amount of symbol dictionary data required.
An end of file segment has no associated data. Its segment data length field must be zero.
A profiles segment contains a list of the profiles that a given JBIG2 data stream is in compliance with. If any profiles segments are present, then the first segment of the data stream must be a profiles segment, and must not be associated with any page. Profiles of this Recommendation | International Standard are listed in Annex F.
A profiles segment begins with a four-byte field containing the number of profiles listed. This field is followed by that many four-byte fields. Each of those fields contains a profile identification number. The data stream must be in

---

## Page 91

ISO/IEC 14492:2001 (E)
More than one profiles segment may be present. If more than one is present, then each one, other than the first one, must be associated with a page. No page may have more than one profiles segment associated with it. Also, each profiles segment past the first one must be more restrictive than the first one; that is, it must list all of the profile identification numbers listed in the first segment, and possibly more. The segments making up each page must, collectively, be in compliance with each of the profiles listed in any profiles segment associated with that page.
NOTE ñ The global profiles segment allows a decoder to find out quickly that it cannot decode a given data stream. Allowing each page to contain a possibly different (though more restrictive) profiles segment eases moving pages from one file to another.
| 7.4.13 | Code table segment syntax |
|---|---|
|  | A code table segment's syntax is described in Annex B. |
| 7.4.14   | Extension segment syntax |
|  | An extension segment's data begins with a extension header: |
| Extension type |  |
| extension segment: |  |
|  | The three most significant bits of this field have special meaning: |
| Bit 29   |  |
|  |  |
|  |  |
|  | with this bit equal to   0 |
|  | Recommendation | International Standard. |
| Bit 30   |  |
|  | Dependent. If this bit is   1 |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
| Bit 31   |  |
|  | Necessary. If this bit is   1 |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  | set of colours that should be applied to the symbols on the page as they are drawn. |
| If the "necessary" bit is |   1 , then the "reserved" bit must also be |
|  |  |
|  | way particular to the type of extension. |
| 7.4.15   | Defined extension types |
|  | The following extension types are currently defined. |
| 0x20000000 |   Single-byte coded comment. See 7.4.15.1. |
| 0x20000002 |   Multi-byte coded comment. See 7.4.15.2. |
| 7.4.15.1   | Single-byte coded comment |
|  |  |
|  |  |
|  |  |
|  |  |
|  | comments applying to those segments. |

---

## Page 92

ISO/IEC 14492:2001 (E)
NOTE ñ A single-byte coded comment extension segment may contain any valid ISO/IEC 8859-1 character, including those whose values are greater than 127.
EXAMPLE ñ The comment containing the following pairs
Title An Illustrated History of False Teeth Author The Big Cheese
is stored as the following sequence of bytes. The bytes are shown as hexadecimal numbers together with their printable equivalents, with "." indicating an unprintable byte. Note the four-byte extension type at the start of the segment data:
20 00 00 00 54 69 74 6C 65 00 41 6E 20 49 6C 6C ...Title.An Ill 75 73 74 72 61 74 65 64 20 48 69 73 74 6F 72 79 ustrated History 20 6F 66 20 46 61 6C 73 65 20 54 65 65 74 68 00 of False Teeth. 41 75 74 68 6F 72 00 54 68 65 20 42 69 67 20 43 Author.The Big C 68 65 65 73 65 00 00 heese..
7.4.15.2 Multi-byte coded comment
A multi-byte coded comment extension segment is formatted in the same manner as a single-byte coded comment extension segment, except that the individual characters each occupy two bytes, in the ISO/IEC 10646-1:1993 encoding (UCS-2). Each element of each pair in the comment is terminated by a 0x0000 and the final pair is followed by an additional 0x0000.
### 8
### Page Make-up
8.1 Decoder model
This clause describes the result that a decoder conforming to this Recommendation | International Standard shall produce when decoding a page. It does this by specifying a set of steps that produce the correct result; a conforming decoder need not perform these exact steps, but shall produce the same result as if the steps had been followed.
Here we describe only the steps taken to decode a single page. A conforming decoder may operate on multiple pages at once, as long as it produces the correct final result for each page.
In the following description, we will assume for simplicity that the decoder has a single page buffer, auxiliary buffers to be used while decoding that page, and additional dictionary memory. Decoders with other components are allowed, as long as they produce the same page buffer as this abstract decoder does.
At the end of the decoding process, the page buffer contains the result of decoding the page.
Each auxiliary buffer has a location associated with it; this location is the location of the bufferís top left pixel, relative to the top left pixel of the page buffer. Some region segments require the use of auxiliary buffers; others can be decoded directly into the page buffer. See 8.2 for details on how combinations of image segments are to be interpreted.
The dictionary memory contains the information obtained by decoding dictionary segments.
8.2 Page image composition
The final bitmap for each page is coded by zero or more region segments associated with that page. Each region segment describes some of the contents of a rectangular region of the page. Since these regions of the page may overlap, and since some parts of the page might be described at multiple levels of refinement, it is important to define what the rules for region segment composition are. Also, since a decoder might want to display intermediate representations of a page, based on partial information, it is useful to suggest the interpretation of partial pages.
As described in 7.4.8, each page has a default pixel value (0 or 1) and one of four combination operators (OR, AND, XOR, XNOR); these are specified in its page information segment. Each region segment also specifies a combination operator of its own. The "page combination operator overridden" flag bit in the page information segment specifies whether any of the pageís direct region segments overrides the page combination operator. If the bit is 0, then no direct region segment associated with this page overrides the page combination operator. The decoder may use this information to optimise its decoding.
The result of decoding a region segment is a bitmap. The size of this bitmap and its location with respect to the page buffer are given in the region segment information field.

---

## Page 93

ISO/IEC 14492:2001 (E)
The final contents of the page buffer that the decoder shall produce as the final result of decoding a page are those that would be generated by the following steps:
1) Decode the page information segment.
2) Create the page buffer, of the size given in the page information segment.
If the page height is unknown, then this is not possible. However, in this case the page must be striped, and the maximum stripe height specified, and the initial page buffer can be created with height initially equal to this maximum stripe height. As each end of stripe segment is encountered, the page bufferís height can be increased, so that the last row in the new buffer is the maximum stripe height plus the end row of the previous stripe. The end of page segment (together with the last end of stripe segment) allows determination of the pageís actual height.
Alternately, when the page height is unknown, the decoder may use a fixed-size buffer whose height is equal to the pageís maximum stripe height. As each end of stripe segment is encountered, the decoder can print, or copy to some other location, all the rows in this buffer up to and including the stripeís end row, then clear the buffer in preparation for the next stripe. The decoder may follow this strategy whenever the page is striped, even if the page height is known beforehand.
NOTE 1 ñ The steps below can be followed regardless of which striping strategy is followed. The restrictions imposed by striping ensure that once an end of stripe segment is seen, no part of the page above or on that stripeís end row can be modified, and so the presentation below is phrased in terms of a page buffer that is the full size of the page, even when the pageís height is not known initially.
3) Fill the page buffer with the pageís default pixel value.
4) Fetch the next region segment associated with that page.
5) The following cases exist:
a) The region segment is an immediate direct region segment. In this case, decode the region segment. The result of decoding the region segment is a bitmap; combine this bitmap with the current contents of the page buffer, using the region segmentís combination operator.
b) The region segment is an intermediate direct region segment. In this case, allocate a new auxiliary buffer, using the size and location specified in the segmentís region segment information field. This buffer is initially associated with the region segment. Decode the region segment, placing the resulting bitmap into the auxiliary buffer.
c) The region segment is an immediate refinement region segment that refers to no other segments. In this case, the region segment is acting as a refinement of part of the page buffer. Perform the refinement according to the region segment on the part of the page buffer specified in the region segment, according to the data contained in the refinement region segment. This replaces a part of the page buffer with a refined version.
d) The region segment is an immediate refinement region segment that refers to another region segment. This other region segment must be a previously occurring intermediate region segment that has not yet had a refinement region segment refer to it. The other region segment thus has an auxiliary buffer associated with it. Note that the other region segment may itself be an intermediate refinement region segment. Perform the refinement operation on that auxiliary buffer, according to the data contained in the current region segment, and combine the resulting buffer with the page buffer using the current region segmentís combination operator, at the location associated with the auxiliary buffer. Discard the auxiliary buffer.
e) The region segment is an intermediate refinement region segment. This region segment must refer to one other region segment, which must be a previously occurring intermediate region segment that has not yet had a refinement region segment refer to it; the other region segment thus has an auxiliary buffer associated with it. Perform the refinement operation on that auxiliary buffer, according to the data contained in the current region segment. Replace the previous contents of the auxiliary buffer with the bitmap resulting from the refinement. Change the association of the auxiliary buffer, so that it is now associated with the current region segment, and is no longer associated with the other region segment.
6) Repeat steps 4) and 5) until there are no more region segments associated with the page. At this point, all auxiliary buffers that have been allocated should have been refined, drawn into the page, and discarded, as described in step 5 d); no auxiliary buffers should remain.
7) The result of decompressing that page is given by the final contents of the page buffer.

---

## Page 94

ISO/IEC 14492:2001 (E)
The rules described in step 5) are quite simple in principle. Immediate region segments are to be drawn into the page buffer, either by simply drawing them (direct segments, step 5 a), by refining a part of the page buffer (refinement segments referring to no other segments, step 5 c), or by refining and then drawing an auxiliary buffer (refinement segments referring to some other segment, step 5 d). Intermediate region segments involve creating an auxiliary buffer containing the region bitmap (direct segments, step 5 b), or replacing the current contents of an auxiliary buffer (refinement segments, step 5 e).
Some examples of these rules in operation:
EXAMPLE 1 ñ If the page contains no region segments, then the page buffer is filled entirely with the pageís default pixel value.
• Segment 6, an immediate lossless halftone region segment whose external combination operator is OR.
The resulting page bitmap can be obtained by decoding segments 3, 4 and 6, and drawing each one at its specified region location, using OR, into a bitmap initially containing 0 everywhere. Note that the order in which these three segments are decoded and drawn does not affect the resulting page bitmap. Also, if segment 3 has an internal combination operator of OR and a default pixel value of 0, then it may be drawn by simply drawing the symbol instances directly into the page buffer; it is not necessary to decode it into a temporary bitmap then draw that bitmap into the page buffer. A similar observation holds for segment 6.
• Segment 22, an immediate generic bitmap region segment whose external combination operator is OR.
6) Decode segment 19 and draw the resulting bitmap into the page buffer using OR; 7) Decode segment 22 and draw the resulting bitmap into the page buffer using OR.
The correct result is also obtained no matter what order steps 4) through 7) are performed in; thus a conforming decoder is free to choose any order to decode these steps. In fact, any order of steps 2) through 7) produces the correct result, as long as step 2) is performed before step 5) and step 3) is performed before step 4).
• drawing all the direct-coded region segments that follow the refinement region segment.

---

## Page 95

ISO/IEC 14492:2001 (E)
NOTE 2 ñ In some cases, the decoder may want to display some intermediate form of the page. For example, it may want to provide the user with a progressive display of the page contents as the page segments are received over some transmission medium. Any intermediate page bitmaps that it displays are entirely up to the decoder, and are not specified by this Recommendation | International Standard. One potential strategy a decoder could use is to take the current contents of the page buffer and any currently active auxiliary buffers, and combine all of these buffers using the pageís default combination operator, and display that to the user. If the page combination operator is XOR or XNOR, then this combination can be done reversibly, and so might be done into the actual page buffer, then undone after it has been displayed to the user. If the page combination operator is OR or AND, then this combination is not reversible and an extra buffer is required to hold the results of the combination.
The step-by-step description above is intended to specify only the results of the decompression. A conforming decoder may take any steps it desires, as long as the final page buffer is the same as would have been obtained by following the steps.
EXAMPLE 5 ñ A decoder might notice that an intermediate region segment refers to a region of the page that is not overlapped by any other region segment, and so might not actually allocate an auxiliary buffer for that region segment, but might use the page buffer immediately. It can do this only if it is sure that this will not change the final results of decoding the pageís region segments.

---

## Page 96

ISO/IEC 14492:2001 (E)
### Annex A
### Arithmetic Integer Decoding Procedure
(This annex forms an integral part of this Recommendation | International Standard)
A.1 General description
IARI Used to decode the RI bit of symbol instances
Each of these is used to decode integer values (which may include the out-of-band value OOB). The coding for an integer is based on a decision tree.
An invocation of an arithmetic integer decoding procedure involves decoding a sequence of bits, where each bit is decoded using a context formed by the bits decoded previously in this invocation. Each context for each arithmetic integer decoding procedure has its own adaptive probability estimate used by the underlying arithmetic coder, described in Annex E. The sequence of bits decoded is interpreted to form a value.
Table A.1 is used by all the arithmetic integer decoding procedures except for IAID.
A.2 Procedure for decoding values (except IAID)
• OOB if S = 1 and V = 0
Thus, V represents the absolute value of the integer value being decoded, and S represents the sign; the otherwise-redundant value ñ0 is interpreted to mean "OOB".
1) Set:
PREV = 1
2) Follow the flowchart in Figure A.1. Decode each bit with CX equal to "IAx + PREV" where "IAx" represents the identifier of the current arithmetic integer decoding procedure, "+" represents concatenation, and the rightmost 9 bits of PREV are used.

---

## Page 97

### ISO/IEC 14492:2001 (E)
INTDECODE
Decode S
| Decode a bit | 0 |
|---|---|
|  |  |
1
0
Decode a bit V = (next 4 bits) + 4
1
0 Decode a bit V = (next 6 bits) + 20
1
0
Decode a bit V = (next 8 bits) + 84
1
0 Decode a bit V = (next 12 bits) + 340
1
V = (next 32 bits) + 4436
Return V, S
T0828900-99/d18
### Figure A.1 ñ Flowchart for the integer arithmetic decoding procedures (except IAID)
### 3) After each bit is decoded: If PREV < 256 set:
# PREV = (PREV << 1) OR D
### Otherwise set:
# PREV = (((PREV << 1) OR D) AND 511) OR 256
### where D represents the value of the just-decoded bit.
### Thus, PREV always contains the values of the eight most-recently-decoded bits, plus a leading 1 bit, which is used
### to indicate the number of bits decoded so far.
### 4) The sequence of bits decoded, interpreted according to Table A.1, gives the value that is the result of this invocation
### of the integer arithmetic decoding procedure.
### Note that each type of data, and each integer arithmetic decoding procedure, uses a separate set of contexts: the contexts
### used for IAFS are separate from the contexts used for IADW, for example.

---

## Page 98

### ISO/IEC 14492:2001 (E)
### Table A.1 ñ Arithmetic integer decoding procedure table
VAL Encoding
OOB 1000
### EXAMPLE ñ An invocation of IADW might go as follows:
### • Set CX to "IADW000000001". This identifies a particular adaptive probability estimate identified. Decode a bit.
### Suppose the value decoded (D) is 0.
### • Using CX = IADW000000010, decode a bit; suppose the value decoded is 1.
### • Using CX = IADW000000101, decode a bit; suppose the value decoded is 0.
### • Using CX = IADW000001010, decode a bit; suppose the value decoded is 1.
### • Using CX = IADW000010101, decode a bit; suppose the value decoded is 0.
### • Using CX = IADW000101010, decode a bit; suppose the value decoded is 0.
### • Using CX = IADW001010100, decode a bit; suppose the value decoded is 0.
### • The sequence of bits decoded so far is 0101000. According to Table A.1 and Figure A.1, this corresponds to the
### value 12 (S = 0, V = 12), which is the result of this invocation of IADW.
### A context is identified by an arithmetic integer decoding procedure name and a sequence of nine bits. Thus, each
### arithmetic integer decoding procedure requires 512 bytes of storage for its context memory.
# A.3
# The IAID decoding procedure
### This decoding procedure is different from all the other integer arithmetic decoding procedures. It uses fixed-length
### representations of the values being decoded, and does not limit the number of previously-decoded bits used as part of the
### context. The length is equal to SBSYMCODELEN. This decoding procedure is only invoked from within the text region
### decoding procedure, so at the time of invocation SBSYMCODELEN is known.
### The procedure for decoding an integer using the IAID decoding procedure is as follows:
### 1) Set:
# PREV = 1
### 2) Decode SBSYMCODELEN bits as follows:
### a) Decode a bit with CX equal to "IAID + PREV" where "+" represents concatenation, and the rightmost
### SBSYMCODELEN + 1 bits of PREV are used.
### b) After each bit is decoded, set:
# PREV = (PREV << 1) OR D

---

## Page 99

ISO/IEC 14492:2001 (E)
where D represents the value of the just-decoded bit.
Thus, PREV always contains the values of all the bits decoded so far, plus a leading 1 bit, which is used to indicate the number of bits decoded so far.
3) After SBSYMCODELEN bits have been decoded, set:
PREV = PREV ñ 2SBSYMCODELEN
This step has the effect of clearing the topmost (leading 1) bit of PREV before returning it.
4) The contents of PREV are the result of this invocation of the IAID decoding procedure.
amount of memory needed for contexts can be calculated from the number of symbols, and is typically no more than two bytes per symbol.
EXAMPLE ñ Suppose that SBSYMCODELEN = 3. An invocation of IAID might go as follows:
• Using the adaptive probability estimate identified setting CX equal to "IAID0001", decode a bit. Suppose the value decoded is 0.
• Using CX = IAID0010, decode a bit; suppose the value decoded is 1.
• Using CX = IAID0101, decode a bit; suppose the value decoded is 0.
• At this point, PREV = 1010. Apply Step 3); PREV is now 010. Thus, the result of this invocation of the IAID decoding procedure is the value 010, or (in decimal) 2.
The context identification used here depends on the value of SBSYMCODELEN. In all cases the arithmetic coder contexts will be reset in between changes of SBSYMCODELEN: SBSYMCODELEN never changes during the decoding of a single segment (but may change between segments).

---

## Page 100

ISO/IEC 14492:2001 (E)
### Annex B
### Huffman Table Decoding Procedure
(This annex forms an integral part of this Recommendation | International Standard)
B.1 General description
Code tables may be used for encoding any type of numerical data in the Huffman variant coders. In many locations where a table is used, the encoder has the option of using one of the standard tables, or sending its own table. A code table segment provides the means to send such a custom table. The code table is a list of code table lines, each describing how to encode a single value, or a value from a specified range. A table may optionally be able to code for an OOB, which is an out-of-band signal to the decoding procedure using the table.
B.2 Code table structure
ranges are not really open-ended. There is also, potentially, an additional special table line that encodes an out-of-band value OOB.
Code table flags Code table lowest value Code table highest value First table line Second table line ∑ ∑ ∑ Last table line Lower range table line Upper range table line Out-of-band table line
Figure B.1 ñ Coded structure of a Huffman table
Each table line specifies the length of the prefix that is associated with it and the number of bits that follow that prefix to encode a value.
4) Set:
CURRANGELOW = HTLOW NTEMP = 0
b) Read HTRS bits. Let RANGELEN[NTEMP] be the value decoded.

---

## Page 101

ISO/IEC 14492:2001 (E)
c) Set:
NTEMP = NTEMP + 1
d) If CURRANGELOW ≥ HTHIGH then proceed to step 6). 6) Read HTPS bits. Let LOWPREFLEN be the value read. 7) Set:
PREFLEN[NTEMP] = LOWPREFLEN RANGELEN[NTEMP] = 32 RANGELOW[NTEMP] = HTLOW ñ 1 NTEMP = NTEMP + 1
This is the lower range table line for this table. 8) Read HTPS bits. Let HIGHPREFLEN be the value read. 9) Set:
PREFLEN[NTEMP] = HIGHPREFLEN RANGELEN[NTEMP] = 32 RANGELOW[NTEMP] = HTHIGH NTEMP = NTEMP + 1
This is the upper range table line for this table. 10) If HTOOB is 1, then: a) Read HTPS bits. Let OOBPREFLEN be the value read. b) Set:
PREFLEN[NTEMP] = OOBPREFLEN NTEMP = NTEMP + 1
This is the out-of-band table line for this table. Note that there is no range associated with this value. 11) Create the prefix codes using the algorithm described in B.3.
B.2.1 Code table flags
This one-byte field has the following bits defined:
Bit 0 HTOOB. If this bit is 1, the table can code for an out-of-band value.
Bits 1-3 Number of bits used in code table line prefix size fields. The value of HTPS is the value of this field plus one.
Bits 4-6 Number of bits used in code table line range size fields. The value of HTRS is the value of this field plus one.
Bit 7 Reserved; must be zero.
B.2.2 Code table lowest value
This signed four-byte field is the lower bound of the first table line in the encoded table.
B.2.3 Code table highest value
This signed four-byte field is one larger than the upper bound of the last normal table line in the encoded table.
B.3 Assigning the prefix codes
Given the table of prefix code lengths, PREFLEN, and the number of codes to be assigned, NTEMP, this algorithm assigns a unique prefix code to each table line, of the length given by PREFLEN for that table line.
Note that the PREFLEN value 0 indicates that the table line is never used. 1) Build a histogram in the array LENCOUNT counting the number of times each prefix length value occurs in PREFLEN: LENCOUNT[I] is the number of times that the value I occurs in the array PREFLEN.

---

## Page 102

ISO/IEC 14492:2001 (E)
2) Let LENMAX be the largest value for which LENCOUNT[LENMAX] > 0. Set:
CURLEN = 1 FIRSTCODE[0] = 0 LENCOUNT[0] = 0
a) Set:
FIRSTCODE[CURLEN] = (FIRSTCODE[CURLEN ñ 1] + LENCOUNT[CURLEN ñ 1]) × 2 CURCODE = FIRSTCODE[CURLEN] CURTEMP = 0
i) If PREFLEN[CURTEMP] = CURLEN, then set:
CODES[CURTEMP] = CURCODE CURCODE = CURCODE + 1
ii) Set CURTEMP = CURTEMP + 1 c) Set:
CURLEN = CURLEN + 1
After this algorithm has executed, then table line number I has been assigned a PREFLEN[I]-bit long code, whose value is stored in the PREFLEN[I] low-order bits of CODES[I], unless PREFLEN[I] was equal to zero, in which case that table line has not been assigned any code.
B.4 Using a Huffman table
2) Read RANGELEN[I] bits. Let HTOFFSET be the value read. 3) If HTOOB is 1 for this table, and table line I is the out-of-band table line for this table, then set:
HTVAL = OOB
4) Otherwise, if table line I is the lower range table line for this table, then set:
HTVAL = RANGELOW[I] ñ HTOFFSET
5) Otherwise, set:
HTVAL = RANGELOW[I] + HTOFFSET
The value of HTVAL is the value decoded using this table. Note that this may be a numerical value or the special value OOB.
EXAMPLE ñ The encoding for Table B.1 might be the sequence of bytes, in hexadecimal:
0x42 0x00 0x00 0x00 0x00 0x00 0x01 0x01 0x10 0x49 0x23 0x81 0x80
• The code table flags field, 0x42. This field itself breaks down into the fields, in binary, 0 100 001 0, which decode to produce the assignments:
HTOOB = 0 HTPS = 2 HTRS = 5

---

## Page 103

ISO/IEC 14492:2001 (E)
• The code table lowest value field, and the value of HTLOW, 0x00000000.
• The code table highest value field, and the value of HTHIGH, 0x00010110 (which, in decimal, is 65808).
• Three table lines, the lower range table line and the upper range table line. These are encoded as the sequence of bytes 0x49 0x23 0x81 0x80, or in binary, 01001001 00100011 10000001 10000000. This bitstring is further broken down into the table lines as follows:
01 00100 The first two (HTPS) bits of this table line indicate a prefix length of 1, and the last five (HTRS) bits of this table line indicate a range length of 4.
10 01000 This table line has a prefix length of 2 and a range length of 8.
11 10000 This table line has a prefix length of 3 and a range length of 16.
00 The lower range table line has a prefix length of 0, indicating that this table line is not used.
11 The upper range table line has a prefix length of 3.
0000000 Seven bits of padding, to fill out the last byte.
After decoding these table lines, the value of NTEMP is 5. The arrays PREFLEN, RANGELEN and RANGELOW are:
PREFLEN 1 2 3 0 3 RANGELEN 4 8 16 32 32 RANGELOW 0 16 272 ñ1 65808
Applying the algorithm of B.3 to this yields the array of codes, in binary,
CODES
| 0 | 10 110 | X 111 |
|---|---|---|
| where the X indicates that the lower range table line has not been assigned a code. Thus, the prefix code |  |  |
|   10   |  | precedes an 8-bit field encoding a value from 16 to 271, and |
|  |  |  |
|  |  |  |
|  |  |  |
| This subclause presents some standard Huffman tables that may be used in the appropriate contexts without having been |  |  |
|  |  |  |
| Each Huffman table is presented in a form that is similar to the table transmission described above. The table parameter |  |  |
| HTOOB is given (HTPS, HTRS, HTLOW and HTHIGH can be derived from the values in the table), followed by a list |  |  |
| of table lines, giving the range to which that table line applies, the table line prefix length, table line range length, and the |  |  |
| actual encoding (prefix and base value) for that table line; these table lines are followed by a lower and upper range table |  |  |
| line, and optionally (depending on HTOOB) an out-of-band table line. In some cases the lower or upper range table lines |  |  |
| are omitted from the tables as shown, indicating that these table lines are not used in the table (and would be assigned a |  |  |
|  |  |  |
| Table B.1 ñ Standard Huffman table A |  |  |
|  |  |  |
|  |  |  |
0 . . . 15 1 4 0 + VAL encoded as 4 bits 16 . . . 271 2 8 10 + (VAL ñ 16) encoded as 8 bits 272 . . . 65807 3 16 110 + (VAL ñ 272) encoded as 16 bits 65808 . . . ∞ 3 32 111 + (VAL ñ 65808) encoded as 32 bits

---

## Page 104

ISO/IEC 14492:2001 (E)
HTOOB
VAL
OOB
HTOOB
VAL
OOB
HTOOB
VAL
76 . . . ∞
Table B.2 ñ Standard Huffman table B
1
PREFLEN RANGELEN Encoding
6 111111
Table B.3 ñ Standard Huffman table C
1
PREFLEN RANGELEN Encoding
6 111110
Table B.4 ñ Standard Huffman table D
0
PREFLEN RANGELEN Encoding
5 32 11111 + (VAL ñ 76) encoded as 32 bits

---

## Page 105

HTOOB 0
VAL
76 . . . ∞
HTOOB 0
VAL
2048 . . . ∞
ISO/IEC 14492:2001 (E)
Table B.5 ñ Standard Huffman table E
PREFLEN RANGELEN Encoding
6 32 111110 + (VAL ñ 76) encoded as 32 bits
Table B.6 ñ Standard Huffman table F
PREFLEN RANGELEN Encoding
6 32 111111 + (VAL ñ 2048) encoded as 32 bits

---

## Page 106

ISO/IEC 14492:2001 (E)
HTOOB 0
VAL
2048 . . . ∞
HTOOB 1
VAL
OOB
Table B.7 ñ Standard Huffman table G
PREFLEN RANGELEN Encoding
5 32 11111 + (VAL ñ 2048) encoded as 32 bits
Table B.8 ñ Standard Huffman table H
PREFLEN RANGELEN Encoding
2 01

---

## Page 107

HTOOB 1
VAL PREFLEN
OOB 2
ISO/IEC 14492:2001 (E)
Table B.9 ñ Standard Huffman table I
RANGELEN Encoding
00

---

## Page 108

ISO/IEC 14492:2001 (E)
HTOOB
VAL
OOB
HTOOB
VAL
141 . . . ∞
Table B.10 ñ Standard Huffman table J
1
PREFLEN RANGELEN Encoding
2 10
Table B.11 ñ Standard Huffman table K
0
PREFLEN RANGELEN Encoding
7 32 1111111 + (VAL ñ 141) encoded as 32 bits

---

## Page 109

HTOOB 0
VAL
73 . . . ∞
HTOOB 0
VAL
141 . . . ∞
HTOOB 0
VAL
2
ISO/IEC 14492:2001 (E)
Table B.12 ñ Standard Huffman table L
PREFLEN RANGELEN Encoding
8 32 11111111 + (VAL ñ 73) encoded as 32 bits
Table B.13 ñ Standard Huffman table M
PREFLEN RANGELEN Encoding
7 32 1111111 + (VAL ñ 141) encoded as 32 bits
Table B.14 ñ Standard Huffman table N
PREFLEN RANGELEN Encoding
3 0 111

---

## Page 110

ISO/IEC 14492:2001 (E)
HTOOB 0
VAL PREFLEN
25 . . . ∞ 7
RANGELEN Encoding
32 1111111 + (VAL ñ 25) encoded as 32 bits
Table B.15 ñ Standard Huffman table O

---

## Page 111

### ISO/IEC 14492:2001 (E)
# Annex C
# Gray-scale Image Decoding Procedure
### (This annex forms an integral part of this Recommendation | International Standard)
# C.1
# General description
### This decoding procedure is used by the halftone region decoding procedure to produce an array of gray-scale values,
### which are then used as indexes into a dictionary of patterns.
# C.2
# Input parameters
### The parameters to this decoding procedure are shown in Table C.1.
### Table C.1 ñ Parameters for the gray-scale image decoding procedure
Size Name Type Signed? Description and restrictions (bits)
GSTEMPLATE Integer 2 N The template used to code the gray-scale bitplanes.b)
wide, GSH pixels high.a)
a) Unused if GSUSESKIP = 0. b) Unused if GSMMR = 1.
# C.3
# Return value
### The variable whose value is the result of this decoding procedure is shown in Table C.2.
### Table C.2 ñ Return value from the gray-scale image decoding procedure
Size Name Type Signed? Description and restrictions (bits)
The decoded gray-scale image. The array is GSW wide, GSH GSVALS Array high.
# C.4
# Variables used in decoding
### The variables used by this decoding procedure are shown in Table C.3.
### Table C.3 ñ Variables used in the gray-scale image decoding procedure
Size Name Type Signed? Description and restrictions (bits)
Bitplanes of the gray-scale image. There are GSBPP GSPLANES Array of bitmaps bitplanes in GSPLANES. Each bitplane is GSW pixels wide, GSH pixels high.
J
Integer 32 Y Bitplane counter.

---

## Page 112

### ISO/IEC 14492:2001 (E)
# C.5
# Decoding the gray-scale image
### The gray-scale image is obtained by decoding GSBPP bitplanes. These bitplanes are denoted (from least significant to
### most significant) GSPLANES[0], GSPLANES[1], . . . , GSPLANES[GSBPP ñ 1]. The bitplanes are Gray-coded, so that
### each bitplaneís true value is equal to its coded value XORed with the next-more-significant bitplane.
### The gray-scale image is obtained by the following procedure:
### 1) Decode GSPLANES[GSBPP ñ 1] using the generic region decoding procedure. The parameters to the generic
### region decoding procedure are as shown in Table C.4.
### Table C.4 ñ Parameters used to decode a bitplane of the gray-scale image
Name Value
GBATY
| 1 | ñ1 |
|---|---|
| GBATX |  |
| 2   | ñ3 |
| GBATY |  |
| 2   | ñ1 |
| GBATX |  |
| 3   | 2 |
| GBATY |  |
| 3   | ñ2 |
| GBATX |  |
| 4   | ñ2 |
| GBATY |  |
| 4   | ñ2 |
### 2) Set J = GSBPP ñ 2.
### 3) While J ≥ 0, perform the following steps:
### a) Decode GSPLANES[J] using the generic region decoding procedure. The parameters to the generic region
### decoding procedure are as shown in Table C.4.
### b) For each pixel (x, y) in GSPLANES[J], set:
# GSPLANES[J][x, y] = GSPLANES[J + 1][x, y] XOR GSPLANES[J][x, y]
### c) Set J = J ñ 1.
### 4) For each (x, y), set:
GSBPPñ1 GSVALS[x, y] =∑GSPLANES[J][x, y] × 2J
J=0

---

## Page 113

ISO/IEC 14492:2001 (E)
### Annex D
### File Formats
(This annex forms an integral part of this Recommendation | International Standard)
There are two standalone file organisations possible for a JBIG2 bitstream. There is also a third organisation, not intended for standalone usage, but instead to allow JBIG2-encoded data to be embedded in another file format.
NOTE ñ It is recommended that ".jbig2" is used as the extension for JBIG2 files. In environments where only three characters are allowed, ".jb2" is recommended. It is also recommended that JBIG2 decoders recognise both extensions.
D.1 Sequential organisation
This is a standalone file organisation. This organisation is intended for streaming applications, where the decoder is guaranteed to begin at the start of the bitstream and decode everything up to the end of the bitstream.
In this organisation, the file structure looks like Figure D.1. A file header is followed by a sequence of segments. The two parts of each segment are stored together: first the segment header then the segment data.
The segments must appear in increasing order of their segment numbers: no segment may precede a segment having a lower number than it.
File header Segment 1 segment header Segment 1 data Segment 2 segment header Segment 2 data . . . Segment N segment header Segment N data
Figure D.1 ñ Sequential organisation
D.2 Random-access organisation
This is a standalone file organisation. This organisation is intended for random-access applications, where the decoder might want to process parts of the file in an arbitrary order, such as decoding all the odd-numbered pages before any even-numbered page, or decode pages individually in response to some user input. The ability to perform random access is therefore important.
In this organisation, the file structure looks like Figure D.2. A file header is followed by a sequence of segments headers; the last segment header is followed by the data for the first segment, then the data for the second segment, and so on. The last segment must be an end of file segment; otherwise, it is impossible for the decoder to determine when it has read the last segment header.
The segments must appear in increasing order of their segment numbers: no segment may precede a segment having a lower number than it.
File header Segment 1 segment header Segment 2 segment header . . . Segment N segment header Segment 1 data Segment 2 data . . . Segment N data
Figure D.2 ñ Random-access organisation

---

## Page 114

ISO/IEC 14492:2001 (E)
D.3 Embedded organisation
This is not a standalone file organisation, but relies on some other file format to carry the JBIG2 segments. Each segment is stored by concatenating its segment header and segment data parts, but there is no defined storage order for these segments. The embedding file format is allowed to store those segments in any order, and may separate them by arbitrary data.
NOTE ñ The intent of the embedded organisation is that many current systems can benefit from incorporating improved bi-level image compression. However, the best way to do this is not always to incorporate an entire JBIG2 bitstream as a monolithic entity, as this can conflict with other constraints. For example, the system might have its own ideas of how pages must be divided up, which might not agree with JBIG2ís ideas. Thus, JBIG2 is flexible in allowing the embedding system to store JBIG2 data in whatever way is most convenient.
D.4 File header syntax
A file header contains the following fields, in order:
ID string ñ see D.4.1.
File header flags ñ see D.4.2.
Number of pages ñ see D.4.3.
D.4.1 ID string
NOTE ñ This is similar to the PNG ID string. The first character is non-printable, so that the file cannot be mistaken for ASCII. The first characterís high bit is set, to detect passing through a 7-bit channel. The next three bytes are JB2, and are intended to allow a human looking at the header to guess the file type. The following bytes are CR LF CONTROL-Z LF; any corruption by CR/LF translation and DOS file truncation can be detected immediately.
D.4.2 File header flags
This is a 1-byte field. The bits that are defined are:
NOTE ñ Note that there is no way to indicate the embedded organisation, as that organisation does not include a JBIG2 file header.
Bit 1 Unknown number of pages. If this bit is 0, then the number of pages contained in the file is known. If this bit is 1, then the number of pages contained in the file was not known at the time that the file header was coded.
Bits 2-7 Reserved; must be 0.
D.4.3 Number of pages
This is a 4-byte field, and is not present if the "unknown number of pages" bit was 1. If present, it must equal the number of pages contained in the file.

---

## Page 115

ISO/IEC 14492:2001 (E)
### Annex E
### Arithmetic Coding
(This annex forms an integral part of this Recommendation | International Standard)
An adaptive binary arithmetic coder may be used as the entropy coder when allowed by the models. The models used with adaptive binary arithmetic coding are defined in 6.2, 6.3 and Annex A. In this annex the basic arithmetic coding procedures are defined.
In this annex and all of its subclauses, the flow charts and tables are normative only in the sense that they are defining an output that alternative implementations shall duplicate. In H.2 a simple test example is given which should be helpful in determining if a given implementation is correct.
E.1 Binary encoding
Figure E.1 shows a simple block diagram of the binary adaptive arithmetic encoder. The decision (D) and context (CX) pairs are processed together to produce compressed data (CD) output. Both D and CX are provided by the model unit (not shown). CX selects the probability estimate to use during the coding of D. In this Recommendation | International Standard, CX is a label for a context, formed by some character string followed by a string of bits.
EXAMPLE ñ Two possible values of CX are "IADW001010100" and "GB1110110010000000".
D
CX
T0828910-99/d19
Figure E.1 ñ Arithmetic encoder inputs and outputs
E.1.1 Recursive interval subdivision
The recursive probability interval subdivision of Elias coding is the basis for the binary arithmetic coding process. With each binary decision the current probability interval is subdivided into two sub-intervals, and the code string is modified (if necessary) so that it points to the base (the lower bound) of the probability sub-interval assigned to the symbol which occurred.
In the partitioning of the current interval into two sub-intervals, the sub-interval for the more probable symbol (MPS) is ordered above the sub-interval for the less probable symbol (LPS). Therefore, when the MPS is coded, the LPS sub-interval is added to the code string. This coding convention requires that symbols be recognised as either MPS or LPS, rather than 0 or 1. Consequently, the size of the LPS interval and the sense of the MPS for each decision must be known in order to code that decision.
Since the code string always points to the base of the current interval, the decoding process is a matter of determining, for each decision, which sub-interval is pointed to by the code string. This is also done recursively, using the same interval sub-division process as in the encoder. Each time a decision is decoded, the decoder subtracts any interval the encoder added to the code string. Therefore, the code string in the decoder is a pointer into the current interval relative to the base of the current interval. Since the coding process involves addition of binary fractions rather than concatenation of integer code words, the more probable binary decisions can often be coded at a cost of much less than one bit per decision.
E.1.2 Coding conventions and approximations
The coding operations are done using fixed precision integer arithmetic and using an integer representation of fractional values in which 0x8000 is equivalent to decimal 0.75. The interval A is kept in the range 0.75 ≤ A < 1.5 by doubling it whenever the integer value falls below 0x8000.
The code register C is also doubled each time A is doubled. Periodically, to keep C from overflowing, a byte of data is removed from the high order bits of the C-register and placed in an external compressed data buffer. Carry-over into the external buffer is resolved by a bit-stuffing procedure.

---

## Page 116

ISO/IEC 14492:2001 (E)
A ñ (Qe × A) = sub-interval for the MPS Qe × A = sub-interval for the LPS
Because the value of A is of order unity, these are approximated by:
A ñ Qe = sub-interval for the MPS Qe = sub-interval for the LPS
Whenever the MPS is coded, the value of Qe is added to the code register and the interval is reduced to A ñ Qe. Whenever the LPS is coded, the code register is left unchanged and the interval is reduced to Qe. The precision range required for A is then restored, if necessary, by renormalisation of both A and C.
With the process illustrated above, the approximations in the interval subdivision process can sometimes make the LPS sub-interval larger than the MPS sub-interval. If, for example, the value of Qe is 0.5 and A is at the minimum allowed value of 0.75, the approximate scaling gives 1/3 of the interval to the MPS and 2/3 to the LPS. To avoid this size inversion, the MPS and LPS intervals are exchanged whenever the LPS interval is larger than the MPS interval. This MPS/LPS conditional exchange can only occur when a renormalisation is needed.
Whenever a renormalisation occurs, a probability estimation process is invoked which determines a new probability estimate for the context currently being coded. No explicit symbol counts are needed for the estimation. The relative probabilities of renormalisation after coding an LPS and MPS provide an approximate symbol counting mechanism which is used to directly estimate the probabilities.
E.2 Description of the arithmetic encoder
The ENCODER (Figure E.2) initialises the encoder through the INITENC procedure. CX and D pairs are read and passed on to ENCODE until all pairs have been read. The probability estimation procedures which provide adaptive estimates of the probability for each context are embedded in ENCODE. Bytes of compressed data are output when no longer modifiable. When all of the CX and D pairs have been read (Finished?), FLUSH sets the contents of the C-register to as many 1-bits as possible and then outputs the final bytes. FLUSH also terminates the encoding operations and generates the required terminating marker.
ENCODER
INITENC
Read CX, D
ENCODE
No
Finished?
Yes
FLUSH
T0828920-99/d20
Figure E.2 ñ Encoder for the MQ-coder

---

## Page 117

ISO/IEC 14492:2001 (E)
E.2.1 Encoder code register conventions
The flow charts given in this annex assume the following register structures for the encoder:
0000cbbb bbbbbsss xxxxxxxx
| C-register | xxxxxxxx |
|---|---|
|  |  |
|  |  |
|  |   |
| A-register   | aaaaaaaa |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
| Encoding a decision (ENCODE) |  |
|  |  |
|  |  |
| procedures directly to code a 0-decision or a 1-decision. |  |
|  |  |
| Encoding a 1 or 0 (CODE1 and CODE0) |  |
|  |  |
|  |  |
|  |  |
| value are stored. MPS(CX) is the sense ( |  |
ENCODE
No Yes D = 0?
CODE1 CODE0
T0828930-99/d21
Done
Figure E.3 ñ ENCODE procedure
CODE1
No Yes MPS(CX) = 1?
CODELPS CODEMPS
T0828950-99/d22
Done
Figure E.4 ñ CODE1 procedure

---

## Page 118

# ISO/IEC 14492:2001 (E)
CODE0
No Yes MPS(CX) = 0?
CODELPS CODEMPS
T0828950-99/d23
Done
# Figure E.5 ñ CODE0 procedure
# E.2.4
# Encoding an MPS or LPS (CODEMPS and CODELPS)
# The CODELPS (Figure E.6) procedure usually consists of a scaling of the interval to Qe(I(CX)), the probability estimate
# of the LPS determined from the index I stored for context CX. The upper interval is first calculated so it can be compared
# to the lower interval to confirm that Qe has the smaller size. It is always followed by a renormalisation (RENORME). In
# the event that the interval sizes are inverted, however, the conditional MPS/LPS exchange occurs and the upper interval
# is coded. In either case, the probability estimate is updated. If the SWITCH flag for the index I(CX) is set, then the
# MPS(CX) is inverted. A new index I is saved at CX as determined from the next LPS index (NLPS) column in
# Table E.1.
CODELPS
A = A ñ Qe(I(CX))
No Yes A < Qe(I(CX))?
A = Qe(I(CX)) C = C + Qe(I(CX))
Yes SWITCH(I(CX)) = 1?
No
MPS(CX) = 1 ñ MPS(CX)
I(CX) = NLPS(I(CX))
RENORME
Done
T0828960-99/d24
# Figure E.6 ñ CODELPS procedure with conditional MPS/LPS exchange
# The CODEMPS (Figure E.7) procedure usually reduces the size of the interval to the MPS sub-interval and adjusts the
# code register so that it points to the base of the MPS sub-interval. However, if the interval sizes are inverted, the LPS
# sub-interval is coded instead. Note that the size inversion cannot occur unless a renormalisation (RENORME) is required
# after the coding of the symbol. The probability estimate update changes the index I(CX) according to the next MPS
# index (NMPS) column in Table E.1.

---

## Page 119

ISO/IEC 14492:2001 (E)
CODEMPS
A = A ñ Qe(I(CX))
No Yes A AND 0x8000 = 0?
No Yes C = C + Qe(I(CX)) A < Qe(I(CX))?
C = C + Qe(I(CX)) A = Qe(I(CX))
I(CX) = NMPS(I(CX))
RENORME
Done
T0828970-99/d25
Figure E.7 ñ CODEMPS procedure with conditional MPS/LPS exchange
E.2.5 Probability estimation
Table E.1 shows the Qe value associated with each Qe index. The Qe values are expressed as hexadecimal integers, as binary integers, and as decimal fractions. To convert the 15-bit integer representation of Qe to the decimal probability, the Qe values are divided by (4/3) × (0x8000).
The estimator can be defined as a finite-state machine ñ a table of Qe indexes and associated next states for each type of renormalisation (i.e. new table positions) ñ as shown in Table E.1. The change in state occurs only when the arithmetic coder interval register is renormalised. This is always done after coding the LPS, and whenever the interval register is less than 0x8000 (0.75 in decimal notation) after coding the MPS.
After an LPS renormalisation, NLPS gives the new index for the LPS probability estimate; also, if Switch is 1, the MPS symbol sense is reversed. After an MPS renormalisation, NMPS gives the new index for the LPS probability estimate.
The index to the current estimate is part of the information stored for context CX. This index is used as the index to the table of values in NMPS, which gives the next index for an MPS renormalisation. This index is saved in the context storage at CX. MPS(CX) does not change.
The procedure for estimating the probability on the LPS renormalisation path is similar to that of an MPS renormalisation, except that when Switch(I(CX)) is 1, the sense of MPS(CX) is inverted.
The final index state 46 can be used to establish a fixed 0.5 probability estimate.
E.2.6 Renormalisation in the encoder (RENORME)
Renormalisation is very similar in both encoder and decoder, except that in the encoder it generates compressed bits and in the decoder it consumes compressed bits.
The RENORME procedure for the encoder renormalisation is illustrated in Figure E.8. Both the interval register A and the code register C are shifted, one bit at a time. The number of shifts is counted in the counter CT, and when CT is counted down to zero, a byte of compressed data is removed from C by the procedure BYTEOUT. Renormalisation continues until A is no longer less than 0x8000.

---

## Page 120

ISO/IEC 14492:2001 (E)
Qe_Value
(hexadecimal) (binary)
Index
0 0x5601 0101011000000001 1 0x3401 0011010000000001 2 0x1801 0001100000000001 3 0x0AC1 0000101011000001 4 0x0521 0000010100100001 5 0x0221 0000001000100001 6 0x5601 0101011000000001 7 0x5401 0101010000000001 8 0x4801 0100100000000001 9 0x3801 0011100000000001 10 0x3001 0011000000000001 11 0x2401 0010010000000001 12 0x1C01 0001110000000001 13 0x1601 0001011000000001 14 0x5601 0101011000000001 15 0x5401 0101010000000001 16 0x5101 0101000100000001 17 0x4801 0100100000000001 18 0x3801 0011100000000001 19 0x3401 0011010000000001 20 0x3001 0011000000000001 21 0x2801 0010100000000001 22 0x2401 0010010000000001 23 0x2201 0010001000000001 24 0x1C01 0001110000000001 25 0x1801 0001100000000001 26 0x1601 0001011000000001 27 0x1401 0001010000000001 28 0x1201 0001001000000001 29 0x1101 0001000100000001 30 0x0AC1 0000101011000001 31 0x09C1 0000100111000001 32 0x08A1 0000100010100001 33 0x0521 0000010100100001 34 0x0441 0000010001000001 35 0x02A1 0000001010100001 36 0x0221 0000001000100001 37 0x0141 0000000101000001 38 0x0111 0000000100010001 39 0x0085 0000000010000101 40 0x0049 0000000001001001 41 0x0025 0000000000100101 42 0x0015 0000000000010101 43 0x0009 0000000000001001 44 0x0005 0000000000000101 45 0x0001 0000000000000001 46 0x5601 0101011000000001
E.2.7 Compressed data output (BYTEOUT)
buffer.
is for the case where bit stuffing is not needed.
NMPS NLPS SWITCH
(decimal)
0.503937 1 1 1 0.304715 2 6 0 0.140650 3 9 0 0.063012 4 12 0 0.030053 5 29 0 0.012474 38 33 0 0.503937 7 6 1 0.492218 8 14 0 0.421904 9 14 0 0.328153 10 14 0 0.281277 11 17 0 0.210964 12 18 0 0.164088 13 20 0 0.128931 29 21 0 0.503937 15 14 1 0.492218 16 14 0 0.474640 17 15 0 0.421904 18 16 0 0.328153 19 17 0 0.304715 20 18 0 0.281277 21 19 0 0.234401 22 19 0 0.210964 23 20 0 0.199245 24 21 0 0.164088 25 22 0 0.140650 26 23 0 0.128931 27 24 0 0.117212 28 25 0 0.105493 29 26 0 0.099634 30 27 0 0.063012 31 28 0 0.057153 32 29 0 0.050561 33 30 0 0.030053 34 31 0 0.024926 35 32 0 0.015404 36 33 0 0.012474 37 34 0 0.007347 38 35 0 0.006249 39 36 0 0.003044 40 37 0 0.001671 41 38 0 0.000847 42 39 0 0.000481 43 40 0 0.000206 44 41 0 0.000114 45 42 0 0.000023 45 43 0 0.503937 46 46 0
0xFF byte; the similar procedure on the left
Table E.1 ñ Qe values and probability estimation process
The BYTEOUT routine called from RENORME is illustrated in Figure E.9. This routine contains the bit-stuffing procedures which are needed to limit carry propagation into the completed bytes of compressed data. The conventions used make it impossible for a carry to propagate through more than the byte most recently written to the compressed data
The procedure in the block in the lower right section does bit stuffing after a

---

## Page 121

ISO/IEC 14492:2001 (E)
B is the byte pointed to by the compressed data buffer pointer BP. If B is not a 0xFF byte, the carry bit is checked. If the carry bit is set, it is added to B and B is again checked to see if a bit needs to be stuffed in the next byte. After the need for bit stuffing has been determined, the appropriate path is chosen, BP is incremented and the new value of B is removed from the code register "b" bits.
RENORME
A = A << 1 C = C << 1 CT = CT ñ 1
No
CT = 0?
Yes
BYTEOUT
Yes
No
Done
T0828980-99/d26
Figure E.8 ñ Encoder renormalisation procedure
E.2.8 Initialisation of the encoder (INITENC)
The INITENC procedure is used to start the arithmetic coder. The basic steps are shown in Figure E.10.
The interval register and code register are set to their initial values, and the bit counter is set. Setting CT = 12 reflects the fact that there are three spacer bits in the register which need to be filled before the field from which the bytes are removed is reached. Note that BP always points to the byte preceding the position BPST where the first byte is placed. Therefore, if the preceding byte is a 0xFF byte, a spurious bit stuff will occur, but can be compensated for by increasing CT. Note that the default initialisation of the statistics bins is MPS = 0 and I = 0 (i.e. Qe = 0x5601 or decimal 0.503937).
E.2.9 Termination of encoding (FLUSH)
The FLUSH procedure shown in Figure E.11 is used to terminate the encoding operations and generate the required terminating marker. The procedure guarantees that the 0xFF prefix to the marker code overlaps the final bits of the compressed data. This guarantees that any marker code at the end of the compressed data will be recognized and interpreted before decoding is complete.
The first part of the FLUSH procedure sets as many bits in the C-register to 1 as possible as shown in Figure E.12. The exclusive upper bound for the C-register is the sum of the C-register and the interval register. The low order 16 bits of C are forced to 1, and the result is compared to the upper bound. If C is too big, the leading 1-bit is removed, reducing C to a value which is within the interval.
The byte in the C-register is then completed by shifting C, and two bytes are then removed. If the second byte is not 0xFF, another byte is added to the compressed data which is guaranteed to be 0xFF.
| E.2.10 | Minimisation of the compressed data |
|---|---|
|  |  |
|  | generated by the arithmetic coder, bit stuffing will produce pairs of |
|  | from the compressed data, provided that the earliest   |
| 0xFF   | byte then becomes the prefix to the marker code which terminates the compressed data. |

---

## Page 122

## ISO/IEC 14492:2001 (E)
## Decoding is not affected by this trimming process because the convention is used in the decoder that when a marker code
## is encountered, 1-bits (without bit stuffing) are supplied to the decoder until the coding interval is complete.
BYTEOUT
Yes
B = 0xFF?
No
Yes
C < 0x8000000?
No
B = B + 1
No
B = 0xFF?
Yes
C = C AND 0x7FFFFFF
BP = BP + 1 BP = BP + 1 B = C >> 19 B = C >> 20 C = C AND 0x7FFFF C = C AND 0xFFFFF CT = 8 CT = 7
T0828990-99/d27
## Figure E.9 ñ BYTEOUT procedure for encoder
INITENC
A = 0x8000 C = 0 BP = BPST ñ 1 CT =12
No
B = 0xFF?
Yes
CT = 13
T0829000-99/d28
## Figure E.10 ñ Initialisation of the encoder

---

## Page 123

### ISO/IEC 14492:2001 (E)
# E.3
# Arithmetic decoding procedure
### Figure E.13 shows a simple block diagram of a binary adaptive arithmetic decoder. The compressed data CD and a
### context CX from the decoderís model unit (not shown) are input to the arithmetic decoder. The decoderís output is the
### decision D. The encoder and decoder model units need to supply exactly the same context CX for each given decision.
### The DECODER (Figure E.14) initialises the decoder through INITDEC. Contexts, CX, and bytes of compressed data (as
### needed) are read and passed on to DECODE until all contexts have been read. The DECODE routine decodes the binary
### decision D and returns a value of either 0 or 1. The probability estimation procedures which provide adaptive estimates
### of the probability for each context are embedded in DECODE. When all contexts have been read (Finished?), the
### compressed data has been decompressed.
FLUSH
SETBITS
C = C << CT
BYTEOUT
C = C << CT
BYTEOUT
Yes
B = 0xFF?
No
BP = BP + 1 B = 0xFF
Optionally remove trailing 0x7FFF pairs following the leading 0xFF
BP = BP + 1 B = 0xAC BP = BP + 1
Done
T0829010-99/d29
### Figure E.11 ñ FLUSH procedure

---

## Page 124

## ISO/IEC 14492:2001 (E)
SETBITS
TEMPC = C + A C = C OR 0xFFFF
No
C ≥TEMPC?
Yes
C = C ñ 0x8000
Done
T0829020-99/d30
## Figure E.12 ñ Setting the final bits in the C register
CD
CX
DECODER D
T0829030-99/d31
## Figure E.13 ñ Arithmetic decoder inputs and outputs
DECODER
INITDEC
Read CX
D = DECODE
No
Finished?
Yes
Done
T0829040-99/d32
## Figure E.14 ñ Decoder for the MQ-coder

---

## Page 125

ISO/IEC 14492:2001 (E)
E.3.1 Decoder code register conventions
The flow charts given in this annex assume the following register structures for the decoder:
xxxxxxxx
| Chigh register | xxxxxxxx |
|---|---|
|  |   |
| Clow register   | 00000000 |
|  |   |
| A-register   | aaaaaaaa |
|  |  |
|  |  |
|  |  |
| The detailed description of the handling of data with stuff-bits will be given later in this subclause. |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
| Renormalisation in the decoder (RENORMD) |  |
|  |  |
|  |  |
|  |  |
| Both the interval register A and the code register C are shifted, one bit at a time, until A is no longer less than |  |
|  |  |
| Compressed data input (BYTEIN) |  |
|  |  |
|   0xFF   | byte in the process. It also detects the marker codes which must |
|  |  |
|  |  |
| B is the byte pointed to by the compressed data buffer pointer BP. If B is not a |   0xFF   |
| new value of B is inserted into the high order 8 bits of Clow. |  |
| byte, then B1 (the byte pointed to by BP | 1) is tested. If B1 exceeds 0x8F, then B1 must be one of the |
| marker codes. The marker code is interpreted as required, and the buffer pointer remains pointed to the |  |
|  |  |
|   0xFF00   | to the C-register and setting the bit counter CT to 8. |

---

## Page 126

# ISO/IEC 14492:2001 (E)
# If B1 is not a marker code, then BP is incremented to point to the next byte which contains a stuffed bit. The B is added
# to the C-register with an alignment such that the stuff bit (which contains any carry) is added to the low order bit of
# Chigh.
DECODE
A = A ñ Qe(I(CX))
| No | Yes |
|---|---|
|  |  |
Chigh = Chigh ñ Qe(I(CX))
No
A AND0x8000= 0?
Yes
D = D = MPS(CX)
MPS_EXCHANGE D = LPS_EXCHANGE
RENORMD RENORMD
T0829050-99/d33
Return D
# Figure E.15 ñ Decoding an MPS or an LPS
MPS_EXCHANGE
| No | Yes |
|---|---|
|  |  |
D = MPS(CX) I(CX) = NMPS(I(CX))
D = 1 ñ MPS(CX)
Yes
SWITCH(I(CX)) = 1?
No
MPS(CX) = 1 ñ MPS(CX)
I(CX) = NLPS(I(CX))
T0829060-99/d34
Return D
# Figure E.16 ñ Decoder MPS path conditional exchange procedure

---

## Page 127

## ISO/IEC 14492:2001 (E)
LPS_EXCHANGE
Yes No A < Qe(I(CX))?
A = Qe(I(CX)) D = MPS(CX) I(CX) = NMPS(I(CX))
A = Qe(I(CX)) D = 1 ñ MPS(CX)
Yes SWITCH(I(CX)) = 1?
No
MPS(CX) = 1 ñ MPS(CX)
I(CX) = NLPS(I(CX))
T0829070-99/d35
Return D
## Figure E.17 ñ Decoder LPS path conditional exchange procedure
RENORMD
No
CT = 0?
Yes
BYTEIN
A = A << 1 C = C << 1 CT = CT ñ 1
Yes
A AND 0x8000 = 0?
No
Done
T0829080-99/d36
## Figure E.18 ñ Decoder renormalisation procedure

---

## Page 128

# ISO/IEC 14492:2001 (E)
BYTEIN
Yes No B = 0xFF?
| No | Yes |
|---|---|
| B1 >   | ? |
BP = BP + 1 C = C + (B << 9) CT = 7
C = C + 0xFF00 CT = 8
Done
# Figure E.19 ñ BYTEIN procedure for decoder
# E.3.5
# Initialisation of the decoder (INITDEC)
INITDEC
BP = BPST C = B << 16
BYTEIN
C = C << 7 CT = CT ñ 7 A = 0x8000
Done
T0829100-99/d38
# Figure E.20 ñ Initialisation of the decoder
# register A is set to match the starting value in the encoder.
# E.3.6
# Resynchronisation of the decoder
# the 0xFF
# 0xFF
# is not standardised.
BP = BP + 1 C = C + (B << 8) CT = 8
T0829090-99/d37
# The INITDEC procedure is used to start the arithmetic decoder. The basic steps are shown in Figure E.20.
# BP, the pointer to the compressed data, is initialised to BPST (pointing to the first compressed byte). The first byte of the
# compressed data is shifted into the low order byte of Chigh, and a new byte is then read in. The C-register is then shifted
# by 7 bits and CT is decremented by 7, bringing the C-register into alignment with the starting value of A. The interval
# Usually, when the end of the arithmetically compressed data is reached, the compressed data buffer pointer BP points to
# byte of the terminating marker code. If for any reason the compressed data buffer pointer is not at the
# byte of the marker, a resynchronisation procedure needs to scan the compressed data until it finds the terminating
# marker code prefix. If a search of this type is needed, it is indicative of an error condition. This error recovery procedure

---

## Page 129

ISO/IEC 14492:2001 (E)
E.3.7 Resetting arithmetic coding statistics
At certain points during the decoding, some or all of the arithmetic coding statistics are reset. This process involves setting I(CX) and MPS(CX) equal to zero for some or all values of CX.
EXAMPLE ñ At the start of decoding a text region segment, all the arithmetic coding statistics are reset.
E.3.8 Saving arithmetic coding statistics
In some cases, the decoder needs to save or restore some values of I(CX) and MPS(CX). This is done as part of decoding a symbol dictionary segment. In this case, the values that are saved and/or restored are all the values indexed by CX values whose initial label is "GB" or "GR" (i.e. all those CX values used by the generic region decoding procedure or the generic refinement region decoding procedure).

---

## Page 130

# ISO/IEC 14492:2001 (E)
# Annex F
# Profiles
# (This annex forms an integral part of this Recommendation | International Standard)
# It is recommended that a JBIG2 decoder implement one of the profiles described in Tables F.1 through F.7. Note that
# profile 0x00000001 (Table F.1) includes all the capabilities of this entire Recommendation | International Standard,
# and is the profile assumed when none is explicitly specified.
# See 7.4.12 for information on how the profile identification numbers are used.
# Profile identification numbers 0x00000000 through 0x00FFFFFF are reserved for ISO/IEC and ITU-T applications.
# Of this range, profile identification numbers 0x00000100 through 0x00000FFF are reserved for ITU-T facsimile
# applications. Entities other than ISO/IEC and ITU-T wishing to use an unassigned profile identification number should
# choose one in the range 0x01000000 through 0xFFFFFFFF that is not likely to conflict with any other entity's choice.
# It is recommended that the first three bytes of the profile identification number be chosen to match the first three letters
# of the name of the entity, or be a suitable abbreviation of that name.
# Table F.1 ñ Profile description for profile 0x00000001
Application examples General-purpose printing; Format conversion
# Table F.2 ñ Profile description for profile 0x00000002
Application examples Archiving; Wireless WWW

---

## Page 131

## ISO/IEC 14492:2001 (E)
## Table F.3 ñ Profile description for profile 0x00000003
NOTE 1 ñ This profile is a subset of profile 0x00000002.
Application examples WWW; High-end fax
## Table F.4 ñ Profile description for profile 0x00000004
Application examples WWW
## Table F.5 ñ Profile description for profile 0x00000005
NOTE 2 ñ This profile is a subset of profile 0x00000004.
Application examples Low-end fax; High-speed printing; Embedded processors

---

## Page 132

## ISO/IEC 14492:2001 (E)
## Table F.6 ñ Profile description for profile 0x00000006
NOTE 3 ñ This profile is a subset of profile 0x00000003.
Additional constraints • Every page must have at least two stripes • A stripe that contains a text region segment may not contain any halftone or generic region segments. • All region segments must be immediate region segments (no auxiliary buffers). • In any symbol dictionary segment with SDREFAGG equal to 1, REFAGGNINST must be equal to 1 for all symbols.
## Table F.7 ñ Profile description for profile 0x00000007
NOTE 4 ñ This profile is a subset of profile 0x00000005.
• Α stripe that contains a text region segment may not contain any halftone or generic region segments. • All region segments must be immediate region segments (no auxiliary buffers). • In any symbol dictionary segment with SDREFAGG equal to 1, REFAGGNINST must be equal to 1 for all symbols.

---

## Page 133

ISO/IEC 14492:2001 (E)
## Annex G
## Arithmetic Decoding Procedure (Software Conventions)
(This annex does not form an integral part of this Recommendation | International Standard)
This annex provides some alternative flowcharts for a version of the adaptive entropy decoder. This alternative version may be more efficient when implemented in software, as it has fewer operations along the fast path.
• Replace the flowchart in Figure E.19 with the flowchart in Figure G.3.
INITDEC
BP = BPST C = (B XOR 0xFF) << 16
BYTEIN
C = C << 7 CT = CT ñ 7 A = 0x8000
Done
T0829110-99/d39
Figure G.1 ñ Initialisation of the software conventions decoder
FIGURE G.1/T088 [D39]

---

## Page 134

## ISO/IEC 14492:2001 (E)
DECODE
A = A ñ Qe(I(CX))
Yes No Chigh < A?
Chigh = Chigh ñ A
No
A AND 0x8000 = 0?
Yes
D =
MPS_EXCHANGE D = MPS(CX) D = LPS_EXCHANGE
RENORMD RENORMD
T0829120-99/d40
Return D
## Figure G.2 ñ Decoding an MPS or an LPS in the software-conventions decoder
BYTEIN
| Yes | No |
|---|---|
|  | ? |
No Yes B1 > 0x 8F?
| BP = BP + 1 | BP = BP + 1 |
|---|---|
| 0xFE00   ñ (B << 9) |  |
|  |  |
C = C + C = C + 0xFF00 ñ (B << 8) CT = 7 CT = 8
T0829130-99/d41
Done
## Figure G.3 ñ Inserting a new byte into the C register in the software-conventions decoder

---

## Page 135

ISO/IEC 14492:2001 (E)
### Annex H
### Datastream Example and Test Sequence
(This annex does not form an integral part of this Recommendation | International Standard)
H.1 Datastream example
This subclause gives a small datastream that exercises a large number of the features of JBIG2.
The raw data here is shown in the following format: 0023: 01 23 45 67 89 AB CD EF
where the first field (before the colon) is the byte offset into the datastream of the data being displayed, and the remainder of the line is a sequence of bytes starting at that offset. All these values are in hexadecimal.
In general, the decoding of the first occurrence of any type of data is explained in detail; further occurrences of the same type of data are not explained in as much detail.
• one symbol dictionary that is not associated with any page; and
| • | three pages, |
|---|---|
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
| correctly. |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  | Recommendation | International Standard. |
|  | The datastream is: |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |
|  |  |

---

## Page 136

ISO/IEC 14492:2001 (E)
The datastream is decoded as follows: 1) The file header: 0000: 97 4A 42 32 0D 0A 1A 0A 01 00 00 00 02 a) The eight-byte ID string: 0000: 97 4A 42 32 0D 0A 1A 0A b) The one-byte file header flags field: 0008: 01 This field indicates that the file uses the sequential organisation, and that the number of pages is known. c) The four-byte number of pages field: 0009: 00 00 00 03 This field indicates that the file has three pages. 2) The first segment header: 000D: 00 00 00 00 00 01 00 00 00 00 18 a) The four-byte segment number field: 000D: 00 00 00 00 This field indicates that the segment is segment number 0. b) The one-byte segment header flags field: 0011: 00 This field indicates that the segment has type "Symbol dictionary" (type 0), has a short page association field, and does not have the deferred non-retain bit set. c) The one-byte referred-to segment count and retention flags field: 0012: 01 This field indicates that the segment refers to no other segment, and that it should be retained. d) The one-byte (short-form) segment page association field: 0013: 00 This field indicates that this segment is not associated with any page. e) The four-byte segment data length field: 0014: 00 00 00 18 This field indicates that the segment's data part is 24 bytes long.

---

## Page 137

ISO/IEC 14492:2001 (E)
T0829140-99/d42
Figure H.1 ñ Test datastream page bitmap
0028: 04 BF F0 78 2F E0 00 40
0018: 00 01
This field indicates that the segment is encoded using the Huffman coding variant, does not use refinement/aggregate coding, uses Table B.4 for SDHUFFDH, and uses Table B.2 for SDHUFFDW.
001A: 00 00 00 01
This field indicates that SDNUMEXSYMS is 1: one symbol is exported by this symbol dictionary.
001E: 00 00 00 01
This field indicates that SDNUMNEWSYMS is 1: one symbol is defined by this symbol dictionary.
0022: E9 CB F4 00 26 AF 04 BF F0 78 2F E0 00 40

---

## Page 138

ISO/IEC 14492:2001 (E)
using MMR. This produces the height class collective bitmap, which is also the bitmap for the single symbol in the height class, shown in Figure H.2.
T0829150-99/d43
Figure H.2 ñ The first symbol in the first symbol dictionary
This indicates that the page's X resolution is unknown.

---

## Page 139

ISO/IEC 14492:2001 (E)
vii) Read the next 12 bytes (6 rows of 2 bytes each), and use the leftmost 12 bits of each row as the height class collective bitmap. These 12 bytes are: 0067: 79 E0 84 10 81 F0 82 10 86 10 79 F0 or, in binary:

---

## Page 140

ISO/IEC 14492:2001 (E)
| 01111001 | 11100000 |
|---|---|
| 10000100   | 00010000 |
| 10000001   | 11110000 |
| 10000010   | 00010000 |
| 10000110   | 00010000 |
| 01111001   | 11110000 |
|  | The height class collective bitmap is therefore as shown in Figure H.3, and the two symbols are as shown |
|  |  |
T0829160-99/d44
Figure H.3 ñ The height class collective bitmap in the second symbol dictionary
viii) Since SDNUMNEWSYMS is 2, the last symbol has now been decoded.
ix) Using Table B.1, decode an export run length. This consumes the bits 00000, indicating that the first 0 symbols are not exported.
T0829170-99/d45
a) b)
Figure H.4 ñ The symbols in the second symbol dictionary
x) Using Table B.1, decode an export run length. This consumes the bits 00010, indicating that the next 2 symbols are exported. Thus, this symbol dictionary defines two symbols, which are both exported.
xi) Skip the remaining bits in the last byte. This consumes the bits 000000.
0075: 00 00 00 03 07 42 00 02 01 00 00 00 31
This segment has a segment number of 3, a type of "Immediate lossless text region" (type 7), a short page association field, and does not have the deferred non-retain bit set. It refers to two other segments, segments number 0 and 2; segment 0 should be retained, but segment 2 and this segment should not be retained. It is associated with page 1, and has a data length of 49 bytes.
00B2: D0
0082: 00 00 00 25
This field indicates that the region bitmap is 37 pixels wide.
0086: 00 00 00 08
This field indicates that the region bitmap is 8 pixels high.

---

## Page 141

ISO/IEC 14492:2001 (E)
008A: 00 00 00 04
This field indicates that the region bitmap's left edge is 4 pixels right of the page's left edge.
008E: 00 00 00 01
This field indicates that the region bitmap's top edge is 1 pixel down from the page's top edge.
0092: 00
This field indicates that the region should be drawn into the page using the combination operator OR.
0093: 0C 09
This field indicates that the segment is encoded using the Huffman coding variant, does not contain any refinements, has a SBSTRIPS value of 4, has a reference corner of BOTTOMLEFT, is not transposed, combines its symbols using OR, has a default pixel value of 0, and has a SBDSOFFSET value of 3.
0095: 00 10
This field indicates that the segment uses Table B.6 for SBHUFFFS, Table B.8 for SBHUFFDS, and Table B.12 for SBHUFFDT.
0097: 00 00 00 05
This field indicates that SBNUMINSTANCES is 5: five symbol instances are encoded in this text region.
00AB: 00 0C
The two symbol dictionaries referred to by this segment have a total of 3 symbols in them. Decoding the RUNCODE Huffman table, consuming all of the data but the last four bits, gives the following assignment of code lengths to the RUNCODEs:
RUNCODE1 1 RUNCODE2 1
and therefore the following Huffman table for the RUNCODEs:
RUNCODE1 0 RUNCODE2 1
Decoding using this table produces the sequence RUNCODE2, RUNCODE2, RUNCODE1 (consuming the bits 110). Thus, the first symbol (the "p" from the first symbol dictionary) has a Huffman code length of 2; the second symbol (the "c" from the second symbol dictionary) has a Huffman code length of 2, and the third symbol (the "a" from the second symbol dictionary) has a code length of 1. Applying the Huffman code assignment algorithm gives the table:
p 10 c 11 a 0
At this point, there is one bit (0) remaining in the last of data; this is now skipped.

---

## Page 142

ISO/IEC 14492:2001 (E)
| j) | The encoded text region data: |
|---|---|
|  | 00AD: 40 07 08 70 41 D0 |
|  | This is decoded as follows: |
| i)   |  |
|  |  |
|  | table's decoded value of 1, multiplied by |
| ii)   |  |
|  |  |
|  | (2 times   SBSTRIPS |
| iii)   |  |
|  |  |
|  | of 0. |
| iv)   |  |
|  | Reading two bits (since   |
|  | therefore 5 (STRIPT plus the decoded value of 1). |
| v)   |  |
|  |  |
|  |  |
| vi)   |  |
|  | At this point, CURS is 8 (0 plus |
| vii)   |  |
|  |  |
k) Decoding the data produced the following list of symbol instances and locations (the locations are where the symbol's lower left corner should be placed):
c (31, 5)

---

## Page 143

ISO/IEC 14492:2001 (E)
Drawing these symbol instances produces the 37-by-8 pixel region bitmap shown in Figure H.5.
T0829180-99/d46
Figure H.5 ñ The text region bitmap
10) The fifth segment header: 00B3: 00 00 00 04 27 00 01 00 00 00 2C This segment has a segment number of 4, a type of "Immediate lossless generic region" (type 39), a short page association field, and does not have the deferred non-retain bit set. It refers to no other segments, and is not retained. It is associated with page 1, and has a data length of 44 bytes. 11) The fifth segment data part: 00BE: 00 00 00 36 00 00 00 2C 00 00 00 04 00 00 00 0B 00CE: 00 01 26 A0 71 CE A7 FF FF FF FF FF FF FF FF FF 00DE: FF FF FF FF FF FF FF FF FF FF F8 F0 a) The region segment information field: 00BE: 00 00 00 36 00 00 00 2C 00 00 00 04 00 00 00 0B 00CE: 00 This indicates that the region bitmap encoded by this segment is 54 pixels wide, 44 pixels high, and its top left corner is 4 pixels right of the page's left edge and 11 pixels down from the page's top edge. It should be drawn into the page using OR. b) The region data: 00CF: 01 26 A0 71 CE A7 FF FF FF FF FF FF FF FF FF FF 00DF: FF FF FF FF FF FF FF FF FF F8 F0 The first byte (01) is the generic region segment flags byte, and indicates that the region is encoded using MMR. The remaining bytes are the MMR-encoded data for the region bitmap. These bytes MMR-decompress to the 54-by-44 region bitmap shown in Figure H.6. 12) The sixth segment header: 00EA: 00 00 00 05 10 01 01 00 00 00 2D This segment has a segment number of 5, a type of "Pattern dictionary" (type 16), a short page association field, and does not have the deferred non-retain bit set. It refers to no other segments, and is retained. It is associated with page 1, and has a data length of 45 bytes. 13) The sixth segment data part: 00F5: 01 04 04 00 00 00 0F 20 D1 84 61 18 45 F2 F9 7C 0105: 8F 11 C3 9E 45 F2 F9 7D 42 85 0A AA 84 62 2F EE 0115: EC 44 62 22 35 2A 0A 83 B9 DC EE 77 80 a) The one-byte pattern dictionary flags field: 00F5: 01 This field indicates that the segment is encoded using the MMR coding variant. b) The one-byte HDPW field: 00F6: 04 This field indicates that HDPW, the width of the patterns defined in this dictionary, is 4. c) The one-byte HDPH field: 00F7: 04 This field indicates that HDPH, the height of the patterns defined in this dictionary, is 4.

---

## Page 144

ISO/IEC 14492:2001 (E)
T0829190-99/47
Figure H.6 ñ The generic region bitmap
d) The four-byte GRAYMAX field:
00F8: 00 00 00 0F
This field indicates that GRAYMAX is 15, and thus there are 16 patterns in this pattern dictionary (numbered 0 through 15).
e) The remaining 38 bytes in the segment:
00FC: 20 D1 84 61 18 45 F2 F9 7C 8F 11 C3 9E 45 F2 F9
010C: 7D 42 85 0A AA 84 62 2F EE EC 44 62 22 35 2A 0A
011C: 83 B9 DC EE 77 80
These bytes MMR-decompress to the pattern dictionary's collective bitmap. The width of the bitmap is (GRAYMAX + 1) ◊ HDPW, and the height is HDPH pixels. The 64-by-4 bitmap that is the result of this MMR decompression is shown in Figure H.7.
T0829200-99/d48
Figure H.7 ñ The pattern dictionary collective bitmap
The 16 individual patterns are obtained from the collective bitmap by breaking it into 4-pixel-wide pieces. They are shown in Figure H.8, where pattern number 0 is the top left pattern, pattern number 1 is to its right, and so on.

---

## Page 145

ISO/IEC 14492:2001 (E)
T0829210-99/d49
Figure H.8 ñ The patterns defined in the pattern dictionary
This field indicates that the vertical offset of the halftone grid is 0 pixels.

---

## Page 146

ISO/IEC 14492:2001 (E)
0150: 04 00
This field indicates that the HRX is 1024, and thus the horizontal coordinate of the grid vector is 1024/256 pixels, or 4 pixels.
0152: 00 00
This field indicates that the vertical coordinate of the grid vector is 0 pixels.
| i) | The first bitplane: |
|---|---|
|  | 0154: AA AA AA AA 80 08 00 80 |
|  |  |
|  |  |
|  | consume an integral number of bytes). |
| j)   | The second bitplane: |
|  | 015C: 36 D5 55 6B 5A D4 00 40 04 |
|  |  |
| k)   | The third bitplane: |
|  |  |
|  |  |
| l)   | The fourth bitplane: |
|  |  |
|  | 0181: 40 04 00 40 |
|  |  |
T0829220-99/d50
a) b) c) d)
Figure H.9 ñ The raw bitplanes for the halftone region
m) These bitplanes are then Gray-decoded as described in C.5, by XORing the first into the second, then the resulting bitplane into the third, and so on. The resulting bitplanes are shown in Figure H.10.
T0829230-99/d51
a) b) c) d)
Figure H.10 ñ The bitplanes for the halftone region

---

## Page 147

# n)
0 1 2 3 4 5 6
1 2 3 4 5 6 7
2 3 4 5 6 7 8
3 4 5 6 7 8 9
4 5 6 7 8 9 10
5 6 7 8 9 10 11
6 7 8 9 10 11 12
7 8 9 10 11 12 13
8 9 10 11 12 13 14
# o)
(0, 0) (4, 0) (8, 0) (12, 0) (16, 0) (20, 0)
(0, 4) (4, 4) (8, 4) (12, 4) (16, 4) (20, 4)
(0, 8) (4, 8) (8, 8) (12, 8) (16, 8) (20, 8)
(0, 12) (4, 12) (8, 12)
| (12, 12) | (16, 12) | (20, 12) |
|---|---|---|
|  |  |  |
|  |  |  |
| (8, 16)   |  |  |
| (12, 16)   (16, 16) |   (20, 16) |   |
|  |  |  |
|  |  |  |
| (8, 20)   |  |  |
| (12, 20)   (16, 20) |   (20, 20) |   |
|  |  |  |
|  |  |  |
| (8, 24)   |  |  |
| (12, 24)   (16, 24) |   (20, 24) |   |
|  |  |  |
|  |  |  |
| (8, 28)   |  |  |
| (12, 28)   (16, 28) |   (20, 28) |   |
|  |  |  |
|  |  |  |
| (8, 32)   |  |  |
| (12, 32)   (16, 32) |   (20, 32) |   |
# p)
# 16) The eighth segment header:
# 0185: 00 00 00 07 31 00 01 00 00 00 00
# page 1, and has a data length of zero bytes.
# 17)
# in Figure H.1.
# 18) The ninth segment header:
# 0190: 00 00 00 08 30 00 02 00 00 00 13
# page 2, and has a data length of 19 bytes.
# 19) The ninth segment data part:
# 019B: 00 00 00 40 00 00 00 38 00 00 00 00 00 00 00 00
# 01AB: 01 00 00
# This contains the same information as does segment number 1.
# ISO/IEC 14492:2001 (E)
7
8
9
10
11
12
13
14
15
(24, 0) (28, 0)
(24, 4) (28, 4)
(24, 8) (28, 8)
| (24, 12) | (28, 12) |
|---|---|
| (24, 16)   (28, 16) |  |
| (24, 20)   (28, 20) |  |
| (24, 24)   (28, 24) |  |
| (24, 28)   (28, 28) |  |
| (24, 32)   (28, 32) |  |
# 0,
# Stacking up these bitplanes, with the first being the most significant, results in the array of values:
# The halftone grid vector and offset produces the following array of locations. Combining this with the array of
# values results in a list of drawing operations, indicating that the pattern whose index in the pattern dictionary in
# segment number 5 is that value should be drawn with its upper left pixel at the given location.
# Performing those drawing operations produces the 32-by-36 region bitmap shown in Figure H.11
# This segment has a segment number of 7, a type of "End of page" (type 49), a short page association field, and does
# not have the deferred non-retain bit set. It refers to no other segments, and is not retained. It is associated with
# The region segment bitmaps are combined as follows, taking into account the page default pixel value and each
# region segment's combination operator. First, the page bitmap (64 pixels wide and 56 pixels high) is filled with
# the page default pixel value. Next, the bitmap shown in Figure H.5 is drawn using OR into the page bitmap with its
# top left pixel at location (4, 1). Next, the bitmap shown in Figure H.6 is drawn using OR into the page bitmap with
# its top left pixel at location (4, 11). Finally, the bitmap shown in Figure H.11 is drawn using OR into the page
# bitmap with its top left pixel at location (16, 15). After all this has been done, the resulting bitmap is the one shown
# This segment has a segment number of 8, a type of "Page information" (type 48), a short page association field, and
# does not have the deferred non-retain bit set. It refers to no other segments, and is not retained. It is associated with

---

## Page 148

ISO/IEC 14492:2001 (E)
T0829240-99/d52
Figure H.11 ñ The halftone region bitmap
Decoding this gives the same two symbols shown in Figure H.4 a) and b).

---

## Page 149

ISO/IEC 14492:2001 (E)
This field is eight bytes long because GBTEMPLATE is 0, and there are thus four AT pixels whose positions must be determined. The AT pixels are located with A1 at (3, ñ1); A2 at (ñ3, ñ1); A3 at (2, ñ2); and A4 at (ñ2, ñ2). These are the nominal positions of those pixels for this template.

---

## Page 150

ISO/IEC 14492:2001 (E)
d) The region data: 0225: 04 EE ED 87 FB CB 2B FF AC Decoding this using the decoded values of GBTEMPLATE, TPGDON, and the AT pixel locations produces the 54-by-44 region bitmap shown in Figure H.6. 26) The thirteenth segment header: 022E: 00 00 00 0C 10 01 02 00 00 00 1C This segment has a segment number of 12, a type of "Pattern dictionary" (type 16), a short page association field, and does not have the deferred non-retain bit set. It refers to no other segments, and is retained. It is associated with page 2, and has a data length of 28 bytes. 27) The thirteenth segment data part: 0239: 06 04 04 00 00 00 0F 90 71 6B 6D 99 A7 AA 49 7D 0249: F2 E5 48 1F DC 68 BC 6E 40 BB FF AC a) The one-byte pattern dictionary flags field: 0239: 06 This field indicates that the segment is encoded using the arithmetic coding variant, and that HDTEMPLATE is 3. b) The one-byte HDPW field: 023A: 04 This field indicates that HDPW is 4. c) The one-byte HDPH field: 023B: 04 This field indicates that HDPH is 4. d) The four-byte GRAYMAX field: 023C: 00 00 00 0F This field indicates that GRAYMAX is 15, and thus there are 16 patterns in this pattern dictionary. e) The remaining 21 bytes in the segment: 0240: 90 71 6B 6D 99 A7 AA 49 7D F2 E5 48 1F DC 68 BC 0250: 6E 40 BB FF AC These bytes decompress, using the pattern dictionary decoding procedure, to the collective bitmap shown in Figure H.7, and thus the 16 patterns defined by this pattern dictionary are as shown in Figure H.8. 28) The fourteenth segment header: 0255: 00 00 00 0D 17 20 0C 02 00 00 00 3E This segment has a segment number of 13, a type of "Immediate lossless halftone region" (type 23), a short page association field, and does not have the deferred non-retain bit set. It refers to one other segment, segment number 12; neither segment 12 nor this segment should be retained. It is associated with page 2, and has a data length of 62 bytes. 29) The fourteenth segment data part: 0261: 00 00 00 20 00 00 00 24 00 00 00 10 00 00 00 0F 0271: 00 02 00 00 00 08 00 00 00 09 00 00 00 00 00 00 0281: 00 00 04 00 00 00 87 CB 82 1E 66 A4 14 EB 3C 4A 0291: 15 FA CC D6 F3 B1 6F 4C ED BF A7 BF FF AC a) The region segment information field: 0261: 00 00 00 20 00 00 00 24 00 00 00 10 00 00 00 0F 0271: 00 This indicates that the region bitmap encoded by this segment is 32 pixels wide, 36 pixels high, and its top left corner is 16 pixels right of the page's left edge and 15 pixels down from the page's top edge. It should be drawn into the page using OR. b) The halftone region segment flags field: 0272: 02 This field indicates that the halftone region is encoded using the arithmetic coding variant, and that HTEMPLATE is 1. The patterns should be combined using OR. The default pixel value is 0.

---

## Page 151

ISO/IEC 14492:2001 (E)
c) The other parameters: 0273: 00 00 00 08 00 00 00 09 00 00 00 00 00 00 00 00 0283: 04 00 00 00 The following fields indicate that HGW is 8, HGH is 9, HGX is 0, HGY is 0, HRX is 1024, and HRY is 0. d) The four bitplanes: 0287: 87 CB 82 1E 66 A4 14 EB 3C 4A 15 FA CC D6 F3 B1 0297: 6F 4C ED BF A7 BF FF AC Decoding four 8-by-9 bitplanes from this data results in the four bitplanes shown in Figure H.9. As in segment number 6, Gray-decoding these bitplanes, combining them into an array of values, and drawing the patterns from the pattern dictionary according to that array and the halftone grid parameters results in the region bitmap shown in Figure H.11. 30) The fifteenth segment header: 029F: 00 00 00 0E 31 00 02 00 00 00 00 This segment has a segment number of 14, a type of "End of page" (type 49), a short page association field, and does not have the deferred non-retain bit set. It refers to no other segments, and is not retained. It is associated with page 2, and has a data length of zero bytes. 31) The page bitmap is made by combining the three region bitmaps in the identical way that they were combined in page 1, resulting in the same page bitmap. 32) The sixteenth segment header: 02AA: 00 00 00 0F 30 00 03 00 00 00 13 This segment has a segment number of 15, a type of "Page information" (type 48), a short page association field, and does not have the deferred non-retain bit set. It refers to no other segments, and is not retained. It is associated with page 3, and has a data length of 19 bytes. 33) The sixteenth segment data part: 02B5: 00 00 00 25 00 00 00 08 00 00 00 00 00 00 00 00 02C5: 01 00 00 This indicates that the page is 37 pixels wide, is 8 pixels high, has unknown X and Y resolution, is eventually lossless, does not contain any refinements, has a default pixel value of 0, a default combination operator of OR, does not require any auxiliary buffers, and uses the page default combination operator in every region on the page. 34) The seventeenth segment header: 02C8: 00 00 00 10 00 01 00 00 00 00 16 This segment has a segment number of 16, a type of "Symbol dictionary" (type 0), a short page association field, and does not have the deferred non-retain bit set. It refers to no other segments, and is retained. It is associated with no page, and has a data length of 22 bytes. 35) The seventeenth segment data part: 02D3: 08 00 02 FF 00 00 00 01 00 00 00 01 4F E7 8D 68 02E3: 1B 14 2F 3F FF AC a) The two-byte symbol dictionary flags field: 02D3: 08 00 This field indicates that the segment is encoded using the arithmetic coding variant and does not use refinement/aggregate coding. SDTEMPLATE has the value 2. The "bitmap coding context used" and "bitmap coding context retained" bits are both 0. b) The symbol dictionary AT flags field: 02D5: 02 FF This field indicates that SDATX1 is 2 and SDATY1 is ñ1; thus, AT pixel A1 is at location (2, ñ1), which is the nominal value for template 2. c) The four-byte SDNUMEXSYMS field: 02D7: 00 00 00 01 This field indicates that SDNUMEXSYMS is 1: one symbol is exported by this symbol dictionary. d) The four-byte SDNUMNEWSYMS field: 02DB: 00 00 00 01 This field indicates that SDNUMNEWSYMS is 1: one symbol is defined by this symbol dictionary.

---

## Page 152

ISO/IEC 14492:2001 (E)
viii) Using the IAEX arithmetic integer decoding procedure, decode an export run length. The value decoded is 1, indicating that the next 1 symbols are exported. Thus, this symbol dictionary defines one symbol, which is exported.
T0829250-99/d53
a) b) c)
Figure H.12 ñ The symbols in the symbol dictionary on the third page
This field indicates that SDRATX1 is ñ1, SDRATY1 is ñ1, SDRATX2 is ñ1, and SDRATY2 is ñ1. Thus, AT pixel RA1 is at location (ñ1, ñ1) and AT pixel RA2 is at location (ñ1, ñ1), which are the nominal locations for refinement template 0.

---

## Page 153

ISO/IEC 14492:2001 (E)
• Using the IAID arithmetic integer decoding procedure, decode a symbol ID. The value decoded is 1. The second symbol is thus the one identified by symbol ID 1, which is the first (and, thus far only) symbol defined in this symbol dictionary.

---

## Page 154

ISO/IEC 14492:2001 (E)
iii) Using the IADT arithmetic integer decoding procedure, decode a delta T value. The value decoded is 0.

---

## Page 155

ISO/IEC 14492:2001 (E)
iv) Using the IAFS arithmetic integer decoding procedure, decode a first S value. The value decoded is 0. The reference corner (top left corner in this case) of the first symbol instance in the text region is thus at (0, 0).
v) Using the IAID arithmetic integer decoding procedure, decode a symbol ID. The value decoded is 1. The first symbol instance thus uses the symbol shown in Figure H.12 b).
vi) Using the IARI arithmetic integer decoding procedure, decode a refinement flag. The value decoded is 0, indicating that the first symbol instance is not refined.
vii) At this point CURS is 5. Using the IADS arithmetic integer decoding procedure, decode a delta S value. The value decoded is 0. After adding in SBDSOFFSET, the reference corner of the second symbol instance in the aggregation is thus at (8, 0).
viii) Using the IAID arithmetic integer decoding procedure, decode a symbol ID. The value decoded is 0. The second symbol instance thus uses the symbol shown in Figure H.12 a).
ix) Using the IARI arithmetic integer decoding procedure, decode a refinement flag. The value decoded is 0, indicating that the first symbol instance is not refined.
x) At this point CURS is 13. Using the IADS arithmetic integer decoding procedure, decode a delta S value. The value decoded is 0. After adding in SBDSOFFSET, the reference corner of the third symbol instance in the aggregation is thus at (16, 0).
xi) Using the IAID arithmetic integer decoding procedure, decode a symbol ID. The value decoded is 1. The third symbol instance thus uses the symbol shown in Figure H.12 b).
xii) Using the IARI arithmetic integer decoding procedure, decode a refinement flag. The value decoded is 1, indicating that the third symbol instance is refined.
xiii) Using the IARDW arithmetic integer decoding procedure, decode a symbol instance refinement delta width value. The value decoded is ñ1.
xiv) Using the IARDH arithmetic integer decoding procedure, decode a symbol instance refinement delta height value. The value decoded is 2. Since the reference symbol is 6 ◊ 6 pixels, the refined symbol instance is therefore 5 ◊ 8 pixels.
xv) Using the IARDX arithmetic integer decoding procedure, decode a symbol instance refinement delta X value. The value decoded is 1.
xvi) Using the IARDY arithmetic integer decoding procedure, decode a symbol instance refinement delta Y value. The value decoded is ñ2. GRREFERENCEDX and GRREFERENCEDY are therefore set as:
GRREFERENCEDX = ñ1/2 + 1 = ñ1 + 1 = 0
GRREFERENCEDY = 2/2 ñ2 = 1 ñ 2 = ñ1
so the refinement is done with the refined symbol placed so that the top left pixel of the refined symbol is aligned with pixel (0, 1) of the reference symbol.
xvii) Using the generic refinement region decoding procedure, decode the refined symbol instance bitmap. The reference bitmap is the bitmap shown in Figure H.12 b) and the refined bitmap, which is the bitmap for the third symbol instance, is the bitmap shown in Figure H.2.
xviii) At this point CURS is 18. Using the IADS arithmetic integer decoding procedure, decode a delta S value. The value decoded is 0. After adding in SBDSOFFSET, the reference corner of the third symbol instance in the aggregation is thus at (23, 0).
xix) Using the IAID arithmetic integer decoding procedure, decode a symbol ID. The value decoded is 2. The fourth symbol instance thus uses the symbol shown in Figure H.12 c).
xx) Using the IADS arithmetic integer decoding procedure, decode a delta S value. The value decoded is OOB, indicating the end of the strip.
xxi) Decoding of the text region is now complete. The text region bitmap is the bitmap shown in Figure H.5. It is obtained by drawing Figure H.12 b) with its top left corner at (0, 0), Figure H.12 a) with its top left corner at (8, 0), Figure H.2 with its top left corner at (16, 0), and Figure H.12 c) with its top left corner at (23, 0).

---

## Page 156

ISO/IEC 14492:2001 (E)
40) The twentieth segment header:
0346: 00 00 00 13 31 00 03 00 00 00 00
page 3, and has a data length of zero bytes.
segment's bitmap (Figure H.5).
41)
42) The twenty-first segment header:
0351: 00 00 00 14 33 00 00 00 00 00 00
page, and has a data length of zero bytes.
H.2 Test sequence for arithmetic coder
00 02 00 51 00 00 00 C0 03 52 87 2A AA AA AA AA
82 C0 20 00 FC D7 9E F6 BF 7F ED 90 4F 46 A3 BF
84 C7 3B FC E1 A1 43 04 02 20 00 00 41 0D BB 86
F4 31 7F FF 88 FF 37 47 1A DB 6A DF FF AC
Decoding those 30 bytes should produce the 32 original bytes.
Note that the A register is always greater than or equal to 0x8000.
pointer BP is advanced to point beyond it.
point at it. Note that the final 0xAC BYTEIN.
0.
This segment has a segment number of 19, a type of "End of page" (type 49), a short page association field, and does not have the deferred non-retain bit set. It refers to no other segments, and is not retained. It is associated with
The page bitmap is formed from the page's only region segment, segment number 18, and is equal to that region
This segment has a segment number of 20, a type of "End of file" (type 51), a short page association field, and does not have the deferred non-retain bit set. It refers to no other segments, and is not retained. It is associated with no
In this subclause, a small data set is provided for testing the arithmetic encoder and decoder. The test is structured to test many of the encoder and decoder paths, but it is impossible, in a short test sequence, to check all of them so agreement with the results of this test, unfortunately, does not guarantee a completely correct implementation.
The decisions to be encoded, packed into 32 bytes and shown in hexadecimal, are:
The encoded data that should be obtained by encoding that sequence, shown as 30 hexadecimal bytes, are:
Table H.1 provides a bit-by-bit list of the arithmetic encoder and decoder operation. The first line in this table corresponds to the INITENC and INITDEC operations. The value of the byte before the first byte in the output buffer is assumed to be 0x00, making the initial value of B 0x00. The last line in the table corresponds to the FLUSH operation.
For this entire test, a single value of CX is used. I(CX) is initially 0 and MPS(CX) is initially
The first column is the event counter EC. The second column is the decision D to be encoded. The third and fourth columns give the values of I(CX) and MPS(CX). The fifth column shows an "X" indicating that the conditional exchange will occur when encoding (decoding) the current decision. The sixth column shows the current Qe value corresponding to I(CX) (see Table E.1). The seventh column shows the value of the register A before the decision is encoded (decoded).
The variables up to this point were common for the encoder and decoder. The next four columns (C, CT, B, OUT) are only for the encoder. The next four columns (C, CT, B, IN) are only for the decoder. The final column (C) shows the C register for the software-conventions decoder, as it is the only value that differs for the software-conventions decoder. All the values shown for the C registers are given before the current decision is encoded (decoded).
For the encoder, CT is a counter indicating when a byte is ready for output from register C. The column under B shows the byte in variable B waiting to be sent out. This byte can sometimes change from a carry-over. Finally, for the encoder the compressed bytes are listed under column OUT. A byte is considered to be "output" when the compressed data
The decoder's counter CT indicates when to input the next byte from the compressed data. The column under B shows the value of the B register, which is used to determine when a bit-stuffing has occurred. The column under IN shows the bytes that are consumed. A byte is considered to be "consumed" when the compressed data pointer BP is advanced to byte is never consumed, according to this definition, though it is read as part of

---

## Page 157

Common Encoder
Qe A C B OUT EC D I MPS CE CT hex hex hex hex hex
0 00 1 0 0 0 X 5601 8000 00000000 12 00 2 0 1 0 3401 AC02 00000000 11 00 3 0 2 0 1801 F002 00006802 10 00 4 0 2 0 1801 D801 00008003 10 00 5 0 2 0 1801 C000 00009804 10 00 6 0 2 0 1801 A7FF 0000B005 10 00 7 0 2 0 1801 8FFE 0000C806 10 00 8 0 3 0 0AC1 EFFA 0001C00E 9 00 9 0 3 0 0AC1 E539 0001CACF 9 00 10 0 3 0 0AC1 DA78 0001D590 9 00 11 0 3 0 0AC1 CFB7 0001E051 9 00 12 0 3 0 0AC1 C4F6 0001EB12 9 00 13 0 3 0 0AC1 BA35 0001F5D3 9 00 14 0 3 0 0AC1 AF74 00020094 9 00 15 1 3 0 0AC1 A4B3 00020B55 9 00 16 0 12 0 1C01 AC10 0020B550 5 00 17 0 12 0 1C01 900F 0020D151 5 00 18 0 13 0 1601 E81C 0041DAA4 4 00 19 0 13 0 1601 D21B 0041F0A5 4 00 20 0 13 0 1601 BC1A 004206A6 4 00 21 0 13 0 1601 A619 00421CA7 4 00 22 0 13 0 1601 9018 004232A8 4 00 23 0 29 0 1101 F42E 00849152 3 00 24 0 29 0 1101 E32D 0084A253 3 00 25 0 29 0 1101 D22C 0084B354 3 00 26 1 29 0 1101 C12B 0084C455 3 84 27 0 27 0 1401 8808 000622A8 8 84 28 1 28 0 1201 E80E 000C6D52 7 84 29 0 26 0 1601 9008 00636A90 4 84 30 0 27 0 1401 F40E 00C70122 3 84 31 0 27 0 1401 E00D 00C71523 3 84 32 1 27 0 1401 CC0C 00C72924 3 C7 84 33 0 25 0 1801 A008 00014920 8 C7 34 0 25 0 1801 8807 00016121 8 C7 35 0 26 0 1601 E00C 0002F244 7 C7 36 0 26 0 1601 CA0B 00030845 7 C7 37 0 26 0 1601 B40A 00031E46 7 C7 38 0 26 0 1601 9E09 00033447 7 C7 39 0 26 0 1601 8808 00034A48 7 C7 40 0 27 0 1401 E40E 0006C092 6 C7 41 0 27 0 1401 D00D 0006D493 6 C7 42 0 27 0 1401 BC0C 0006E894 6 C7 43 0 27 0 1401 A80B 0006FC95 6 C7 44 0 27 0 1401 940A 00071096 6 C7 45 0 27 0 1401 8009 00072497 6 C7 46 0 28 0 1201 D810 000E7130 5 C7 47 0 28 0 1201 C60F 000E8331 5 C7 48 0 28 0 1201 B40E 000E9532 5 C7 49 0 28 0 1201 A20D 000EA733 5 C7 50 0 28 0 1201 900C 000EB934 5 C7 51 0 29 0 1101 FC16 001D966A 4 C7 52 0 29 0 1101 EB15 001DA76B 4 C7 53 0 29 0 1101 DA14 001DB86C 4 C7 54 0 29 0 1101 C913 001DC96D 4 C7 55 0 29 0 1101 B812 001DDA6E 4 C7 56 0 29 0 1101 A711 001DEB6F 4 C7 57 1 29 0 1101 9610 001DFC70 4 C7 58 1 27 0 1401 8808 00EFE380 1 3B C7 59 0 25 0 1801 A008 001F1C00 6 3B 60 0 25 0 1801 8807 001F3401 6 3B 61 0 26 0 1601 E00C 003E9804 5 3B 62 0 26 0 1601 CA0B 003EAE05 5 3B 63 0 26 0 1601 B40A 003EC406 5 3B 64 0 26 0 1601 9E09 003EDA07 5 3B
### ISO/IEC 14492:2001 (E)
Software Conven-tions Decoder
C B IN C CT hex hex hex hex
00000000 84 C7 00000000 42638000 1 C7 3D9C0000 84C70000 0 C7 3B 273A0000 A18C7600 7 3B 4E758800 898B7600 7 3B 4E758800 718A7600 7 3B 4E758800 59897600 7 3B 4E758800 41887600 7 3B 4E758800 530EEC00 6 3B 9CEB1000 484DEC00 6 3B 9CEB1000 3D8CEC00 6 3B 9CEB1000 32CBEC00 6 3B 9CEB1000 280AEC00 6 3B 9CEB1000 1D49EC00 6 3B 9CEB1000 1288EC00 6 3B 9CEB1000 07C7EC00 6 3B 9CEB1000 7C7EC000 2 3B 2F910000 607DC000 2 3B 2F910000 88F98000 1 3B 5F220000 72F88000 1 3B 5F220000 5CF78000 1 3B 5F220000 46F68000 1 3B 5F220000 30F58000 1 3B 5F220000 35E90000 0 3B BE440000 24E80000 0 3B BE440000 13E70000 0 3B BE440000 02E60000 0 3B FC BE440000 1737E000 5 FC 70D01800 066DC000 4 FC E1A03000 336E0000 1 FC 5C998000 3ADA0000 0 FC B9330000 26D90000 0 FC B9330000 12D80000 0 FC E1 B9330000 96C70800 5 E1 0940F000 7EC60800 5 E1 0940F000 CD8A1000 4 E1 1281E000 B7891000 4 E1 1281E000 A1881000 4 E1 1281E000 8B871000 4 E1 1281E000 75861000 4 E1 1281E000 BF0A2000 3 E1 2503C000 AB092000 3 E1 2503C000 97082000 3 E1 2503C000 83072000 3 E1 2503C000 6F062000 3 E1 2503C000 5B052000 3 E1 2503C000 8E084000 2 E1 4A078000 7C074000 2 E1 4A078000 6A064000 2 E1 4A078000 58054000 2 E1 4A078000 46044000 2 E1 4A078000 68068000 1 E1 940F0000 57058000 1 E1 940F0000 46048000 1 E1 940F0000 35038000 1 E1 940F0000 24028000 1 E1 940F0000 13018000 1 E1 940F0000 02008000 1 E1 A1 940F0000 10068400 6 A1 78017800 80342000 3 A1 1FD3C000 68332000 3 A1 1FD3C000 A0644000 2 A1 3FA78000 8A634000 2 A1 3FA78000 74624000 2 A1 3FA78000 5E614000 2 A1 3FA78000
Decoder
### Table H.1 ñ Encoder and decoder trace data

---

## Page 158

### ISO/IEC 14492:2001 (E)
### Table H.1 ñ Encoder and decoder trace data
Common Encoder
Qe A C B OUT EC D I MPS CE CT hex hex hex hex hex
65 0 26 0 1601 8808 003EF008 5 3B 66 0 27 0 1401 E40E 007E0C12 4 3B 67 0 27 0 1401 D00D 007E2013 4 3B 68 0 27 0 1401 BC0C 007E3414 4 3B 69 0 27 0 1401 A80B 007E4815 4 3B 70 0 27 0 1401 940A 007E5C16 4 3B 71 1 27 0 1401 8009 007E7017 4 3B 72 1 25 0 1801 A008 03F380B8 1 FC 3B 73 0 23 0 2201 C008 001C05C0 6 FC 74 1 23 0 2201 9E07 001C27C1 6 FC 75 0 21 0 2801 8804 00709F04 4 FC 76 1 22 0 2401 C006 00E18E0A 3 FC 77 0 20 0 3001 9004 03863828 1 E1 FC 78 0 21 0 2801 C006 0004D052 8 E1 79 1 21 0 2801 9805 0004F853 8 E1 80 0 19 0 3401 A004 0013E14C 6 E1 81 1 20 0 3001 D806 00282A9A 5 E1 82 0 19 0 3401 C004 00A0AA68 3 E1 83 0 19 0 3401 8C03 00A0DE69 3 E1 84 0 20 0 3001 B004 014224D4 2 E1 85 0 20 0 3001 8003 014254D5 2 E1 86 1 21 0 2801 A004 028509AC 1 A1 E1 87 1 19 0 3401 A004 000426B0 7 A1 88 1 18 0 3801 D004 00109AC0 5 A1 89 0 17 0 4801 E004 00426B00 3 A1 90 0 17 0 4801 9803 0042B301 3 A1 91 1 18 0 3801 A004 0085F604 2 42 A1 92 0 17 0 4801 E004 0007D810 8 42 93 1 17 0 4801 9803 00082011 8 42 94 0 16 0 X 5101 9002 00104022 7 42 95 1 17 0 4801 A202 00208044 6 42 96 0 16 0 X 5101 9002 00410088 5 42 97 1 17 0 4801 A202 00820110 4 42 98 0 16 0 X 5101 9002 01040220 3 42 99 1 17 0 4801 A202 02080440 2 42 100 0 16 0 X 5101 9002 04100880 1 04 43 101 1 17 0 4801 A202 00001100 8 04 102 0 16 0 X 5101 9002 00002200 7 04 103 1 17 0 4801 A202 00004400 6 04 104 0 16 0 X 5101 9002 00008800 5 04 105 1 17 0 4801 A202 00011000 4 04 106 0 16 0 X 5101 9002 00022000 3 04 107 1 17 0 4801 A202 00044000 2 04 108 0 16 0 X 5101 9002 00088000 1 02 04 109 1 17 0 4801 A202 00010000 8 02 110 0 16 0 X 5101 9002 00020000 7 02 111 1 17 0 4801 A202 00040000 6 02 112 0 16 0 X 5101 9002 00080000 5 02 113 1 17 0 4801 A202 00100000 4 02 114 0 16 0 X 5101 9002 00200000 3 02 115 1 17 0 4801 A202 00400000 2 02 116 0 16 0 X 5101 9002 00800000 1 20 02 117 1 17 0 4801 A202 00000000 8 20 118 0 16 0 X 5101 9002 00000000 7 20 119 1 17 0 4801 A202 00000000 6 20 120 0 16 0 X 5101 9002 00000000 5 20 121 1 17 0 4801 A202 00000000 4 20 122 0 16 0 X 5101 9002 00000000 3 20 123 1 17 0 4801 A202 00000000 2 20 124 0 16 0 X 5101 9002 00000000 1 00 20 125 1 17 0 4801 A202 00000000 8 00 126 0 16 0 X 5101 9002 00000000 7 00 127 1 17 0 4801 A202 00000000 6 00 128 0 16 0 X 5101 9002 00000000 5 00
### (continued)
Software Conven-tions Decoder
C B IN C CT hex hex hex hex
48604000 2 A1 3FA78000 64BE8000 1 A1 7F4F0000 50BD8000 1 A1 7F4F0000 3CBC8000 1 A1 7F4F0000 28BB8000 1 A1 7F4F0000 14BA8000 1 A1 7F4F0000 00B98000 1 A1 43 7F4F0000 05CD0C00 6 43 9A3AF000 2E686000 3 43 919F8000 0C676000 3 43 919F8000 319D8000 1 43 56660000 13390000 0 43 04 ACCC0000 4CE41000 6 04 431FEC00 39C62000 5 04 863FD800 11C52000 5 04 863FD800 47148000 3 04 58EF6000 26270000 2 04 B1DEC000 989C0000 0 04 27670000 649B0000 0 04 02 27670000 61340400 7 02 4ECFFA00 31330400 7 02 4ECFFA00 02640800 6 02 9D9FF400 09902000 4 02 9673D000 26408000 2 02 A9C34000 99020000 0 02 47010000 51010000 0 02 20 47010000 12004000 7 20 8E03BE00 48010000 5 20 9802F800 00000000 5 20 9802F800 00000000 4 20 9001F000 00000000 3 20 A201E000 00000000 2 20 9001C000 00000000 1 20 A2018000 00000000 0 20 00 90010000 00000000 7 00 A201FE00 00000000 6 00 9001FC00 00000000 5 00 A201F800 00000000 4 00 9001F000 00000000 3 00 A201E000 00000000 2 00 9001C000 00000000 1 00 A2018000 00000000 0 00 90010000 00000000 7 00 A201FE00 00000000 6 00 9001FC00 00000000 5 00 A201F800 00000000 4 00 9001F000 00000000 3 00 A201E000 00000000 2 00 9001C000 00000000 1 00 A2018000 00000000 0 00 41 90010000 00008200 7 41 A2017C00 00010400 6 41 9000F800 00020800 5 41 A1FFF000 00041000 4 41 8FFDE000 00082000 3 41 A1F9C000 00104000 2 41 8FF18000 00208000 1 41 A1E10000 00410000 0 41 0D 8FC00000 00821A00 7 0D A17FE400 01043400 6 0D 8EFDC800 02086800 5 0D 9FF99000 0410D000 4 0D 8BF12000 0821A000 3 0D 99E04000 10434000 2 0D 7FBE8000
Decoder

---

## Page 159

### Table H.1 ñ Encoder and decoder trace data
Common Encoder
Qe A C B OUT EC D I MPS CE CT hex hex hex hex hex
129 1 17 0 4801 A202 00000000 4 00 130 0 16 0 X 5101 9002 00000000 3 00 131 0 17 0 4801 A202 00000000 2 00 132 0 18 0 3801 B402 00009002 1 00 00 133 0 19 0 3401 F802 00019006 8 00 134 0 19 0 3401 C401 0001C407 8 00 135 1 19 0 3401 9000 0001F808 8 00 136 0 18 0 3801 D004 0007E020 6 00 137 1 18 0 3801 9803 00081821 6 00 138 1 17 0 4801 E004 00206084 4 00 139 0 16 0 X 5101 9002 0040C108 3 00 140 0 17 0 4801 A202 00818210 2 00 141 0 18 0 3801 B402 01039422 1 40 00 142 0 19 0 3401 F802 00079846 8 40 143 0 19 0 3401 C401 0007CC47 8 40 144 0 19 0 3401 9000 00080048 8 40 145 0 20 0 3001 B7FE 00106892 7 40 146 0 20 0 3001 87FD 00109893 7 40 147 1 21 0 2801 AFF8 00219128 6 40 148 0 19 0 3401 A004 008644A0 4 40 149 0 20 0 3001 D806 010CF142 3 40 150 0 20 0 3001 A805 010D2143 3 40 151 0 21 0 2801 F008 021AA288 2 40 152 0 21 0 2801 C807 021ACA89 2 40 153 0 21 0 2801 A006 021AF28A 2 40 154 0 22 0 2401 F00A 04363516 1 40 155 0 22 0 2401 CC09 04365917 1 40 156 0 22 0 2401 A808 04367D18 1 40 157 0 22 0 2401 8407 0436A119 1 0D 41 158 0 23 0 2201 C00C 00058A34 8 0D 159 0 23 0 2201 9E0B 0005AC35 8 0D 160 0 24 0 1C01 F814 000B9C6C 7 0D 161 1 24 0 1C01 DC13 000BB86D 7 0D 162 1 22 0 2401 E008 005DC368 4 0D 163 1 20 0 3001 9004 01770DA0 2 BB 0D 164 1 19 0 3401 C004 00043680 8 BB 165 1 18 0 3801 D004 0010DA00 6 BB 166 1 17 0 4801 E004 00436800 4 BB 167 0 16 0 X 5101 9002 0086D000 3 BB 168 0 17 0 4801 A202 010DA000 2 BB 169 1 18 0 3801 B402 021BD002 1 86 BB 170 1 17 0 4801 E004 000F4008 7 86 171 0 16 0 X 5101 9002 001E8010 6 86 172 1 17 0 4801 A202 003D0020 5 86 173 0 16 0 X 5101 9002 007A0040 4 86 174 1 17 0 4801 A202 00F40080 3 86 175 1 16 0 X 5101 9002 01E80100 2 F4 86 176 1 15 0 5401 FC04 00014804 8 F4 177 1 14 0 X 5601 A802 00029008 7 F4 178 0 14 1 X 5601 A402 0005CC12 6 F4 179 0 14 0 X 5601 9C02 000C4426 5 F4 180 1 15 0 5401 AC02 0018884C 4 F4 181 1 14 0 X 5601 A802 00311098 3 F4 182 1 14 1 X 5601 A402 0062CD32 2 F4 183 1 15 1 5401 AC02 00C59A64 1 31 F4 184 0 16 1 5101 B002 0003DCCA 8 31 185 1 15 1 X 5401 A202 0007B994 7 31 186 1 16 1 5101 A802 000F7328 6 31 187 1 17 1 4801 AE02 001F8852 5 31 188 1 18 1 3801 CC02 003FA0A6 4 31 189 0 18 1 3801 9401 003FD8A7 4 31 190 1 17 1 4801 E004 00FF629C 2 31 191 1 17 1 4801 9803 00FFAA9D 2 31 192 0 18 1 3801 A004 01FFE53C 1 7F 31
### ISO/IEC 14492:2001 (E)
### (continued)
Software Conven-tions Decoder
C B IN C CT hex hex hex hex
20868000 1 0D 817B0000 410D0000 0 0D BB 4EF40000 821B7600 7 BB 1FE68800 7434EC00 6 BB 3FCD1000 7867D800 5 BB 7F9A2000 4466D800 5 BB 7F9A2000 1065D800 5 BB 7F9A2000 41976000 3 BB 8E6C8000 09966000 3 BB 8E6C8000 26598000 1 BB B9AA0000 4CB30000 0 BB 86 434E0000 99670C00 7 86 089AF200 A2CC1800 6 86 1135E400 D5963000 5 86 226BC800 A1953000 5 86 226BC800 6D943000 5 86 226BC800 73266000 4 86 44D79000 43256000 4 86 44D79000 2648C000 3 86 89AF2000 99230000 1 86 06E08000 CA440000 0 86 0DC10000 9A430000 0 86 F4 0DC10000 D485E800 7 F4 1B821600 AC84E800 7 F4 1B821600 8483E800 7 F4 1B821600 B905D000 6 F4 37042C00 9504D000 6 F4 37042C00 7103D000 6 F4 37042C00 4D02D000 6 F4 37042C00 5203A000 5 F4 6E085800 3002A000 5 F4 6E085800 1C034000 4 F4 DC10B000 00024000 4 F4 DC10B000 00120000 1 F4 31 DFF58000 00486200 7 31 8FBB9C00 01218800 5 31 BEE27000 04862000 3 31 CB7DC000 12188000 1 31 CDEB0000 24310000 0 31 7F 6BD00000 4862FE00 7 7F 599F0000 00C3FC00 6 7F B33E0000 030FF000 4 7F DCF40000 061FE000 3 7F 89E20000 0C3FC000 2 7F 95C20000 187F8000 1 7F 77820000 30FF0000 0 7F FF 71020000 61FFFE00 7 FF 2E020000 43FBF800 5 FF B8080000 87F7F000 4 FF 200A0000 63EDE000 3 FF 40140000 1BD9C000 2 FF 80280000 37B38000 1 FF 744E0000 6F670000 0 FF 88 389A0000 32CE2000 6 88 7133DC00 659C4000 5 88 4665B800 23368000 4 88 8CCB7000 466D0000 3 88 5B94E000 8CDA0000 2 88 1B27C000 77B20000 1 88 364F8000 5F620000 0 88 6C9F0000 27610000 0 88 FF 6C9F0000 9D87FC00 6 FF 427C0000 5586FC00 6 FF 427C0000 1B0BF800 5 FF 84F80000
Decoder

---

## Page 160

### ISO/IEC 14492:2001 (E)
Common
Qe A C hex hex hex
EC D I MPS CE
193 1 17 1 4801 E004 000F94F0 194 0 17 1 4801 9803 000FDCF1 195 1 16 1 X 5101 9002 001FB9E2 196 1 17 1 4801 A202 003F73C4 197 1 18 1 3801 B402 007F778A 198 1 19 1 3401 F802 00FF5F16 199 1 19 1 3401 C401 00FF9317 200 1 19 1 3401 9000 00FFC718 201 0 20 1 3001 B7FE 01FFF632 202 1 19 1 3401 C004 0007D8C8 203 1 19 1 3401 8C03 00080CC9 204 1 20 1 3001 B004 00108194 205 1 20 1 3001 8003 0010B195 206 1 21 1 2801 A004 0021C32C 207 1 22 1 2401 F006 0043D65A 208 1 22 1 2401 CC05 0043FA5B 209 1 22 1 2401 A804 00441E5C 210 1 22 1 2401 8403 0044425D 211 1 23 1 2201 C004 0088CCBC 212 0 23 1 2201 9E03 0088EEBD 213 1 21 1 2801 8804 0223BAF4 214 1 22 1 2401 C006 0447C5EA 215 0 22 1 2401 9C05 0447E9EB 216 1 20 1 3001 9004 001FA7AC 217 1 21 1 2801 C006 003FAF5A 218 0 21 1 2801 9805 003FD75B 219 0 19 1 3401 A004 00FF5D6C 220 1 18 1 3801 D004 03FD75B0 221 0 18 1 3801 9803 03FDADB1 222 0 17 1 4801 E004 0006B6C4 223 0 16 1 X 5101 9002 000D6D88 224 0 15 1 5401 FC04 0036FA24 225 0 14 1 X 5601 A802 006DF448 226 1 14 0 X 5601 A402 00DC9492 227 0 14 1 X 5601 9C02 01B9D526 228 0 14 0 X 5601 8C02 0004564E 229 1 15 0 5401 AC02 0008AC9C 230 1 14 0 X 5601 A802 00115938 231 1 14 1 X 5601 A402 00235E72 232 1 15 1 5401 AC02 0046BCE4 233 0 16 1 5101 B002 008E21CA 234 1 15 1 X 5401 A202 011C4394 235 0 16 1 5101 A802 00008728 236 0 15 1 X 5401 A202 00010E50 237 0 14 1 X 5601 9C02 0002C4A2 238 1 14 0 X 5601 8C02 00063546 239 1 14 1 5601 D804 001A2D1C 240 0 14 1 X 5601 8203 001A831D 241 1 14 0 5601 B008 006B6478 242 0 14 1 5601 AC02 0006C8F0 243 1 14 0 5601 AC02 000D91E0 244 0 14 1 5601 AC02 001B23C0 245 0 14 0 5601 AC02 00364780 246 0 15 0 5401 AC02 006D3B02 247 1 16 0 5101 B002 00DB1E06 248 1 15 0 X 5401 A202 01B63C0C 249 1 14 0 X 5601 9C02 036D201A 250 0 14 1 X 5601 8C02 0002EC36 251 1 14 0 5601 D804 000D08DC 252 1 14 1 5601 AC02 001A11B8 253 1 15 1 5401 AC02 0034CF72 254 1 16 1 5101 B002 006A46E6 255 1 17 1 4801 BE02 00D52FCE 256 1 18 1 3801 EC02 01AAEF9E 257
### (concluded)
Software Conven-tions Decoder
B OUT C B IN C CT CT hex hex hex hex hex hex
7 7F 6C2FE000 3 FF 73D40000 7 7F 242EE000 3 FF 73D40000 6 7F 485DC000 2 FF 47A40000 5 7F 90BB8000 1 FF 11460000 4 7F 91750000 0 FF 37 228C0000 3 7F B2E8DC00 6 37 45192000 3 7F 7EE7DC00 6 37 45192000 3 7F 4AE6DC00 6 37 45192000 2 FF 7F 2DCBB800 5 37 8A324000 8 FF B72EE000 3 37 08D50000 8 FF 832DE000 3 37 08D50000 7 FF 9E59C000 2 37 11AA0000 7 FF 6E58C000 2 37 11AA0000 6 FF 7CAF8000 1 37 23540000 5 FF A95D0000 0 37 46A80000 5 FF 855C0000 0 37 46A80000 5 FF 615B0000 0 37 46A80000 5 FF 3D5A0000 0 37 47 46A80000 4 FF 32B28E00 7 47 8D517000 4 FF 10B18E00 7 47 8D517000 2 FF 42C63800 5 47 453DC000 1 FF 358A7000 4 47 8A7B8000 1 88 FF 11897000 4 47 8A7B8000 6 88 4625C000 2 47 49DE0000 5 88 2C498000 1 47 93BC0000 5 88 04488000 1 47 1A 93BC0000 3 88 11223400 7 1A 8EE1CA00 1 88 4488D000 5 1A 8B7B2800 1 FF 88 0C87D000 5 1A 8B7B2800 7 FF 321F4000 3 1A ADE4A000 6 FF 643E8000 2 1A 2BC34000 4 FF 4CF60000 0 1A DB AF0D0000 3 FF 99EDB600 7 DB 0E144800 2 FF 87D96C00 6 DB 1C289000 1 37 FF 63B0D800 5 DB 38512000 7 37 1B5FB000 4 DB 70A24000 6 37 36BF6000 3 DB 75428000 5 37 6D7EC000 2 DB 3A830000 4 37 2EFB8000 1 DB 75060000 3 37 5DF70000 0 DB 6A 4E0A0000 2 37 13ECD400 7 6A 9C152A00 1 47 37 27D9A800 6 6A 7A285400 8 47 4FB35000 5 6A 584EA800 7 47 9F66A000 4 6A 029B5000 6 47 96CB4000 3 6A 0536A000 5 47 81948000 2 6A 0A6D4000 3 47 AE4E0000 0 6A 29B50000 3 47 584D0000 0 6A DF 29B50000 1 1A 47 09337C00 6 DF A6D48000 8 1A 1266F800 5 DF 999B0000 7 1A 24CDF000 4 DF 87340000 6 1A 499BE000 3 DF 62660000 5 1A 9337C000 2 DF 18CA0000 4 1A 7A6D8000 1 DF 31940000 3 1A 4CD90000 0 DF FF 63280000 2 1A 99B3FE00 7 FF 084E0000 1 DB 1A 8B65FC00 6 FF 109C0000 8 DB 6AC9F800 5 FF 21380000 6 DB 5323E000 3 FF 84E00000 5 DB A647C000 2 FF 05BA0000 4 DB A08D8000 1 FF 0B740000 3 DB 99190000 0 FF 16E80000 2 DB 9031FE00 7 FF 2DD00000 1 DB 9061FC00 6 FF 5BA00000 DB 6A DF FF AC
Encoder Decoder
### Table H.1 ñ Encoder and decoder trace data

---

## Page 161

[1]
[2]
[3]
[4]
[5]
[6]
[7]
[8]
[9]
[10]
[11]
[12]
[13]
[14]
[15]
[16]
[17]
[18]
[19]
[20]
ISO/IEC 14492:2001 (E)
text, IEEE Transactions on Computers, C-23:1174-1179, November 1974.
BILGIN (A.), SEMENTILLI (P.), MARCELLIN (M.): Progressive image coding using trellis coded quantization, IEEE Transactions on Image Processing, November 1997. (submitted).
Proc. of Data Compression Conference, pages 397-406, Snowbird, Utah, USA, 1997.
IEEE Trans. Commun., 42:1881-1893, Feb.-April 1994.
Proc. of Data Compression Conference, pages 210-219, 1996.
HOWARD (P. G.): Text image compression using soft pattern matching, Computer Journal, 40:2-3, 1997.
emerging JBIG2 standard, IEEE Transactions on Circuits and Systems for Video Technology, 8(7):838-848, November 1998.
Proceedings of IEEE, Volume 68, pages 854-867, July 1980.
Proc. IEEE Data Compression Conference, 1994.
IEEE Southwest Symposium on Image Analysis and Interpretation, pages 141-144, San Antonio, TX, USA, April 1996.
MARTINS (B.), FORCHHAMMER (S.): Lossless/lossy compression of bi-level images, in Proc. of IS&T/SPIE Symp. on Electr. Im.: Science and Technology 1997, Volume 3018, pages 38-49, 1997.
Imaging, September 1998.
MARTINS (B.), FORCHHAMMER (S.): Tree Coding of Bi-level Images, IEEE Trans. Image Proc., 7(4):517-528, 1998.
IEEE Trans. Image Processing, 8(5):601-613, May 1999.
matching, in , pages 447-451, Bangalore, India, 1984.
symbol matching facsimile data compression system, in Proceedings of the IEEE, Volume 68, pages 786-796, July 1980.
hierarchical trees, IEEE Transactions on Circuits and Systems for Video Technology, 6:243-250, June 1996.
SAYOOD (K.): Introduction to Data Compression, Morgan Kaufmann Publishers, San Francisco, CA, 1996.
TOU (J. T.), GONZALEZ (R. C.): Pattern Recognition Principles, Addison-Wesley Publishing Company, Reading, MA, 1974.
WITTEN (I.H.), MOFFAT (A.), BELL (T.C.): Managing Gigabytes, Van Nostrand Reinhold, New York, 1994.
Bibliography
ASCHER (R. N.), NAGY (G.): A means for achieving a high degree of compaction on scan-digitized printed
CONSTANTINESCU (C.), ARPS (R.): Fast residue coding for lossless textual image compression, in
FORCHHAMMER (S.), JENSEN (K. S.): Data compression of scanned halftone images,
HOWARD (P. G.): Lossless and lossy compression of text images by soft pattern matching, in
HOWARD (P. G.), KOSSENTINI (F.), MARTINS (B.), FORCHHAMMER (S.), RUCKLIDGE (W.J.): The
HUNTER (R.), ROBINSON (A. H.): International digital facsimile coding standards, in
INGLIS (S.), WITTEN (I. H.): Compression-based template matching, in J. Storer and M. Cohn, editors,
KOSSENTINI (F.), WARD (R.): An analysis-compression technique for black and white documents, in
MARTINS (B.), FORCHHAMMER (S.): Halftone Coding with JBIG2, submitted to Journal of Electronic
MARTINS (B.), FORCHHAMMER (S.): Lossless, near-lossless, and refinement coding of bi-level images,
MOHIUDDIN (K.), RISSANEN (J. J.), ARPS (R.): Lossless binary image compression based on pattern Proceedings of International Conference on Computers, Systems, and Signal Processing
PRATT (W. K.), CAPITANT (P. J.), CHEN (W. H.), HAMILTON (E. R.), WALLS (R. H.): Combined
SAID (A.), PEARLMAN (W. A): A new fast and efficient image codec based on set partitioning in

---

## Page 164

Series A
Series B
Series C
Series D
Series E
Series F
Series G
Series H
Series I
Series J
Series K
Series L
Series M
Series N
Series O
Series P
Series Q
Series R
Series S
Series T
Series U
Series V
Series X
Series Y
Series Z
### ITU-T RECOMMENDATIONS SERIES
Organization of the work of the ITU-T
Means of expression: definitions, symbols, classification
General telecommunication statistics
General tariff principles
Non-telephone telecommunication services
Transmission systems and media, digital systems and networks
Audiovisual and multimedia systems
Integrated services digital network
Protection against interference
TMN and network maintenance: international transmission systems, telephone circuits, telegraphy, facsimile and leased circuits
Specifications of measuring equipment
Switching and signalling
Telegraph transmission
Telegraph services terminal equipment
Terminals for telematic services
Telegraph switching
Data communication over the telephone network
Data networks and open system communications
Global information infrastructure and Internet protocol aspects
Languages and general software aspects for telecommunication systems
### *18592*
Printed in Switzerland Geneva, 2002
Overall network operation, telephone service, service operation and human factors
Transmission of television, sound programme and other multimedia signals
Construction, installation and protection of cables and other elements of outside plant
Maintenance: international sound programme and television transmission circuits
Telephone transmission quality, telephone installations, local line networks