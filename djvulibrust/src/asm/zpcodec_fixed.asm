%line 1 "zpcodec_fixed.asm"
; DjVu ZPCodec encoder (x86-64, NASM)
; Field offsets (repr C on x86_64): byte=0 scount=1 delay=2 encoding=3 a=4 subend=8 buffer=12 nrun=16 bs(ptr)=24
; Tables: p@32, m@1056, up@2080, dn@2336

section .text
global zpcodec_einit

zpcodec_einit:
    ; rdi = pointer to ZPCodec struct (first argument in x64 ABI)
    
    ; Initialize a = 0
    mov     dword [rdi + 4], 0
    
    ; Initialize scount = 0  
    mov     byte [rdi + 1], 0
    
    ; Initialize byte = 0
    mov     byte [rdi + 0], 0
    
    ; Initialize delay = 25
    mov     byte [rdi + 2], 25
    
    ; Initialize subend = 0
    mov     dword [rdi + 8], 0
    
    ; Initialize buffer = 0xffffff
    mov     dword [rdi + 12], 0xffffff
    
    ; Initialize nrun = 0
    mov     dword [rdi + 16], 0
    
    ;
    ret

; Export symbol for linking
global _zpcodec_einit
_zpcodec_einit:
    jmp zpcodec_einit

; Renormalization bit emission (zemit)

section .text
global zpcodec_zemit
; zpcodec_outbit is defined later in this file; no extern needed here

zpcodec_zemit:
    push    rbp
    mov     rbp, rsp
    push    rbx              ; Save callee-saved registers
    push    r12
    push    r13
    sub     rsp, 8           ; maintain 16-byte alignment before calls
    
    ; rdi = ZPCodec* self
    ; esi = int b (bit to emit)
    
    mov     r12, rdi         ; Save self pointer
    mov     r13d, esi        ; Save input bit
    ; Debug: log state at zemit entry (event=2)
    mov     edi, 2           ; event id = 2 (zemit)
    mov     esi, [r12 + 4]   ; a
    mov     edx, [r12 + 8]   ; subend
    mov     ecx, [r12 + 12]  ; buffer
    mov     r8d, [r12 + 16]  ; nrun
    mov     r9d, r13d        ; bit parameter
    and     r9d, 1
    call    zp_debug_hook
    
    ; Shift new bit into 3-byte buffer
    ; buffer = (buffer << 1) + b
    mov     eax, [r12 + 12]  ; Load buffer
    shl     eax, 1           ; Shift left by 1
    add     eax, r13d        ; Add input bit
    mov     [r12 + 12], eax  ; Store back
    
    ; Extract bit going out of 3-byte buffer
    ; b = (buffer >> 24)
    mov     ebx, eax
    shr     ebx, 24          ; Get bit 24
    
    ; Mask buffer to 24 bits
    ; buffer = buffer & 0xffffff
    and     eax, 0xffffff
    mov     [r12 + 12], eax
    
    ; Switch on the extracted bit value
    cmp     ebx, 1
    je      .case_one
    cmp     ebx, 0xff
    je      .case_ff
    test    ebx, ebx
    jz      .case_zero
    
    ; Should never reach here - invalid state
    ; Fallback: treat as central renormalization
    inc     dword [r12 + 16]
    jmp     .exit
    
.case_one:
    ; Upper renormalization: emit 1, then nrun 0s
    mov     rdi, r12
    mov     esi, 1
    call    zpcodec_outbit   ; outbit(1)
    
    ; Load nrun counter
    mov     ecx, [r12 + 16]
    test    ecx, ecx
    jz      .done_ones
    
.emit_zeros_loop:
    mov     rdi, r12
    xor     esi, esi         ; esi = 0
    call    zpcodec_outbit   ; outbit(0)
    dec     ecx
    jnz     .emit_zeros_loop
    
.done_ones:
    mov     dword [r12 + 16], 0  ; nrun = 0
    jmp     .exit
    
.case_ff:
    ; Lower renormalization: emit 0, then nrun 1s
    mov     rdi, r12
    xor     esi, esi         ; esi = 0
    call    zpcodec_outbit   ; outbit(0)
    
    ; Load nrun counter
    mov     ecx, [r12 + 16]
    test    ecx, ecx
    jz      .done_ff
    
.emit_ones_loop:
    mov     rdi, r12
    mov     esi, 1
    call    zpcodec_outbit   ; outbit(1)
    dec     ecx
    jnz     .emit_ones_loop
    
.done_ff:
    mov     dword [r12 + 16], 0  ; nrun = 0
    jmp     .exit
    
.case_zero:
    ; Central renormalization: increment run counter
    inc     dword [r12 + 16]     ; nrun++
    
.exit:
    ; Function epilogue
    add     rsp, 8
    pop     r13
    pop     r12
    pop     rbx
    pop     rbp
    ret

; macOS symbol
global _zpcodec_zemit
_zpcodec_zemit:
    jmp zpcodec_zemit

; Output one bit to the bytestream (outbit)

section .text
global zpcodec_outbit
extern bytestream_write  ; External function to write to bytestream
extern zp_debug_hook     ; Debug hook: void zp_debug_hook(int event, uint32 a, uint32 subend, uint32 buffer, uint32 nrun, int bit)

zpcodec_outbit:
    push    rbp
    mov     rbp, rsp
    push    rbx
    push    r12
    sub     rsp, 16          ; Align stack to 16 bytes
    
    ; rdi = ZPCodec* self
    ; esi = int bit
    
    mov     r12, rdi         ; Save self pointer
    mov     ebx, esi         ; Save bit value
    ; Debug: log state before processing the bit (event=1)
    mov     edi, 1           ; event id = 1 (outbit)
    mov     esi, [r12 + 4]   ; a
    mov     edx, [r12 + 8]   ; subend
    mov     ecx, [r12 + 12]  ; buffer
    mov     r8d, [r12 + 16]  ; nrun
    mov     r9d, ebx         ; bit
    and     r9d, 1
    call    zp_debug_hook
    
    ; Check delay
    movzx   eax, byte [r12 + 2]  ; Load delay
    test    eax, eax
    jz      .emit_bit        ; If delay == 0, emit the bit
    
    ; delay > 0
    cmp     al, 0xff
    je      .exit            ; If delay == 0xff, suspend forever
    
    ; Decrement delay
    dec     al
    mov     byte [r12 + 2], al
    jmp     .exit
    
.emit_bit:
    ; Insert bit into byte buffer
    ; byte = (byte << 1) | bit
    movzx   eax, byte [r12 + 0]  ; Load byte
    shl     al, 1                 ; Shift left
    and     ebx, 1               ; Ensure bit is 0 or 1
    or      al, bl               ; OR in the bit
    mov     byte [r12 + 0], al   ; Store back
    
    ; Increment scount
    movzx   ecx, byte [r12 + 1]  ; Load scount
    inc     ecx
    mov     byte [r12 + 1], cl   ; Store back
    
    ; Check if we have a full byte (scount == 8)
    cmp     cl, 8
    jne     .exit
    
    ; Output the byte
    ; Check encoding flag
    movzx   edx, byte [r12 + 3]  ; Load encoding flag
    test    edx, edx
    jz      .error               ; If not encoding, error
    
    ; Write byte to bytestream
    ; Prepare for bytestream_write(bs, &byte, 1)
    mov     rdi, [r12 + 24]      ; Load bs pointer (offset 24)
    lea     rsi, [r12 + 0]       ; Address of byte field
    mov     rdx, 1               ; Write 1 byte
    call    bytestream_write
    
    ; Check return value (should be 1)
    cmp     rax, 1
    jne     .write_error
    
    ; Reset scount and byte
    mov     byte [r12 + 1], 0    ; scount = 0
    mov     byte [r12 + 0], 0    ; byte = 0
    
.exit:
    ; Function epilogue
    add     rsp, 16
    pop     r12
    pop     rbx
    pop     rbp
    ret
    
.error:
    ; Handle "not encoding" error
    ; For now, just return (could add error handling)
    jmp     .exit
    
.write_error:
    ; Handle write error
    ; For now, just return (could add error handling)
    jmp     .exit

; macOS symbol
global _zpcodec_outbit
_zpcodec_outbit:
    jmp zpcodec_outbit
; Note: The bytestream_write function signature assumes a C-style interface.

; Arithmetic encoder (MPS/LPS)

section .text
global zpcodec_encode_mps
global zpcodec_encode_lps
extern zpcodec_zemit

; ============================================================================
; encode_mps - Encode Most Probable Symbol
; ============================================================================
zpcodec_encode_mps:
    push    rbp
    mov     rbp, rsp
    push    rbx
    push    r12
    push    r13
    push    r14
    
    ; rdi = ZPCodec* self
    ; rsi = BitContext* ctx
    ; edx = unsigned int z
    
    mov     r12, rdi         ; Save self pointer
    mov     r13, rsi         ; Save ctx pointer
    mov     r14d, edx        ; Save z
    
    ; Interval reversion: if (z >= 0x8000) z = 0x4000 + (z >> 1)
    cmp     r14d, 0x8000
    jb      .no_revert_mps
    mov     eax, r14d
    shr     eax, 1
    add     eax, 0x4000
    mov     r14d, eax
.no_revert_mps:
    
    ; Load context value
    movzx   ebx, byte [r13]  ; Load *ctx
    
    ; Check adaptation: if (a >= m[ctx])
    mov     eax, [r12 + 4]   ; Load a
    lea     rcx, [r12 + 1056] ; Address of m table (offset 1056)
    mov     edx, [rcx + rbx*4] ; Load m[ctx]
    cmp     eax, edx
    jb      .no_adapt
    
    ; ctx = up[ctx]
    lea     rcx, [r12 + 2080] ; Address of up table (offset 2080)
    movzx   ebx, byte [rcx + rbx] ; Load up[ctx]
    mov     byte [r13], bl   ; Store back to *ctx
    
.no_adapt:
    ; Code MPS: a = z
    mov     [r12 + 4], r14d  ; a = z
    
    ; Export bits: while (a >= 0x8000)
.export_loop:
    mov     eax, [r12 + 4]   ; Load a
    cmp     eax, 0x8000
    jb      .done
    
    ; zemit(1 - (subend>>15))
    mov     ecx, [r12 + 8]   ; Load subend
    shr     ecx, 15          ; subend >> 15
    xor     esi, esi         ; ESI = (subend>>15)==0 ? 1 : 0
    test    ecx, ecx
    setz    sil
    mov     rdi, r12
    call    zpcodec_zemit
    
    ; subend = (subend << 1)
    mov     eax, [r12 + 8]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 8], eax
    
    ; a = (a << 1)
    mov     eax, [r12 + 4]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 4], eax
    
    jmp     .export_loop
    
.done:
    pop     r14
    pop     r13
    pop     r12
    pop     rbx
    pop     rbp
    ret

; ============================================================================
; encode_lps - Encode Least Probable Symbol
; ============================================================================
zpcodec_encode_lps:
    push    rbp
    mov     rbp, rsp
    push    rbx
    push    r12
    push    r13
    push    r14
    
    ; rdi = ZPCodec* self
    ; rsi = BitContext* ctx
    ; edx = unsigned int z
    
    mov     r12, rdi         ; Save self pointer
    mov     r13, rsi         ; Save ctx pointer
    mov     r14d, edx        ; Save z
    
    ; Interval reversion: if (z >= 0x8000) z = 0x4000 + (z >> 1)
    cmp     r14d, 0x8000
    jb      .no_revert_lps
    mov     eax, r14d
    shr     eax, 1
    add     eax, 0x4000
    mov     r14d, eax
.no_revert_lps:
    
    ; Load context and adapt: ctx = dn[ctx]
    movzx   ebx, byte [r13]  ; Load *ctx
    lea     rcx, [r12 + 2336] ; Address of dn table (offset 2336)
    movzx   ebx, byte [rcx + rbx] ; Load dn[ctx]
    mov     byte [r13], bl   ; Store back to *ctx
    
    ; Code LPS: z = 0x10000 - z
    mov     eax, 0x10000
    sub     eax, r14d        ; z = 0x10000 - z
    mov     r14d, eax
    
    ; subend += z
    mov     ecx, [r12 + 8]   ; Load subend
    add     ecx, r14d        ; subend + z
    mov     [r12 + 8], ecx   ; Store subend
    
    ; a += z
    mov     eax, [r12 + 4]   ; Load a
    add     eax, r14d        ; a + z
    mov     [r12 + 4], eax   ; Store a
    
    ; Export bits: while (a >= 0x8000)
.export_loop:
    mov     eax, [r12 + 4]   ; Load a
    cmp     eax, 0x8000
    jb      .done
    
    ; zemit(1 - (subend>>15))
    mov     ecx, [r12 + 8]   ; Load subend
    shr     ecx, 15          ; subend >> 15
    xor     esi, esi         ; ESI = (subend>>15)==0 ? 1 : 0
    test    ecx, ecx
    setz    sil
    mov     rdi, r12
    call    zpcodec_zemit
    
    ; subend = (subend << 1)
    mov     eax, [r12 + 8]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 8], eax
    
    ; a = (a << 1)
    mov     eax, [r12 + 4]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 4], eax
    
    jmp     .export_loop
    
.done:
    pop     r14
    pop     r13
    pop     r12
    pop     rbx
    pop     rbp
    ret

; macOS symbols
global _zpcodec_encode_mps
_zpcodec_encode_mps:
    jmp zpcodec_encode_mps

global _zpcodec_encode_lps
_zpcodec_encode_lps:
    jmp zpcodec_encode_lps

; Simple encoder (no adaptation)

section .text
global zpcodec_encode_mps_simple
global zpcodec_encode_lps_simple
extern zpcodec_zemit

; ============================================================================
; encode_mps_simple - Encode MPS without context adaptation
; ============================================================================
zpcodec_encode_mps_simple:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    
    ; rdi = ZPCodec* self
    ; esi = unsigned int z
    
    mov     r12, rdi         ; Save self pointer
    mov     r13d, esi        ; Save z
    
    ; Code MPS: a = z
    mov     [r12 + 4], r13d  ; a = z
    
    ; Check if export needed: if (a >= 0x8000)
    cmp     r13d, 0x8000
    jb      .done
    
    ; Export one bit since a >= 0x8000
    ; zemit(1 - (subend>>15))
    mov     ecx, [r12 + 8]   ; Load subend
    shr     ecx, 15          ; subend >> 15
    xor     esi, esi         ; ESI = (subend>>15)==0 ? 1 : 0
    test    ecx, ecx
    setz    sil
    mov     rdi, r12
    call    zpcodec_zemit
    
    ; subend = (subend << 1)
    mov     eax, [r12 + 8]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 8], eax
    
    ; a = (a << 1)
    mov     eax, [r12 + 4]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 4], eax
    
.done:
    pop     r13
    pop     r12
    pop     rbp
    ret

; ============================================================================
; encode_lps_simple - Encode LPS without context adaptation
; ============================================================================
zpcodec_encode_lps_simple:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    
    ; rdi = ZPCodec* self
    ; esi = unsigned int z
    
    mov     r12, rdi         ; Save self pointer
    mov     r13d, esi        ; Save z
    
    ; Code LPS: z = 0x10000 - z
    mov     eax, 0x10000
    sub     eax, r13d        ; z = 0x10000 - z
    mov     r13d, eax
    
    ; subend += z
    mov     ecx, [r12 + 8]   ; Load subend
    add     ecx, r13d        ; subend + z
    mov     [r12 + 8], ecx   ; Store subend
    
    ; a += z
    mov     eax, [r12 + 4]   ; Load a
    add     eax, r13d        ; a + z
    mov     [r12 + 4], eax   ; Store a
    
    ; Export bits: while (a >= 0x8000)
.export_loop:
    mov     eax, [r12 + 4]   ; Load a
    cmp     eax, 0x8000
    jb      .done
    
    ; zemit(1 - (subend>>15))
    mov     ecx, [r12 + 8]   ; Load subend
    shr     ecx, 15          ; subend >> 15
    xor     esi, esi         ; ESI = (subend>>15)==0 ? 1 : 0
    test    ecx, ecx
    setz    sil
    mov     rdi, r12
    call    zpcodec_zemit
    
    ; subend = (subend << 1)
    mov     eax, [r12 + 8]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 8], eax
    
    ; a = (a << 1)
    mov     eax, [r12 + 4]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 4], eax
    
    jmp     .export_loop
    
.done:
    pop     r13
    pop     r12
    pop     rbp
    ret

; macOS symbols
global _zpcodec_encode_mps_simple
_zpcodec_encode_mps_simple:
    jmp zpcodec_encode_mps_simple

global _zpcodec_encode_lps_simple
_zpcodec_encode_lps_simple:
    jmp zpcodec_encode_lps_simple

; Flush and terminate (eflush)

section .text
global zpcodec_eflush
extern zpcodec_zemit
extern zpcodec_outbit

zpcodec_eflush:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    
    mov     r12, rdi         ; Save self pointer
    
    ; Adjust subend
    mov     eax, [r12 + 8]   ; Load subend
    cmp     eax, 0x8000
    ja      .set_10000       ; if (subend > 0x8000)
    test    eax, eax
    jz      .subend_adjusted ; if (subend == 0)
    
    ; subend > 0 && subend <= 0x8000
    mov     dword [r12 + 8], 0x8000
    jmp     .subend_adjusted
    
.set_10000:
    mov     dword [r12 + 8], 0x10000
    
.subend_adjusted:
    ; Emit many MPS bits
    ; while (buffer != 0xffffff || subend)
.emit_loop:
    mov     eax, [r12 + 12]  ; Load buffer
    cmp     eax, 0xffffff
    jne     .do_emit
    
    mov     ecx, [r12 + 8]   ; Load subend
    test    ecx, ecx
    jz      .emit_done       ; Exit if buffer==0xffffff && subend==0
    
.do_emit:
    ; zemit(1 - (subend>>15))
    mov     ecx, [r12 + 8]   ; Load subend
    shr     ecx, 15          ; subend >> 15
    xor     esi, esi         ; ESI = (subend>>15)==0 ? 1 : 0
    test    ecx, ecx
    setz    sil
    mov     rdi, r12
    call    zpcodec_zemit
    
    ; subend = (subend << 1)
    mov     eax, [r12 + 8]
    shl     eax, 1
    and     eax, 0xffff      ; Keep as unsigned short
    mov     [r12 + 8], eax
    
    jmp     .emit_loop
    
.emit_done:
    ; Emit pending run
    ; outbit(1)
    mov     rdi, r12
    mov     esi, 1
    call    zpcodec_outbit
    
    ; while (nrun-- > 0) outbit(0)
    mov     r13d, [r12 + 16] ; Load nrun
    jmp     .check_run

.emit_run_loop:
    mov     rdi, r12
    xor     esi, esi         ; esi = 0
    call    zpcodec_outbit   ; outbit(0)
    dec     r13d
.check_run:
    test    r13d, r13d
    jg      .emit_run_loop
    
.run_done:
    ; nrun = 0
    mov     dword [r12 + 16], 0
    
    ; Emit 1s until full byte
    ; while (scount > 0) outbit(1)
.fill_byte_loop:
    movzx   eax, byte [r12 + 1]  ; Load scount
    test    eax, eax
    jz      .fill_done
    
    mov     rdi, r12
    mov     esi, 1
    call    zpcodec_outbit   ; outbit(1)
    jmp     .fill_byte_loop
    
.fill_done:
    ; Prevent further emission
    ; delay = 0xff
    mov     byte [r12 + 2], 0xff
    
    pop     r13
    pop     r12
    pop     rbp
    ret

; macOS symbol
global _zpcodec_eflush
_zpcodec_eflush:
    jmp zpcodec_eflush
