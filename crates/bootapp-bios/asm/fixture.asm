; Acceptance fixture only. This is not an operating system.
; Assemble with -DSAVED_MBR for the 'saved original MBR' (LBA 121) variant,
; which also prints ' P=<hex of partition entry 1 at 0x7DBE>'. Code and data
; stay below offset 440 so a partition table can be placed at 440..510.
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
%ifdef SAVED_MBR
    ; Partition entry 1 of the table this MBR was entered with (0x7DBE).
    mov si, table_text
    call print
    mov bx, 0x7c00 + 446
    mov cx, 16
.table:
    mov al, [bx]
    shr al, 4
    call digit
    mov al, [bx]
    and al, 15
    call digit
    inc bx
    loop .table
%endif
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
%ifdef SAVED_MBR
message: db 'BOOTAPP_BIOS_SAVED_MBR_OK DL=', 0
%else
message: db 'BOOTAPP_BIOS_FIXTURE_OK DL=', 0
%endif
suffix: db ' REALMODE=1 SEGMENTS=0', 0
table_text: db ' P=', 0
crlf: db 13, 10, 0
error: db 'BOOTAPP_BIOS_FIXTURE_BAD_ABI', 13, 10, 0
times 510-($-$$) db 0
dw 0xaa55
