bits 16
section .start
global image_start
extern rust_main
extern __bss_start
extern __bss_end
global bios_read
global bios_ticks
global bios_key
global bios_chainload
global bios_boot_drive

image_start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7000
    cld
    mov ax, 3
    int 0x10
    in al, 0x92
    or al, 2
    out 0x92, al
    lgdt [dword gdt_ptr]
    mov eax, cr0
    or al, 1
    mov cr0, eax
    jmp dword 0x08:protected_start

bits 32
protected_start:
    mov eax, cr0
    and eax, 0xfffffff3
    or eax, 2
    mov cr0, eax
    mov eax, cr4
    or eax, 0x600
    mov cr4, eax
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov esp, 0x90000
    xor ebp, ebp
    mov edi, __bss_start
    mov ecx, __bss_end
    sub ecx, edi
    xor eax, eax
    rep stosb
    call rust_main
.halt:
    cli
    hlt
    jmp .halt

bios_boot_drive:
    movzx eax, byte [0x600]
    ret
bios_read:
    mov eax, [esp+4]
    mov [request_drive], al
    mov eax, [esp+8]
    mov [dap_lba], eax
    mov byte [request_op], 0
    jmp common_call
bios_ticks:
    mov byte [request_op], 1
    jmp common_call
bios_key:
    mov byte [request_op], 2
    jmp common_call
bios_chainload:
    mov eax, [esp+4]
    mov [request_drive], al
    mov eax, [esp+8]
    mov [dap_lba], eax
    mov byte [request_op], 3
common_call:
    pushfd
    pushad
    mov [saved_esp], esp
    jmp word 0x18:protected16 - image_start

bits 16
protected16:
    mov ax, 0x20
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov sp, 0x7000
    mov eax, cr0
    and al, 0xfe
    mov cr0, eax
    jmp 0x1000:real_entry - image_start
real_entry:
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov sp, 0x7000
    sti
    cld
    cmp byte [dword request_op], 1
    je .ticks
    cmp byte [dword request_op], 2
    je .key
    mov ax, 0x1000
    mov ds, ax
    mov word [dap + 2 - image_start], 1
    mov si, dap - image_start
    mov dl, [request_drive - image_start]
    mov ah, 0x42
    int 0x13
    jc .disk_error
    xor ax, ax
    mov ds, ax
    cmp byte [dword request_op], 3
    je .chain
    xor eax, eax
    jmp .return
.disk_error:
    movzx eax, ah
    test eax, eax
    jnz .return
    mov eax, 1
    jmp .return
.ticks:
    xor ah, ah
    int 0x1a
    movzx eax, cx
    shl eax, 16
    mov ax, dx
    jmp .return
.key:
    mov ah, 1
    int 0x16
    jz .no_key
    xor ah, ah
    int 0x16
    movzx eax, ax
    jmp .return
.no_key:
    xor eax, eax
    jmp .return
.chain:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov si, 0x8000
    mov di, 0x7c00
    mov cx, 256
    rep movsw
    mov dl, [dword request_drive]
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7c00
    mov si, 0x620
    cld
    sti
    jmp 0:0x7c00
.return:
    cli
    xor dx, dx
    mov ds, dx
    mov [dword result], eax
    lgdt [dword gdt_ptr]
    mov eax, cr0
    or al, 1
    mov cr0, eax
    jmp dword 0x08:protected_return

bits 32
protected_return:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov esp, [saved_esp]
    popad
    popfd
    mov eax, [result]
    ret

align 8
gdt:
    dq 0
    dq 0x00cf9a000000ffff
    dq 0x00cf92000000ffff
    dq 0x00009a010000ffff ; 16-bit code base 0x10000
    dq 0x000092000000ffff ; 16-bit data base 0
gdt_end:
gdt_ptr:
    dw gdt_end-gdt-1
    dd gdt
align 4
saved_esp: dd 0
result: dd 0
request_op: db 0
request_drive: db 0
align 4
dap:
    db 16, 0
    dw 1
    dw 0x8000, 0
dap_lba: dq 0
