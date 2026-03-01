.3.2 Directory Chunk: DIRM
As described in Multipage Documents, a multipage document will contain “component
files” such as individual pages (FORM:DJVU) or shared annotations (FORM:DJVI).
The first contained chunk in a FORM:DJVM composite chunk is the DIRM chunk
containing the document directory. It contains information the decoder will need to
access the component files (see Multipage Documents).
8.3.2.1 Unencoded data
The first part of the “DIRM” chunk consists is unencoded:
Byte
Flags/Version
b7b6…b0
b7 (MSB) is the bundled flag. 1 for bundled, 0 for indirect
b6…b0 is the version. Currently 1.
INT16
nFiles
Number of component files
INT32
Offset0,
Offset1,
Offset2..
When the document is a bundled document (i.e. the flag
bundled is set), the header above is followed by the offsets
of each of the component files within the “FORM:DJVM”.
These offsets allow for random component file access.
These may be omitted for indirect documents.
When the document is indirect, these offsets are omitted.
8.3.2.2 BZZ encoded data
The rest of the chunk is entirely compressed with the BZZ general purpose compressor.
We describe now the data fed into (or retrieved from) the BZZ codec (see
BSByteStream.cpp and appendix 4)
INT24
Size0,
size1,
size2, …
Size of each component file. May be 0 for indirect documents.
BYTE
Flag0,
flag1,
flag2
Flag byte for each component file
0b<hasname><hastitle>000000 for a file included by other files.
0b<hasname><hastitle>000001 for a file representing a page.
0b<hasname><hastitle>000010 for a file containing thumbnails.
Flag hasname is set when the name of the file is different from
the file ID. Flag hastitle is set when the title of the file is different
from the file ID. These flags are used to avoid encoding the same
string three times.
Note: In practice, the hasname and hastitle bits are poorly tested
and not used.
Release Copy
Page 13 of 71
ZSTR
ID0,
Name0,
Title0,
ID1,
Name1,
Title1, …
There are one to three zero-terminated strings per component file.
The first string contains the ID of the component file. If hasname
is set then there is a second string which contains the name of the
component file (in the case of an indirect file, this is the disk
filename). If hastitle is set, then there is a third string which
contains the name of the component (for display … for example
alternate page numberings in the Forward, or Preface).
Note: ID0 in practice, ID0 is the only string used and in the case
of indirect files, is the same as the disk filename of the component
file.
Examples
3 Page bundled file with a shared dictionary
RAW:
81 3 54 e02 1cf52
(BZZ Decoded:)
dad 1c150 1ec5 0 1 1
64 69 63 74 30 30 30 32
dict0002.iff
2e 69 66 66 0
70 30 30 30 31 2e 64 6a 76 75 0
p0001.djvu
70 30 30 30 32 2e 64 6a 76 75 0
p0002.djvu
Flags/Version: bundled, version 1
nFiles: 3
Offsets: 0x54, 0xE02, 0x1CF52
Sizes: 0xDAD, 0x1C150, 0x1EC5
Flags: 0, 1, 1
ZStr: 3 null terminated filenames as shown.
3 Page indirect file with a shared dictionary
RAW:
1 3
(BZZ Decoded:)
dad 1c150 1ec5 0 1 1
64 69 63 74 30 30 30 32
dict0002.iff
2e 69 66 66 0
70 30 30 30 31 2e 64 6a 76 75 0
p0001.djvu
70 30 30 30 32 2e 64 6a 76 75 0
p0002.djvu
Flags/Version: indirect, version 1
nFiles: 3
Offsets: omitted for indirect files
Sizes: 0xDAD, 0x1C150, 0x1EC5
Flags: 0, 1, 1
ZStr: 3 null terminated filenames as shown.