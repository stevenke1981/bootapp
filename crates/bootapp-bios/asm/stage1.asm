bits 16
org 0x7c00
start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7c00
    sti
    cld
    mov [0x600], dl
    mov si, dap
    mov ah, 0x42
    int 0x13
    jc failed
    jmp 0x1000:0
failed:
    mov si, error
.loop:
    lodsb
    test al, al
    jz .halt
    mov ah, 0x0e
    int 0x10
    jmp .loop
.halt:
    cli
    hlt
    jmp .halt
align 4
dap:
    db 16, 0
    dw 120
    dw 0, 0x1000
    dq 1
error: db 'BootApp: BIOS EDD load failed', 0
times 440-($-$$) db 0
dd 0x42504132
dw 0
times 510-($-$$) db 0
dw 0xaa55
