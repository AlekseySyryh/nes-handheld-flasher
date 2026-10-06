/* RAM-only image: loaded by the BOOTSEL UF2 bootloader straight into SRAM and executed there,
   so the flash chip is left untouched. The vector table must sit at the very start of the image,
   which is why the "FLASH" region (code, rodata, .data load image) is placed at the SRAM base. */
MEMORY {
    FLASH : ORIGIN = 0x20000000, LENGTH = 128K
    RAM   : ORIGIN = 0x20020000, LENGTH = 128K
}
