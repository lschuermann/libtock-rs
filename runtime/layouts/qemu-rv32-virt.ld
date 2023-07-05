/* Layout for the RISC-V 32 boards, used by the examples in this repository. */

MEMORY {
  /* Note that the SRAM address may need to be changed depending on
   * the kernel binary, check for the actual address of APP_MEMORY!
   */
  FLASH (X) : ORIGIN = 0x80100000, LENGTH = 32M
  RAM   (W) : ORIGIN = 0x80300000, LENGTH = 0x100000
}

TBF_HEADER_SIZE = 0x60;
INCLUDE libtock_layout.ld
