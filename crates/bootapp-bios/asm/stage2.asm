; BootApp BIOS stage2 bootstrap and firmware thunk.
;
; Stage1 loads this image to linear 0x10000 and far-jumps to 1000:0000.
; Addressing rule (checked by scripts/check-bios-realmode.py): code that runs
; in real mode or 16-bit protected mode never forms an effective offset above
; 0xFFFF. Real-mode CS is IMAGE_SEG and real-mode DS is IMAGE_SEG whenever the
; thunk touches its own data, so every such operand is written IMG(label) =
; label - image_start (always < 0x10000 because the linker asserts that the
; .start section ends below 0x20000). Linear addresses >= 0x10000 are used only
; from 32-bit flat protected mode.
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
global bios_chainload_buffer
global bios_stack_top
global bios_boot_drive

IMAGE_SEG equ 0x1000            ; real-mode segment whose base is image_start
CPU_REQUIRED_EDX equ (1 << 24) | (1 << 25) | (1 << 26) ; FXSR, SSE, SSE2
STACK_CEILING equ 0x90000       ; highest protected-mode stack top
STACK_FLOOR equ 0x20000         ; below this a BDA/INT 12h value is ignored
%define IMG(label) ((label) - image_start)

image_start:                    ; real mode, CS=IMAGE_SEG, IP=0
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7000
    cld
    mov ax, 3
    int 0x10
    mov ax, IMAGE_SEG
    mov ds, ax                  ; DS=IMAGE_SEG for IMG(...) data below
    ; CPU baseline: the prebuilt i686 Rust core uses SSE2 and stage2 enables
    ; OSFXSR, so require CPUID with FXSR+SSE+SSE2 instead of faulting later.
    pushfd
    pop eax
    mov ecx, eax
    xor eax, 0x200000           ; EFLAGS.ID toggles only when CPUID exists
    push eax
    popfd
    pushfd
    pop eax
    push ecx
    popfd
    xor eax, ecx
    test eax, 0x200000
    jz .cpu_unsupported
    mov eax, 1
    cpuid
    and edx, CPU_REQUIRED_EDX
    cmp edx, CPU_REQUIRED_EDX
    jne .cpu_unsupported
    int 0x12                    ; AX = KiB of conventional memory
    movzx eax, ax
    shl eax, 10
    mov [IMG(low_memory_bytes)], eax
    in al, 0x92
    and al, 0xfe                ; never pulse the fast-reset bit
    or al, 2                    ; fast A20 gate
    out 0x92, al
    o32 lgdt [IMG(gdt_ptr)]     ; DS=IMAGE_SEG: offset < 0x10000
    mov eax, cr0
    or al, 1
    mov cr0, eax
    jmp dword 0x08:protected_start
.cpu_unsupported:               ; DS=IMAGE_SEG; screen + raw COM1, then halt
    mov si, IMG(cpu_error)
.cpu_print:
    lodsb
    test al, al
    jz .cpu_halt
    mov dx, 0x3f8
    out dx, al
    mov ah, 0x0e
    xor bx, bx
    int 0x10
    jmp .cpu_print
.cpu_halt:
    cli
    hlt
    jmp .cpu_halt

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
    ; Stack top = min(0x90000, EBDA base) aligned to 16; EBDA base is BDA
    ; word 0x40E << 4 when non-zero and >= 0x20000, else the INT 12h size.
    mov esp, STACK_CEILING
    movzx eax, word [0x40e]
    shl eax, 4
    cmp eax, STACK_FLOOR
    jae .have_limit
    mov eax, [low_memory_bytes]
    cmp eax, STACK_FLOOR
    jb .stack_ready             ; no usable value: keep the ceiling
.have_limit:
    and eax, 0xfffffff0
    cmp eax, esp
    jae .stack_ready
    mov esp, eax
.stack_ready:
    mov [bios_stack_top], esp
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

; 32-bit flat cdecl ABI used by Rust. Linear addresses are fine here.
bios_boot_drive:
    movzx eax, byte [0x600]
    ret
bios_read:
    mov eax, [esp+4]
    mov [request_drive], al
    mov eax, [esp+8]
    mov [dap_lba], eax
    mov dword [dap_lba + 4], 0
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
    mov dword [dap_lba + 4], 0
    mov byte [request_op], 3
    jmp common_call
; Chainload the sector Rust already placed (and may have patched) at 0x8000.
bios_chainload_buffer:
    mov eax, [esp+4]
    mov [request_drive], al
    mov byte [request_op], 4
common_call:
    pushfd
    pushad
    mov [saved_esp], esp
    jmp word 0x18:IMG(protected16)

bits 16
protected16:                    ; 16-bit protected mode, CS=0x18 base 0x10000
    mov ax, 0x20                ; 16-bit data, base 0, limit 0xFFFF
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov sp, 0x7000
    mov eax, cr0
    and al, 0xfe
    mov cr0, eax
    jmp IMAGE_SEG:IMG(real_entry)
real_entry:                     ; real mode, CS=IMAGE_SEG
    mov ax, IMAGE_SEG
    mov ds, ax                  ; thunk data window
    xor ax, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov sp, 0x7000
    sti
    cld
    cmp byte [IMG(request_op)], 1
    je .ticks
    cmp byte [IMG(request_op)], 2
    je .key
    cmp byte [IMG(request_op)], 4
    je .chain                   ; 0x8000 already holds the boot sector
    mov word [IMG(dap) + 2], 1  ; firmware may rewrite the count on error
    mov si, IMG(dap)            ; DS:SI -> DAP, buffer 0000:8000
    mov dl, [IMG(request_drive)]
    mov ah, 0x42
    int 0x13
    jc .disk_error
    cmp byte [cs:IMG(request_op)], 3
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
    mov dl, [cs:IMG(request_drive)] ; read before leaving the image window
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov si, 0x8000
    mov di, 0x7c00
    mov cx, 256
    cld
    rep movsw                   ; 0000:8000 -> 0000:7C00, 512 bytes
    mov ss, ax
    mov sp, 0x7c00
    mov si, 0x620               ; DS:SI -> partition entry copied by Rust
    sti
    jmp 0:0x7c00
.return:
    cli
    mov dx, IMAGE_SEG
    mov ds, dx
    mov [IMG(result)], eax
    o32 lgdt [IMG(gdt_ptr)]
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
    dq 0x00009a010000ffff ; 16-bit code base 0x10000 limit 0xFFFF
    dq 0x000092000000ffff ; 16-bit data base 0 limit 0xFFFF (real-mode cache)
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
low_memory_bytes: dd 0          ; INT 12h result in bytes (real mode)
bios_stack_top: dd 0            ; chosen protected-mode stack top
cpu_error: db 'BootApp 0.3: CPU lacks FXSR/SSE/SSE2 (BOOTAPP_BIOS_CPU_UNSUPPORTED)', 13, 10, 0
align 4
dap:
    db 16, 0
    dw 1
    dw 0x8000, 0
dap_lba: dq 0
