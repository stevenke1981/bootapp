; Acceptance fixture only. This is not an operating system.
bits 16
org 0x7c00
    mov bl, dl
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
    mov si, message
    call print
    mov al, bl
    shr al, 4
    call digit
    mov al, bl
    and al, 15
    call digit
    mov si, suffix
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
message: db 'BOOTAPP_BIOS_FIXTURE_OK DL=', 0
suffix: db ' REALMODE=1 SEGMENTS=0', 13, 10, 0
error: db 'BOOTAPP_BIOS_FIXTURE_BAD_ABI', 13, 10, 0
times 510-($-$$) db 0
dw 0xaa55
