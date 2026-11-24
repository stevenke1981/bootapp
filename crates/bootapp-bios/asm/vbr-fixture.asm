; Acceptance fixture only: a partition boot sector (VBR), not an OS.
; Reports the chainload ABI it was entered with on COM1, then halts:
;   BOOTAPP_BIOS_VBR_OK DL=xx SI=xxxx P=<32 hex digits of the 16 bytes at DS:SI>
; Fails with BOOTAPP_BIOS_VBR_BAD_ABI if PE is set or DS/ES/SS are not 0.
; COM1 was initialised by the BootApp manager before chainloading.
bits 16
org 0x7c00
start:
    ; Entered via JMP 0000:7C00, so CS=0 and CS-relative stores are valid.
    mov [cs:entry_dl], dl
    mov [cs:entry_si], si
    mov eax, cr0
    test al, 1
    jnz failed
    mov ax, ds
    test ax, ax
    jnz failed
    mov ax, es
    test ax, ax
    jnz failed
    mov ax, ss
    test ax, ax
    jnz failed
    cld
    mov di, entry_part          ; copy DS:SI (16 bytes) before SI is reused
    mov cx, 16
    rep movsb
    mov si, message
    call print
    mov al, [entry_dl]
    call hex_byte
    mov si, si_text
    call print
    mov al, [entry_si + 1]
    call hex_byte
    mov al, [entry_si]
    call hex_byte
    mov si, part_text
    call print
    mov bx, entry_part
    mov cx, 16
.part:
    mov al, [bx]
    call hex_byte
    inc bx
    loop .part
    mov si, crlf
    call print
halt:
    cli
    hlt
    jmp halt
failed:
    xor ax, ax
    mov ds, ax
    mov si, error
    call print
    jmp halt
hex_byte:
    push ax
    shr al, 4
    call digit
    pop ax
    and al, 15
digit:
    add al, '0'
    cmp al, '9'
    jbe output
    add al, 7
output:
    mov dx, 0x3f8
    out dx, al
    ret
print:
    lodsb
    test al, al
    jz .done
    call output
    jmp print
.done:
    ret
message: db 'BOOTAPP_BIOS_VBR_OK DL=', 0
si_text: db ' SI=', 0
part_text: db ' P=', 0
crlf: db 13, 10, 0
error: db 'BOOTAPP_BIOS_VBR_BAD_ABI', 13, 10, 0
entry_dl: db 0
entry_si: dw 0
entry_part: times 16 db 0
times 510-($-$$) db 0
dw 0xaa55
